use crate::classify::walk_and_classify_sync;
use crate::cleanup::{compute_sha256, resume_pending_cleanups};
use crate::ssh::server_cmd;
use crate::transfer::{run_passthrough_path, run_postprocess_path};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use purgery_core::{
    resolve_executable, ClientConfig, ClientPostprocessConfig, ClientRunPhase, ClientRunState,
    Manifest, ManifestEntry, RunConfig, RunConfigSync, RunId, RunStatus, SyncExecutionClass,
};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

/// Build a RunConfig from a ClientConfig, optionally filtering to include
/// only purgatory sync groups and rules applicable to those groups.
pub(crate) fn build_run_config(config: &ClientConfig, purgatory_only: bool) -> RunConfig {
    let sync: Vec<RunConfigSync> = config
        .sync
        .iter()
        .filter(|s| {
            if purgatory_only {
                let applicable =
                    purgery_core::applicable_rules(&config.postprocess.rules, s.name.as_str());
                !applicable.is_empty()
            } else {
                true
            }
        })
        .map(|s| RunConfigSync {
            name: s.name.clone(),
            to_path: s.to_path.clone(),
            delete_after_import: s.delete_after_import,
        })
        .collect();

    let purgatory_names: Vec<&str> = sync.iter().map(|s| s.name.as_str()).collect();
    let rules = if purgatory_only {
        config
            .postprocess
            .rules
            .iter()
            .filter(|r| purgatory_names.iter().any(|name| r.applies_to(name)))
            .cloned()
            .collect()
    } else {
        config.postprocess.rules.clone()
    };

    RunConfig {
        nickname: config.nickname.clone(),
        sync,
        postprocess: ClientPostprocessConfig { rules },
    }
}

/// Client boot-time check: verify local executables and config only.
/// Does NOT SSH into the server or mutate anything.
pub(crate) fn client_check(config: &ClientConfig, _config_path: &str) -> Result<()> {
    info!("checking client configuration");

    resolve_executable("ssh").map(|r| {
        info!(path = %r.path.as_str(), "ssh: found");
    })?;

    resolve_executable("rsync").map(|r| {
        info!(path = %r.path.as_str(), "rsync: found");
    })?;

    if config.server.host.as_str().is_empty() {
        anyhow::bail!("server host is empty");
    }
    if config.server.command.is_empty() {
        anyhow::bail!("server command is empty");
    }

    info!("client configuration: OK");
    Ok(())
}

pub(crate) fn sync_and_cleanup(config: &ClientConfig) -> Result<()> {
    let host = config.server.host.as_str();
    let server_command = &config.server.command;

    // 0. Resume pending cleanups before any new operations
    resume_pending_cleanups(config)?;
    resume_pending_postprocess_runs(config)?;

    // 0a. Run local checks before any remote operations
    client_check(config, "")?;

    // 1. Validate postprocess config before any walking
    let sync_names: Vec<purgery_core::SyncName> =
        config.sync.iter().map(|s| s.name.clone()).collect();
    if let Err(e) = config.postprocess.validate(&sync_names) {
        anyhow::bail!("postprocess config validation failed: {e}");
    }
    if let Err(e) = config
        .postprocess
        .validate_delete_after_import(&config.sync)
    {
        anyhow::bail!("postprocess config validation failed: {e}");
    }
    info!("postprocess rules validated");

    // 2. Classify sync groups by their execution class.
    let run_id = RunId::generate();
    let mut all_manifest_entries: Vec<purgery_core::ManifestEntry> = Vec::new();
    let mut passthrough_nodelete: Vec<&purgery_core::SyncMapping> = Vec::new();
    let mut passthrough_cleanup: Vec<&purgery_core::SyncMapping> = Vec::new();
    let mut purgatory_syncs: Vec<&purgery_core::SyncMapping> = Vec::new();

    let classes = purgery_core::classify_sync_groups(&config.sync, &config.postprocess.rules)
        .map_err(|e| anyhow::anyhow!("sync group classification failed: {e}"))?;
    for (class, sync) in &classes {
        match class {
            SyncExecutionClass::PassthroughNoDelete => {
                info!(
                    sync = sync.name.as_str(),
                    "PassthroughNoDelete: direct rsync only"
                );
                passthrough_nodelete.push(sync);
            }
            SyncExecutionClass::PassthroughDeleteAfterImport => {
                info!(
                    sync = sync.name.as_str(),
                    "PassthroughDeleteAfterImport: direct rsync with cleanup"
                );
                passthrough_cleanup.push(sync);
            }
            SyncExecutionClass::Purgatory => {
                let applicable =
                    purgery_core::applicable_rules(&config.postprocess.rules, sync.name.as_str());
                let (entries, _) = walk_and_classify_sync(config, sync, &run_id, &applicable)?;
                all_manifest_entries.extend(entries);
                purgatory_syncs.push(sync);
            }
        }
    }

    if all_manifest_entries.is_empty()
        && passthrough_nodelete.is_empty()
        && passthrough_cleanup.is_empty()
    {
        anyhow::bail!("no filesystem entries found to sync");
    }

    let manifest = purgery_core::Manifest {
        run_id: run_id.clone(),
        nickname: config.nickname.clone(),
        entries: all_manifest_entries,
    };
    let transfer_plan = manifest.to_transfer_plan();
    info!(walked = manifest.entries.len(), "manifest built");

    // 3. Execute transfers based on whether any purgatory groups exist
    if !purgatory_syncs.is_empty() {
        run_postprocess_path(
            config,
            host,
            server_command,
            &manifest,
            &transfer_plan,
            &run_id,
            &passthrough_nodelete,
            &passthrough_cleanup,
        )
    } else {
        run_passthrough_path(
            config,
            host,
            server_command,
            &passthrough_nodelete,
            &passthrough_cleanup,
        )
    }
}

/// Persist client run state, propagating errors.
/// Safety-state writes are not best-effort — if deletion could follow,
/// failure must stop the invocation.
fn persist_client_run_state_or_stop(
    state_dir: &str,
    nickname: &str,
    run_id: &str,
    manifest: &Manifest,
    run_config: &RunConfig,
    phase: ClientRunPhase,
) -> Result<()> {
    write_client_run_state(state_dir, nickname, run_id, manifest, run_config, phase).with_context(
        || format!("failed to persist client run state phase {phase:?}; refusing to continue"),
    )
}

/// Attempt to write an Abandoned tombstone and return an error.
/// If the tombstone write fails, the returned error still mentions the
/// original condition. No deletion occurs regardless.
fn persist_abandoned_or_error(
    state_dir: &str,
    nickname: &str,
    run_id: &str,
    manifest: &Manifest,
    run_config: &RunConfig,
    original_condition: &str,
) -> anyhow::Error {
    if let Err(ts_err) = write_client_run_state(
        state_dir,
        nickname,
        run_id,
        manifest,
        run_config,
        ClientRunPhase::Abandoned,
    ) {
        anyhow::anyhow!(
            "{original_condition}; additionally, failed to persist abandoned tombstone: {ts_err:#}; \
             no deletion authorised. Manual intervention required at: {state_dir}"
        )
    } else {
        anyhow::anyhow!(
            "{original_condition}; marked as abandoned without deletion. \
             Manual intervention required at: {state_dir}"
        )
    }
}

/// Attempt to write a Corrupt tombstone and return an error.
/// If the tombstone write fails, the returned error still mentions the
/// original condition. No deletion occurs regardless.
fn persist_corrupt_or_error(
    state_dir: &str,
    nickname: &str,
    run_id: &str,
    manifest: &Manifest,
    run_config: &RunConfig,
    original_error: &str,
) -> anyhow::Error {
    if let Err(ts_err) = write_client_run_state(
        state_dir,
        nickname,
        run_id,
        manifest,
        run_config,
        ClientRunPhase::Corrupt,
    ) {
        anyhow::anyhow!(
            "{original_error}; additionally, failed to persist corrupt tombstone: {ts_err:#}; \
             no deletion authorised. Manual intervention required at: {state_dir}"
        )
    } else {
        anyhow::anyhow!(
            "{original_error}; marked as corrupt without deletion. \
             Manual intervention required at: {state_dir}"
        )
    }
}

/// Persist local run state for a postprocess run so waiting/cleanup can
/// resume after a client crash.
pub(crate) fn write_client_run_state(
    state_dir: &str,
    nickname: &str,
    run_id: &str,
    manifest: &Manifest,
    run_config: &RunConfig,
    phase: ClientRunPhase,
) -> Result<()> {
    let run_state = ClientRunState {
        protocol_version: 1,
        nickname: nickname.to_owned(),
        run_id: run_id.to_owned(),
        manifest: manifest.to_toml()?,
        run_config: run_config.to_toml()?,
        phase,
    };
    let dir = Utf8PathBuf::from(state_dir)
        .join("runs")
        .join(format!("{nickname}-{run_id}"));
    fs::create_dir_all(dir.as_std_path())
        .with_context(|| format!("failed to create run state dir: {dir}"))?;
    let path = dir.join("state.toml");
    let tmp = dir.join("state.toml.tmp");
    let content = toml::to_string(&run_state)
        .map_err(|e| anyhow::anyhow!("failed to serialize run state: {e}"))?;
    fs::write(&tmp, &content).with_context(|| format!("failed to write run state: {tmp}"))?;
    fs::rename(&tmp, &path).with_context(|| format!("failed to publish run state: {path}"))?;
    Ok(())
}

/// Remove persisted client run state after cleanup is complete.
pub(crate) fn remove_client_run_state(state_dir: &str, nickname: &str, run_id: &str) {
    let dir = Utf8PathBuf::from(state_dir)
        .join("runs")
        .join(format!("{nickname}-{run_id}"));
    let _ = fs::remove_dir_all(dir.as_std_path());
}

/// Wait for the server run to reach a terminal phase.
///
/// Returns the terminal RunStateResponse on success.
/// On `not_found`: writes abandoned tombstone, returns error.
/// On `corrupt`: writes corrupt tombstone, returns error.
/// On transport/server command failure: returns error with state preserved.
/// On malformed response: returns error with state preserved.
/// On any other non-terminal, non-waitable phase: returns error.
pub(crate) fn wait_for_terminal_run_state(
    config: &ClientConfig,
    host: &str,
    server_command: &str,
    manifest: &Manifest,
    nickname: &purgery_core::Nickname,
    run_id: &RunId,
) -> Result<purgery_core::RunStateResponse> {
    let poll_interval = Duration::from_secs(5);
    let mut last_phase = String::new();
    let mut attempts_since_report = 0u64;

    write_client_run_state(
        &config.state_dir,
        nickname.as_str(),
        run_id.as_str(),
        manifest,
        &build_run_config(config, true),
        ClientRunPhase::WaitingForTerminalState,
    )?;

    loop {
        let response: Result<purgery_core::RunStateResponse> = (|| {
            let output = server_cmd(
                host,
                server_command,
                &[
                    "run-state",
                    "--nickname",
                    nickname.as_str(),
                    "--run-id",
                    run_id.as_str(),
                ],
            )?;
            toml::from_str(&output).with_context(|| "failed to parse run-state response")
        })();

        match response {
            Ok(state) => {
                if state.terminal {
                    info!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        phase = %state.phase,
                        "run reached terminal phase"
                    );
                    return Ok(state);
                }

                match state.phase.as_str() {
                    "ready" | "processing" => {
                        if state.phase != last_phase {
                            info!(
                                nickname = %nickname.as_str(),
                                run_id = %run_id.as_str(),
                                phase = %state.phase,
                                message = %state.message,
                                "run phase changed"
                            );
                            last_phase = state.phase.clone();
                            attempts_since_report = 0;
                        }
                        attempts_since_report += 1;
                        if attempts_since_report.is_multiple_of(12u64) {
                            info!(
                                nickname = %nickname.as_str(),
                                run_id = %run_id.as_str(),
                                phase = %last_phase,
                                "still waiting for server to process run"
                            );
                        }
                    }
                    "incoming" => {
                        // incoming is not a normal wait phase after finish-run.
                        // It should only appear during UploadCompleteFinishPending resume.
                        anyhow::bail!(
                            "run {}/{} is still 'incoming' but client state is \
                             'waiting_for_terminal_state'; protocol inconsistency. \
                             Local state preserved, no deletion.",
                            nickname.as_str(),
                            run_id.as_str()
                        );
                    }
                    "not_found" => {
                        warn!(
                            nickname = %nickname.as_str(),
                            run_id = %run_id.as_str(),
                            "run not found on server"
                        );
                        return Err(persist_abandoned_or_error(
                            &config.state_dir,
                            nickname.as_str(),
                            run_id.as_str(),
                            manifest,
                            &build_run_config(config, true),
                            &format!(
                                "run {}/{} not found on server",
                                nickname.as_str(),
                                run_id.as_str()
                            ),
                        ));
                    }
                    "corrupt" => {
                        warn!(
                            nickname = %nickname.as_str(),
                            run_id = %run_id.as_str(),
                            "run state is corrupt"
                        );
                        return Err(persist_corrupt_or_error(
                            &config.state_dir,
                            nickname.as_str(),
                            run_id.as_str(),
                            manifest,
                            &build_run_config(config, true),
                            &format!(
                                "run {}/{} server state is corrupt",
                                nickname.as_str(),
                                run_id.as_str()
                            ),
                        ));
                    }
                    other => {
                        anyhow::bail!(
                            "unexpected run-state phase '{other}' for run {}/{}; \
                             aborting with state preserved",
                            nickname.as_str(),
                            run_id.as_str()
                        );
                    }
                }
            }
            Err(e) => {
                anyhow::bail!(
                    "run-state query failed for run {}/{}: {e:#}; \
                     aborting with state preserved. Re-run to resume.",
                    nickname.as_str(),
                    run_id.as_str()
                );
            }
        }

        std::thread::sleep(poll_interval);
    }
}

/// Read and verify terminal status from the server.
///
/// On transport failure: returns error with `TerminalStatusSeen` preserved.
/// On malformed status: writes corrupt tombstone, returns error.
/// On envelope mismatch: writes corrupt tombstone, returns error.
pub(crate) fn read_and_verify_terminal_status(
    config: &ClientConfig,
    host: &str,
    server_command: &str,
    manifest: &Manifest,
    nickname: &purgery_core::Nickname,
    run_id: &RunId,
) -> Result<RunStatus> {
    persist_client_run_state_or_stop(
        &config.state_dir,
        nickname.as_str(),
        run_id.as_str(),
        manifest,
        &build_run_config(config, true),
        ClientRunPhase::TerminalStatusSeen,
    )?;

    let output = server_cmd(
        host,
        server_command,
        &[
            "status",
            "--nickname",
            nickname.as_str(),
            "--run-id",
            run_id.as_str(),
        ],
    )
    .with_context(|| {
        format!(
            "terminal status read failed for run {}/{}; TerminalStatusSeen preserved",
            nickname.as_str(),
            run_id.as_str()
        )
    })?;

    let trimmed = output.trim();
    let status = match RunStatus::from_toml(trimmed) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!(
                "malformed terminal status for run {}/{}: {e}",
                nickname.as_str(),
                run_id.as_str()
            );
            warn!("{msg}");
            return Err(persist_corrupt_or_error(
                &config.state_dir,
                nickname.as_str(),
                run_id.as_str(),
                manifest,
                &build_run_config(config, true),
                &msg,
            ));
        }
    };

    if status.nickname != *nickname {
        let msg = format!(
            "status nickname '{}' does not match manifest nickname '{}'; aborting deletion",
            status.nickname.as_str(),
            nickname.as_str()
        );
        warn!("{msg}");
        return Err(persist_corrupt_or_error(
            &config.state_dir,
            nickname.as_str(),
            run_id.as_str(),
            manifest,
            &build_run_config(config, true),
            &msg,
        ));
    }
    if status.run_id != *run_id {
        let msg = format!(
            "status run_id '{}' does not match manifest run_id '{}'; aborting deletion",
            status.run_id.as_str(),
            run_id.as_str()
        );
        warn!("{msg}");
        return Err(persist_corrupt_or_error(
            &config.state_dir,
            nickname.as_str(),
            run_id.as_str(),
            manifest,
            &build_run_config(config, true),
            &msg,
        ));
    }

    Ok(status)
}

/// Clean up confirmed postprocess entries from a verified terminal status.
pub(crate) fn cleanup_from_verified_status(
    config: &ClientConfig,
    manifest: &Manifest,
    status: &RunStatus,
    nickname: &purgery_core::Nickname,
    run_id: &RunId,
) -> Result<()> {
    let deletion_count = delete_confirmed_files(config, manifest, status)?;
    info!(deleted = deletion_count, "cleanup complete");

    // Persist CleanupComplete durably. If this fails, leave old state in
    // place so recovery can distinguish complete from interrupted cleanup.
    write_client_run_state(
        &config.state_dir,
        nickname.as_str(),
        run_id.as_str(),
        manifest,
        &build_run_config(config, true),
        ClientRunPhase::CleanupComplete,
    )?;
    remove_client_run_state(&config.state_dir, nickname.as_str(), run_id.as_str());

    info!(state = %status.state.as_str(), "run finished");
    Ok(())
}

/// Wait indefinitely for a postprocess run, then clean up.
pub(crate) fn wait_for_postprocess_run_and_cleanup(
    config: &ClientConfig,
    host: &str,
    server_command: &str,
    manifest: &Manifest,
    nickname: &purgery_core::Nickname,
    run_id: &RunId,
) -> Result<()> {
    wait_for_terminal_run_state(config, host, server_command, manifest, nickname, run_id)?;
    let status =
        read_and_verify_terminal_status(config, host, server_command, manifest, nickname, run_id)?;
    cleanup_from_verified_status(config, manifest, &status, nickname, run_id)?;
    Ok(())
}

/// Resume pending postprocess runs from local state.
pub(crate) fn resume_pending_postprocess_runs(config: &ClientConfig) -> Result<()> {
    let runs_dir = Utf8PathBuf::from(&config.state_dir).join("runs");
    if !runs_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(runs_dir.as_std_path())
        .with_context(|| format!("failed to read runs dir: {runs_dir}"))?
    {
        let entry = entry?;
        let dir_path = entry.path();
        if !dir_path.is_dir() {
            continue;
        }
        let state_path = dir_path.join("state.toml");
        if !state_path.exists() {
            continue;
        }
        let content = fs::read_to_string(&state_path)
            .with_context(|| format!("failed to read run state: {}", state_path.display()))?;
        let run_state: ClientRunState = toml::from_str(&content)
            .with_context(|| format!("failed to parse run state: {}", state_path.display()))?;

        match run_state.phase {
            ClientRunPhase::CleanupComplete => {
                let _ = fs::remove_dir_all(&dir_path);
                continue;
            }
            ClientRunPhase::Abandoned => {
                anyhow::bail!(
                    "abandoned tombstone exists for run {}/{} at {}; \
                     normal sync blocked. Remove the tombstone manually or use an \
                     explicit command to clear it.",
                    run_state.nickname,
                    run_state.run_id,
                    dir_path.display()
                );
            }
            ClientRunPhase::Corrupt => {
                anyhow::bail!(
                    "corrupt tombstone exists for run {}/{} at {}; \
                     normal sync blocked. Remove the tombstone manually or use an \
                     explicit command to clear it.",
                    run_state.nickname,
                    run_state.run_id,
                    dir_path.display()
                );
            }
            ClientRunPhase::TerminalStatusSeen => {
                let nickname = purgery_core::Nickname::new(run_state.nickname.clone())
                    .with_context(|| {
                        format!("invalid nickname in run state: {}", state_path.display())
                    })?;
                let run_id =
                    purgery_core::RunId::new(run_state.run_id.clone()).with_context(|| {
                        format!("invalid run ID in run state: {}", state_path.display())
                    })?;
                let manifest = Manifest::from_toml(&run_state.manifest).with_context(|| {
                    format!("invalid manifest in run state: {}", state_path.display())
                })?;
                let status = read_and_verify_terminal_status(
                    config,
                    config.server.host.as_str(),
                    &config.server.command,
                    &manifest,
                    &nickname,
                    &run_id,
                )?;
                cleanup_from_verified_status(config, &manifest, &status, &nickname, &run_id)?;
                continue;
            }
            _ => {}
        }

        let nickname = purgery_core::Nickname::new(run_state.nickname.clone())
            .with_context(|| format!("invalid nickname in run state: {}", state_path.display()))?;
        let run_id = purgery_core::RunId::new(run_state.run_id.clone())
            .with_context(|| format!("invalid run ID in run state: {}", state_path.display()))?;
        let manifest = Manifest::from_toml(&run_state.manifest)
            .with_context(|| format!("invalid manifest in run state: {}", state_path.display()))?;

        info!(
            nickname = %nickname.as_str(),
            run_id = %run_id.as_str(),
            phase = ?run_state.phase,
            "resuming pending postprocess run"
        );

        let host = config.server.host.as_str();
        let server_command = &config.server.command;

        if run_state.phase == ClientRunPhase::UploadCompleteFinishPending {
            let run_state_resp = (|| -> Result<purgery_core::RunStateResponse> {
                let output = server_cmd(
                    host,
                    server_command,
                    &[
                        "run-state",
                        "--nickname",
                        nickname.as_str(),
                        "--run-id",
                        run_id.as_str(),
                    ],
                )?;
                toml::from_str(&output).with_context(|| "failed to parse run-state")
            })()
            .with_context(|| {
                format!(
                    "run-state query failed for pending run {}/{}",
                    nickname.as_str(),
                    run_id.as_str()
                )
            })?;

            match run_state_resp.phase.as_str() {
                "incoming" => {
                    info!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        "run still incoming, issuing finish-run"
                    );
                    server_cmd(
                        host,
                        server_command,
                        &[
                            "finish-run",
                            "--nickname",
                            nickname.as_str(),
                            "--run-id",
                            run_id.as_str(),
                        ],
                    )
                    .with_context(|| {
                        format!(
                            "finish-run failed during resume for run {}/{}",
                            nickname.as_str(),
                            run_id.as_str()
                        )
                    })?;
                    write_client_run_state(
                        &config.state_dir,
                        nickname.as_str(),
                        run_id.as_str(),
                        &manifest,
                        &build_run_config(config, true),
                        ClientRunPhase::WaitingForTerminalState,
                    )?;
                }
                "not_found" => {
                    warn!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        "run not found on server"
                    );
                    return Err(persist_abandoned_or_error(
                        &config.state_dir,
                        nickname.as_str(),
                        run_id.as_str(),
                        &manifest,
                        &build_run_config(config, true),
                        &format!(
                            "run {}/{} not found on server",
                            nickname.as_str(),
                            run_id.as_str()
                        ),
                    ));
                }
                "corrupt" => {
                    warn!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        "run state is corrupt on server"
                    );
                    return Err(persist_corrupt_or_error(
                        &config.state_dir,
                        nickname.as_str(),
                        run_id.as_str(),
                        &manifest,
                        &build_run_config(config, true),
                        &format!(
                            "run {}/{} server state is corrupt",
                            nickname.as_str(),
                            run_id.as_str()
                        ),
                    ));
                }
                "ready" | "processing" => {}
                phase @ ("done" | "failed") => {
                    if run_state_resp.terminal {
                        let status = read_and_verify_terminal_status(
                            config,
                            host,
                            server_command,
                            &manifest,
                            &nickname,
                            &run_id,
                        )?;
                        cleanup_from_verified_status(
                            config, &manifest, &status, &nickname, &run_id,
                        )?;
                        continue;
                    }
                    anyhow::bail!(
                        "unexpected non-terminal phase '{phase}' for run {}/{}",
                        nickname.as_str(),
                        run_id.as_str()
                    );
                }
                other => {
                    anyhow::bail!(
                        "unexpected run-state phase '{other}' for run {}/{}",
                        nickname.as_str(),
                        run_id.as_str()
                    );
                }
            }
        }

        wait_for_postprocess_run_and_cleanup(
            config,
            host,
            server_command,
            &manifest,
            &nickname,
            &run_id,
        )
        .with_context(|| {
            format!(
                "failed to complete pending postprocess run {}/{}",
                nickname.as_str(),
                run_id.as_str()
            )
        })?;
    }
    Ok(())
}

/// Remove local entries that are confirmed imported and still match their uploaded identity.
pub(crate) fn delete_confirmed_files(
    config: &ClientConfig,
    manifest: &Manifest,
    status: &RunStatus,
) -> Result<usize> {
    let mut count = 0;

    // Build a lookup from local_path to manifest entry
    let manifest_by_path: std::collections::HashMap<&str, &ManifestEntry> = manifest
        .entries
        .iter()
        .map(|f| (f.local_path.as_str(), f))
        .collect();

    for entry_status in &status.entries {
        // Only process entries with status "imported"
        if entry_status.status != purgery_core::FileStatus::Imported {
            continue;
        }

        // Find the corresponding manifest entry
        let Some(manifest_entry) = manifest_by_path.get(entry_status.local_path.as_str()) else {
            warn!(
                local_path = %entry_status.local_path,
                "status references unknown local path"
            );
            continue;
        };

        // Status-based cleanup is for postprocess entries only.
        // Passthrough entries are cleaned via the durable cleanup ledger,
        // not from server status. Covered entries must not be independently
        // cleaned from status — they are retired as part of the postprocessed
        // directory-root cleanup.
        if manifest_entry.mode != purgery_core::ManifestEntryMode::Postprocess {
            continue;
        }

        // Find the corresponding sync mapping
        let Some(sync) = config
            .sync
            .iter()
            .find(|s| s.name.as_str() == manifest_entry.sync_name.as_str())
        else {
            warn!(
                sync_name = %manifest_entry.sync_name.as_str(),
                "no sync mapping for entry"
            );
            continue;
        };

        // Only delete if the sync mapping allows it
        if !sync.delete_after_import {
            continue;
        }

        let local_path_str = manifest_entry.local_path.as_str();
        let local_path = Path::new(local_path_str);

        // Use symlink_metadata to detect post-upload replacements.
        let symmeta = match fs::symlink_metadata(local_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Entry already gone — idempotent, count as success
                count += 1;
                continue;
            }
            Err(_) => {
                warn!(path = %local_path.display(), "cannot read metadata, not removing");
                continue;
            }
        };

        match manifest_entry.kind {
            purgery_core::ManifestEntryKind::Directory => {
                // Verify the current path is still a directory
                if !symmeta.file_type().is_dir() {
                    warn!(path = %local_path.display(), "local path is no longer a directory, not removing");
                    continue;
                }
                // Recursive preflight: check the root directory itself and every
                // descendant directory for unexpected filesystem children.
                // First, collect all descendant entries from the manifest.
                let prefix = format!("{}/", local_path_str);
                let mut descendants: Vec<&ManifestEntry> = manifest
                    .entries
                    .iter()
                    .filter(|me| me.local_path.as_str().starts_with(&prefix))
                    .collect();
                let mut preflight_ok = true;
                // Check the root directory itself first.
                if let Ok(reader) = fs::read_dir(local_path) {
                    for child in reader {
                        let child = match child {
                            Ok(c) => c,
                            Err(_) => {
                                preflight_ok = false;
                                break;
                            }
                        };
                        let child_path = child.path();
                        if child_path == local_path {
                            continue;
                        }
                        let child_local = child_path.to_string_lossy();
                        if !manifest
                            .entries
                            .iter()
                            .any(|me| me.local_path.as_str() == child_local.as_ref())
                        {
                            preflight_ok = false;
                            break;
                        }
                    }
                } else {
                    preflight_ok = false;
                }
                // Then check every descendant directory.
                for desc in &descendants {
                    if desc.kind != purgery_core::ManifestEntryKind::Directory {
                        continue;
                    }
                    let desc_path = Path::new(desc.local_path.as_str());
                    if let Ok(reader) = fs::read_dir(desc_path) {
                        for child in reader {
                            let child = match child {
                                Ok(c) => c,
                                Err(_) => {
                                    preflight_ok = false;
                                    break;
                                }
                            };
                            let child_path = child.path();
                            if child_path == desc_path {
                                continue;
                            }
                            let child_local = child_path.to_string_lossy();
                            if !manifest
                                .entries
                                .iter()
                                .any(|me| me.local_path.as_str() == child_local.as_ref())
                            {
                                preflight_ok = false;
                                break;
                            }
                        }
                    }
                    if !preflight_ok {
                        break;
                    }
                }
                if !preflight_ok {
                    warn!(path = %local_path.display(), "subtree has new or unexpected entries, not removing");
                    continue;
                }
                // Verify all known descendants still match their identities.
                // Absent descendants are treated as idempotently already removed.
                let mut all_verified = true;
                for desc in &descendants {
                    let desc_path = Path::new(desc.local_path.as_str());
                    if !desc_path.exists() {
                        continue;
                    }
                    if !verify_manifest_entry_local(desc) {
                        warn!(
                            path = %desc.local_path.as_str(),
                            "known descendant changed since upload, not removing directory"
                        );
                        all_verified = false;
                        break;
                    }
                }
                if !all_verified {
                    continue;
                }
                // All preflight checks passed - delete bottom-up (deepest first)
                // Skip the root itself since we handle it separately
                descendants.sort_by(|a, b| {
                    b.local_path
                        .as_str()
                        .len()
                        .cmp(&a.local_path.as_str().len())
                });
                for child in &descendants {
                    let child_path = Path::new(child.local_path.as_str());
                    match child.kind {
                        purgery_core::ManifestEntryKind::Directory => {
                            let _ = fs::remove_dir(child_path);
                        }
                        _ => {
                            let _ = fs::remove_file(child_path);
                        }
                    }
                }
                if let Err(e) = fs::remove_dir(local_path) {
                    warn!(path = %local_path.display(), error = %e, "failed to remove directory");
                } else {
                    count += 1;
                }
            }
            purgery_core::ManifestEntryKind::Symlink => {
                // Verify it's still a symlink
                if !symmeta.file_type().is_symlink() {
                    warn!(path = %local_path.display(), "local path is no longer a symlink, not removing");
                    continue;
                }
                // Verify the link target matches
                if let Some(ref expected_target) = manifest_entry.link_target {
                    if let Ok(current_target) = fs::read_link(local_path) {
                        let current = current_target.to_string_lossy().into_owned();
                        if current != expected_target.as_str() {
                            warn!(path = %local_path.display(), "symlink target changed, not removing");
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                // Unlink the symlink (never follow the target)
                if let Err(e) = fs::remove_file(local_path) {
                    warn!(path = %local_path.display(), error = %e, "failed to unlink symlink");
                } else {
                    count += 1;
                }
            }
            purgery_core::ManifestEntryKind::RegularFile => {
                // Verify it's still a regular file (not replaced by symlink)
                if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
                    warn!(path = %local_path.display(), "local path is no longer a regular file, not removing");
                    continue;
                }

                if let Ok(metadata) = fs::metadata(local_path) {
                    let current_size = metadata.len();
                    let current_mtime = metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);

                    let matches_size = current_size == manifest_entry.size;
                    let matches_mtime = current_mtime == manifest_entry.mtime_ns;
                    let matches_sha = match &manifest_entry.sha256 {
                        Some(expected_sha) => match compute_sha256(local_path) {
                            Ok(actual_sha) => &actual_sha == expected_sha,
                            Err(_) => {
                                warn!(path = %local_path.display(), "SHA-256 computation failed during cleanup verification, not removing");
                                false
                            }
                        },
                        None => {
                            warn!(path = %local_path.display(), "entry has no SHA-256 identity, not removing");
                            false
                        }
                    };

                    if !matches_size || !matches_mtime || !matches_sha {
                        warn!(path = %local_path.display(), "file changed since upload, not removing");
                        continue;
                    }
                } else {
                    warn!(path = %local_path.display(), "cannot read metadata, not removing");
                    continue;
                }

                if let Err(e) = fs::remove_file(local_path) {
                    warn!(path = %local_path.display(), error = %e, "failed to delete file");
                } else {
                    count += 1;
                }
            }
        }
    }

    Ok(count)
}

/// Verify that a manifest entry still matches its captured local identity.
/// Used by directory cleanup to check children before removal.
pub(crate) fn verify_manifest_entry_local(entry: &ManifestEntry) -> bool {
    let path = Path::new(entry.local_path.as_str());
    let symmeta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    match entry.kind {
        purgery_core::ManifestEntryKind::RegularFile => {
            if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
                return false;
            }
            let Ok(meta) = fs::metadata(path) else {
                return false;
            };
            if meta.len() != entry.size {
                return false;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            if mtime != entry.mtime_ns {
                return false;
            }
            let Some(expected_sha) = entry.sha256.as_ref() else {
                return false;
            };
            let Ok(actual_sha) = compute_sha256(path) else {
                return false;
            };
            actual_sha == *expected_sha
        }
        purgery_core::ManifestEntryKind::Symlink => {
            if !symmeta.file_type().is_symlink() {
                return false;
            }
            if let Some(ref expected_target) = entry.link_target {
                if let Ok(current) = fs::read_link(path) {
                    current.to_string_lossy().into_owned() == expected_target.as_str()
                } else {
                    false
                }
            } else {
                true
            }
        }
        purgery_core::ManifestEntryKind::Directory => symmeta.file_type().is_dir(),
    }
}
