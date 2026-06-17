#[cfg(not(unix))]
compile_error!("Purgery is Unix-only — it requires rsync, SSH, and Unix filesystem semantics");

use anyhow::{Context, Result};
use purgery_core::{Nickname, RunId, RunPhase, RunStatus, ServerConfig};
use std::fs;
use tracing::{info, warn};

#[cfg_attr(not(test), allow(unused_imports))]
use camino::Utf8Path;
#[cfg_attr(not(test), allow(unused_imports))]
use purgery_core::{FileStatus, Manifest, ManifestEntryKind, PurgeryRoot, RunState};

mod commit;
mod gc;
mod phases;
mod process;
mod recover;
mod transform;

pub use gc::run_gc;
pub use phases::{begin_run, find_processing_runs, find_ready_runs, finish_run, move_to_failed};
pub use process::{process_once_raw, process_processing_run, process_run_target};
pub use recover::{recover_or_process_processing_run, RecoveryError};
pub use transform::{apply_transform, apply_transform_with_heartbeat};

#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use commit::{
    commit_directory_entry, commit_regular_file_entry, commit_symlink_entry, CommitDisposition,
};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use phases::{write_progress, write_progress_best_effort};

/// A resolved transform definition.
#[derive(Debug, Clone)]
pub struct ResolvedTransform {
    pub name: String,
    pub def: purgery_core::TransformDefinition,
}

/// Process a ready run. Kept as the public single-run entry point.
pub fn process_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    match crate::process::claim_ready_run(config, nickname, run_id) {
        crate::process::ReadyClaimOutcome::Claimed(lock) => {
            let result = crate::process::process_processing_run(config, nickname, run_id);
            drop(lock);
            result.map_err(|e| match e {
                crate::process::ProcessingError::Incompatible { message, .. } => {
                    anyhow::anyhow!("{message}")
                }
                crate::process::ProcessingError::Other(e) => e,
            })
        }
        crate::process::ReadyClaimOutcome::AlreadyProcessing => Ok(()),
        crate::process::ReadyClaimOutcome::AlreadyTerminal => Ok(()),
        crate::process::ReadyClaimOutcome::IncompatibleReady { message } => {
            Err(anyhow::anyhow!("{message}"))
        }
        crate::process::ReadyClaimOutcome::MalformedReadyMovedToFailed { error } => Err(error),
        crate::process::ReadyClaimOutcome::MalformedReadyMoveFailed {
            original_error,
            publish_error,
        } => Err(anyhow::anyhow!(
            "malformed ready could not be moved to failed: \
             {original_error}; publication error: {publish_error}"
        )),
        crate::process::ReadyClaimOutcome::NotFound => Err(anyhow::anyhow!("run not found")),
        crate::process::ReadyClaimOutcome::ClaimFailed { error } => Err(error),
    }
}

/// Server-side subcommand: validate the run plan and resolve relative
/// destinations.
///
/// Must be called after the client has written `run.toml` and `manifest.toml`
/// into the incoming directory but before any rsync transfer.
/// This is the gate that prevents an invalid run plan from being processed.
///
/// If the destination in `run.toml` is relative, it is resolved against the
/// server's current working directory and `run.toml` is atomically rewritten
/// so that later `process-once` does not depend on cwd.
pub fn prepare_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<String> {
    let incoming_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Incoming);
    if !incoming_path.exists() {
        anyhow::bail!(
            "incoming directory does not exist for run {}/{} at '{}'",
            nickname.as_str(),
            run_id.as_str(),
            incoming_path.as_str()
        );
    }

    let run_config_path = incoming_path.join("run.toml");
    let run_config_content =
        fs::read_to_string(&run_config_path).with_context(|| "failed to read run config")?;
    purgery_core::require_compatible_toml_version(
        &run_config_content,
        format_args!("run config {run_config_path}"),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let run_config = purgery_core::RunConfig::from_toml(&run_config_content)
        .with_context(|| "failed to parse run config")?;

    if !run_config.delete_after_import {
        anyhow::bail!("transform runs require delete_after_import = true");
    }

    let manifest_path = incoming_path.join("manifest.toml");
    let manifest_content =
        fs::read_to_string(&manifest_path).with_context(|| "failed to read manifest")?;
    purgery_core::require_compatible_toml_version(
        &manifest_content,
        format_args!("manifest {manifest_path}"),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let manifest = purgery_core::Manifest::from_toml(&manifest_content)
        .with_context(|| "failed to parse manifest")?;

    if let Err(e) = purgery_core::validate_envelope(nickname, run_id, &run_config, &manifest) {
        anyhow::bail!("envelope validation failed: {e}");
    }

    if manifest.entries.len() != 1 {
        anyhow::bail!(
            "server run manifest must contain exactly one entry, got {}",
            manifest.entries.len(),
        );
    }

    for entry in &manifest.entries {
        if entry.transform.is_none() {
            anyhow::bail!(
                "server run entry '{}' has no transform",
                entry.relative_path.as_str(),
            );
        }
        let transform_name = entry.transform.as_deref().unwrap();
        if !config.transforms.contains_key(transform_name) {
            anyhow::bail!(
                "run plan validation failed for '{}': transform '{transform_name}' not defined on server",
                entry.relative_path.as_str()
            );
        }
        let def = &config.transforms[transform_name];
        if let Err(e) = purgery_core::validate_transform_definition(def) {
            anyhow::bail!(
                "run plan validation failed for '{}': transform '{transform_name}' definition is invalid: {e}",
                entry.relative_path.as_str()
            );
        }
    }

    // Resolve relative destination against server cwd.
    let resolved_destination = if !run_config.destination.is_absolute() {
        let cwd =
            std::env::current_dir().with_context(|| "failed to get current working directory")?;
        let resolved = cwd.join(run_config.destination.as_str());
        let resolved_utf8 = camino::Utf8PathBuf::from_path_buf(resolved)
            .map_err(|_| anyhow::anyhow!("resolved destination path is not valid UTF-8"))?;
        let resolved_dest = purgery_core::DestinationPath::new(resolved_utf8)
            .with_context(|| "resolved destination path is invalid")?;

        // Atomically rewrite run.toml with the resolved destination so that
        // later process-once does not depend on cwd.
        let updated = purgery_core::RunConfig {
            purgery_version: purgery_core::current_purgery_version().to_string(),
            nickname: run_config.nickname.clone(),
            destination: resolved_dest.clone(),
            delete_after_import: run_config.delete_after_import,
        };
        let updated_toml = updated
            .to_toml()
            .with_context(|| "failed to serialize updated run config")?;
        let tmp_path = run_config_path.with_extension("toml.tmp");
        fs::write(tmp_path.as_std_path(), &updated_toml)
            .with_context(|| "failed to write updated run config")?;
        fs::rename(tmp_path.as_std_path(), run_config_path.as_std_path())
            .with_context(|| "failed to commit updated run config")?;

        Some(resolved_dest.as_str().to_owned())
    } else {
        None
    };

    let response = purgery_core::PrepareRunResponse {
        protocol_version: 1,
        purgery_version: purgery_core::current_purgery_version().to_string(),
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        destination: resolved_destination,
    };

    toml::to_string(&response)
        .map_err(|e| anyhow::anyhow!("failed to serialize prepare-run response: {e}"))
}

/// Server-side subcommand: read the run status from done or failed.
pub fn read_run_status(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStatus> {
    let phases = [RunPhase::Done, RunPhase::Failed];

    for phase in &phases {
        let status_path = config
            .work_dir
            .run_dir(nickname, run_id, *phase)
            .join("status.toml");
        if !status_path.exists() {
            continue;
        }
        let content = fs::read_to_string(&status_path)
            .with_context(|| format!("failed to read status from '{}'", status_path.as_str()))?;
        let version_probe = purgery_core::probe_purgery_version_from_toml(&content);
        match RunStatus::from_toml(&content) {
            Ok(status) => {
                if purgery_core::require_compatible_purgery_version(
                    &status.purgery_version,
                    "status",
                )
                .is_err()
                {
                    warn!(
                        "incompatible status version in '{}'; skipping",
                        status_path.as_str()
                    );
                    continue;
                }
                if status.nickname != *nickname || status.run_id != *run_id {
                    anyhow::bail!(
                        "status envelope mismatch in '{}': expected {}/{}, got {}/{}",
                        status_path,
                        nickname.as_str(),
                        run_id.as_str(),
                        status.nickname.as_str(),
                        status.run_id.as_str()
                    );
                }
                return Ok(status);
            }
            Err(e) => match version_probe {
                Err(purgery_core::VersionProbeError::MissingVersion) => {
                    warn!(
                        "status file '{}' has missing purgery_version (too old); skipping",
                        status_path.as_str()
                    );
                    continue;
                }
                _ => {
                    anyhow::bail!("malformed status file '{}': {e}", status_path.as_str());
                }
            },
        }
    }

    anyhow::bail!(
        "no compatible status found for run {}/{} in done or failed",
        nickname.as_str(),
        run_id.as_str()
    );
}

/// Read the current run phase without requiring terminal status.
/// Returns a RunStateResponse describing the run's filesystem phase.
pub fn run_state(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<purgery_core::RunStateResponse> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Check non-terminal phases first (incoming, ready, processing)
    let non_terminal = [
        (RunPhase::Incoming, "incoming", false),
        (RunPhase::Ready, "ready", false),
        (RunPhase::Processing, "processing", false),
    ];
    for (phase, phase_str, _) in &non_terminal {
        let dir = config.work_dir.run_dir(nickname, run_id, *phase);
        if !dir.exists() {
            continue;
        }
        let (
            message,
            updated_at,
            progress_state,
            entry_index,
            entry_total,
            current_entry,
            current_transform,
            progress_status,
        ) = if *phase == RunPhase::Processing {
            read_progress_fields(&dir, nickname, run_id, now)
        } else {
            (
                format!("run phase: {}", phase_str),
                dir_modified_at(&dir).unwrap_or(0),
                None::<String>,
                None::<usize>,
                None::<usize>,
                None::<String>,
                None::<String>,
                None::<String>,
            )
        };
        return Ok(purgery_core::RunStateResponse {
            protocol_version: 1,
            purgery_version: purgery_core::current_purgery_version().to_string(),
            nickname: nickname.as_str().to_owned(),
            run_id: run_id.as_str().to_owned(),
            phase: phase_str.to_string(),
            terminal: false,
            message,
            updated_at_unix_secs: updated_at,
            observed_at_unix_secs: now,
            progress_state,
            entry_index,
            entry_total,
            current_entry,
            current_transform,
            progress_status,
        });
    }

    // Check terminal phases (done, failed) — require valid status.toml
    let terminal_phases = [(RunPhase::Done, "done"), (RunPhase::Failed, "failed")];
    for (phase, phase_str) in &terminal_phases {
        let dir = config.work_dir.run_dir(nickname, run_id, *phase);
        if !dir.exists() {
            continue;
        }
        let status_path = dir.join("status.toml");
        match try_read_status(&status_path, nickname, run_id) {
            TerminalStatusOutcome::Valid => {
                return Ok(purgery_core::RunStateResponse {
                    protocol_version: 1,
                    purgery_version: purgery_core::current_purgery_version().to_string(),
                    nickname: nickname.as_str().to_owned(),
                    run_id: run_id.as_str().to_owned(),
                    phase: phase_str.to_string(),
                    terminal: true,
                    message: format!("run is {}", phase_str),
                    updated_at_unix_secs: dir_modified_at(&dir).unwrap_or(0),
                    observed_at_unix_secs: now,
                    progress_state: None,
                    entry_index: None,
                    entry_total: None,
                    current_entry: None,
                    current_transform: None,
                    progress_status: None,
                });
            }
            TerminalStatusOutcome::Incompatible { path } => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    status_path = %path.as_str(),
                    "{} directory has incompatible status; ignoring for run-state response",
                    phase_str,
                );
                // Continue to check the next terminal phase
            }
            TerminalStatusOutcome::Malformed(reason) => {
                return Ok(purgery_core::RunStateResponse {
                    protocol_version: 1,
                    purgery_version: purgery_core::current_purgery_version().to_string(),
                    nickname: nickname.as_str().to_owned(),
                    run_id: run_id.as_str().to_owned(),
                    phase: "corrupt".to_string(),
                    terminal: false,
                    message: format!("{} directory exists but status is {reason}", phase_str),
                    updated_at_unix_secs: dir_modified_at(&dir).unwrap_or(0),
                    observed_at_unix_secs: now,
                    progress_state: None,
                    entry_index: None,
                    entry_total: None,
                    current_entry: None,
                    current_transform: None,
                    progress_status: None,
                });
            }
        }
    }

    // No phase directory found
    Ok(purgery_core::RunStateResponse {
        protocol_version: 1,
        purgery_version: purgery_core::current_purgery_version().to_string(),
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        phase: "not_found".to_string(),
        terminal: false,
        message: "no matching run found".to_string(),
        updated_at_unix_secs: 0,
        observed_at_unix_secs: now,
        progress_state: None,
        entry_index: None,
        entry_total: None,
        current_entry: None,
        current_transform: None,
        progress_status: None,
    })
}

/// Outcome of attempting to read a terminal status file.
enum TerminalStatusOutcome {
    /// Compatible status with matching envelope.
    Valid,
    /// File exists but purgery_version is missing or incompatible.
    /// Must not be semantically reused.
    Incompatible { path: camino::Utf8PathBuf },
    /// File exists but is malformed current-format content.
    Malformed(String),
}

/// Try to read and validate a terminal status file.
fn try_read_status(
    status_path: &camino::Utf8Path,
    nickname: &Nickname,
    run_id: &RunId,
) -> TerminalStatusOutcome {
    let content = match std::fs::read_to_string(status_path.as_std_path()) {
        Ok(c) => c,
        Err(_) => return TerminalStatusOutcome::Malformed("missing/unreadable".to_string()),
    };
    let version_probe = purgery_core::probe_purgery_version_from_toml(&content);
    match RunStatus::from_toml(&content) {
        Ok(status) => {
            if purgery_core::require_compatible_purgery_version(&status.purgery_version, "status")
                .is_err()
            {
                return TerminalStatusOutcome::Incompatible {
                    path: status_path.to_owned(),
                };
            }
            if status.nickname != *nickname || status.run_id != *run_id {
                return TerminalStatusOutcome::Malformed("envelope mismatch".to_string());
            }
            TerminalStatusOutcome::Valid
        }
        Err(_) => match version_probe {
            Err(purgery_core::VersionProbeError::MissingVersion) => {
                TerminalStatusOutcome::Incompatible {
                    path: status_path.to_owned(),
                }
            }
            _ => TerminalStatusOutcome::Malformed("malformed".to_string()),
        },
    }
}

/// Read progress.toml fields for a processing-phase response.
#[allow(clippy::type_complexity)]
fn read_progress_fields(
    dir: &camino::Utf8Path,
    nickname: &Nickname,
    run_id: &RunId,
    _now: u64,
) -> (
    String,
    u64,
    Option<String>,
    Option<usize>,
    Option<usize>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let progress_path = dir.join("progress.toml");
    match std::fs::read_to_string(&progress_path) {
        Ok(content) => {
            // Probe purgery_version from raw TOML so we can distinguish
            // missing/incompatible version from malformed current content.
            let version_probe = purgery_core::probe_purgery_version_from_toml(&content);
            match toml::from_str::<purgery_core::ProcessingProgress>(&content) {
                Ok(prog) => {
                    if purgery_core::require_compatible_purgery_version(
                        &prog.purgery_version,
                        "progress",
                    )
                    .is_err()
                    {
                        (
                            "run phase: processing (incompatible progress version)".to_string(),
                            dir_modified_at(dir).unwrap_or(0),
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some("incompatible_version".to_string()),
                        )
                    } else if prog.nickname == nickname.as_str() && prog.run_id == run_id.as_str() {
                        let msg = format!(
                            "processing: {}/{} entries, current: {} transform: {}",
                            prog.entry_index + 1,
                            prog.entry_total,
                            prog.current_entry,
                            prog.current_transform
                        );
                        (
                            msg,
                            prog.updated_at_unix_secs,
                            Some(prog.state),
                            Some(prog.entry_index),
                            Some(prog.entry_total),
                            Some(prog.current_entry),
                            Some(prog.current_transform),
                            Some("valid".to_string()),
                        )
                    } else {
                        (
                            "run phase: processing (progress envelope mismatch)".to_string(),
                            dir_modified_at(dir).unwrap_or(0),
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some("envelope_mismatch".to_string()),
                        )
                    }
                }
                Err(_) => match version_probe {
                    Err(purgery_core::VersionProbeError::MissingVersion) => (
                        "run phase: processing (progress missing purgery_version)".to_string(),
                        dir_modified_at(dir).unwrap_or(0),
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some("incompatible_version".to_string()),
                    ),
                    _ => (
                        "run phase: processing (malformed progress)".to_string(),
                        dir_modified_at(dir).unwrap_or(0),
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some("malformed".to_string()),
                    ),
                },
            }
        }
        Err(_) => (
            "run phase: processing".to_string(),
            dir_modified_at(dir).unwrap_or(0),
            None,
            None,
            None,
            None,
            None,
            Some("missing".to_string()),
        ),
    }
}

/// Get the modification time of a directory in unix seconds.
fn dir_modified_at(dir: &camino::Utf8Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(dir.as_std_path()).ok()?;
    let modified = meta.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Side-effect-free server check: verify config and programs without creating anything.
pub fn server_check(config: &ServerConfig) -> Result<()> {
    info!("checking server configuration");

    if config.gc.incoming_lease_secs == 0 {
        anyhow::bail!("gc.incoming_lease_secs must be greater than 0");
    }
    if config.gc.heartbeat_interval_secs == 0 {
        anyhow::bail!("gc.heartbeat_interval_secs must be greater than 0");
    }
    if config.gc.heartbeat_interval_secs > config.gc.incoming_lease_secs / 2 {
        anyhow::bail!(
            "gc.heartbeat_interval_secs ({}) must be <= half of gc.incoming_lease_secs ({}) \
             to provide a safety margin for lease renewal",
            config.gc.heartbeat_interval_secs,
            config.gc.incoming_lease_secs
        );
    }

    let purgery_path = config.work_dir.as_path();
    if !purgery_path.exists() {
        anyhow::bail!(
            "work_dir '{}' does not exist (run `purgery-server bootstrap` to create it)",
            purgery_path.as_str()
        );
    }
    if !purgery_path.is_dir() {
        anyhow::bail!(
            "work_dir '{}' exists but is not a directory",
            purgery_path.as_str()
        );
    }
    info!(path = %purgery_path.as_str(), "work_dir: OK");

    for td in config.transforms.values() {
        purgery_core::validate_transform_definition(td)
            .map_err(|e| anyhow::anyhow!("transform '{}': {e}", td.name))?;

        purgery_core::resolve_executable(&td.program).map(
            |r| info!(transform = td.name, path = %r.path.as_str(), "transform program found"),
        )?;
    }

    info!("server configuration: OK");
    Ok(())
}

/// Print server version information as TOML (no config required).
pub fn version_response() -> String {
    format!(
        r#"protocol_version = {}
purgery_version = "{}"
"#,
        purgery_core::PROTOCOL_VERSION,
        purgery_core::current_purgery_version(),
    )
}

/// Bootstrap: create root and work_dir directories.
pub fn bootstrap(config: &ServerConfig) -> Result<()> {
    info!("bootstrapping server directories");

    let purgery_path = config.work_dir.as_path();
    fs::create_dir_all(purgery_path.as_std_path())
        .with_context(|| format!("failed to create work_dir: {}", purgery_path.as_str()))?;
    info!(path = %purgery_path.as_str(), "created work_dir");

    info!("bootstrap complete");
    Ok(())
}

/// Heartbeat: update lease file for an incoming run.
pub fn heartbeat_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    let incoming_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Incoming);
    if !incoming_path.exists() {
        anyhow::bail!(
            "run {}/{} is not in incoming phase",
            nickname.as_str(),
            run_id.as_str()
        );
    }

    let lease_path = incoming_path.join("lease.toml");
    let lease_content = fs::read_to_string(lease_path.as_std_path())
        .with_context(|| "failed to read lease file")?;
    // Probe raw TOML for version before full deserialization
    if let Err(e) = purgery_core::probe_purgery_version_from_toml(&lease_content) {
        anyhow::bail!(
            "cannot heartbeat run: lease is missing purgery_version or has invalid TOML \
             (producer version cannot be established) at '{}': {e}",
            lease_path.as_str(),
        );
    }
    let mut lease: purgery_core::LeaseFile =
        toml::from_str(&lease_content).with_context(|| "failed to parse lease file")?;
    purgery_core::require_compatible_purgery_version(&lease.purgery_version, "lease")
        .with_context(|| "incompatible lease version")?;

    if lease.protocol_version != 1 {
        anyhow::bail!(
            "lease protocol version {} does not match expected 1",
            lease.protocol_version
        );
    }
    if lease.nickname != nickname.as_str() {
        anyhow::bail!(
            "lease nickname '{}' does not match expected '{}'",
            lease.nickname,
            nickname.as_str()
        );
    }
    if lease.run_id != run_id.as_str() {
        anyhow::bail!(
            "lease run_id '{}' does not match expected '{}'",
            lease.run_id,
            run_id.as_str()
        );
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    lease.last_heartbeat_unix_secs = now;
    lease.expires_at_unix_secs = now + config.gc.incoming_lease_secs;

    let new_content = toml::to_string(&lease).with_context(|| "failed to serialize lease")?;
    let tmp_path = incoming_path.join("lease.toml.tmp");
    fs::write(tmp_path.as_std_path(), &new_content).with_context(|| "failed to write lease")?;
    fs::rename(tmp_path.as_std_path(), lease_path.as_std_path())
        .with_context(|| "failed to commit lease")?;

    Ok(())
}

// ── Remote shell escaping ──────────────────────────────────────────

/// Build a remote SSH command from a program and arguments.
///
/// Each argument is shell-escaped individually to avoid shell injection
/// from paths containing spaces or special characters.
pub fn build_remote_command(program: &str, args: &[String]) -> String {
    let mut cmd = String::new();
    cmd.push_str(program);
    for a in args {
        cmd.push(' ');
        cmd.push_str(&purgery_core::shell_escape(a));
    }
    cmd
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::commit_directory_tree;
    use camino::Utf8PathBuf;
    use purgery_core::{
        ClientLocalPath, ManifestEntry, NormalizedRelativePath, TransformDefinition, TransformKind,
    };
    use std::collections::BTreeMap;

    fn single_transform(
        name: &str,
        def: TransformDefinition,
    ) -> BTreeMap<String, TransformDefinition> {
        let mut m = BTreeMap::new();
        m.insert(name.to_owned(), def);
        m
    }

    /// Call apply_transform with a no-op progress callback for testing.
    fn test_apply_transform(
        server_config: &ServerConfig,
        work_path: &Utf8Path,
    ) -> Result<Vec<Utf8PathBuf>, String> {
        let target_directory = work_path.parent().unwrap_or(work_path).to_owned();
        let destination_root = &target_directory;
        test_apply_transform_with_target(
            server_config,
            work_path,
            destination_root,
            &target_directory,
        )
    }

    fn test_apply_transform_with_target(
        server_config: &ServerConfig,
        work_path: &Utf8Path,
        destination_root: &Utf8Path,
        target_directory: &Utf8Path,
    ) -> Result<Vec<Utf8PathBuf>, String> {
        let (name, _) = server_config
            .transforms
            .iter()
            .next()
            .expect("test plan must have at least one transform");
        let resolved = ResolvedTransform {
            name: name.clone(),
            def: server_config.transforms[name].clone(),
        };
        apply_transform(
            &resolved,
            work_path,
            destination_root,
            target_directory,
            &mut |_: &purgery_core::ProgressUpdate| {},
            0,
            1,
            "test",
        )
    }

    fn test_server_config(work_dir: &Utf8Path) -> ServerConfig {
        fs::create_dir_all(work_dir).unwrap();
        ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: BTreeMap::new(),
            logging: Default::default(),
        }
    }

    fn test_storage_root(work_dir: &Utf8Path) -> Utf8PathBuf {
        work_dir.parent().unwrap().join("storage")
    }

    fn test_destination_from_run_dir(dir: &Utf8Path, requested: &str) -> Utf8PathBuf {
        let work_dir = dir.ancestors().nth(3).unwrap();
        test_storage_root(work_dir).join(requested)
    }

    fn write_run_toml(dir: &Utf8Path, nickname: &Nickname) {
        let destination = test_destination_from_run_dir(dir, "univ/default");
        let content = format!(
            r#"purgery_version = "0.1.0-test"
nickname = "{}"
destination = "{}"
delete_after_import = true
"#,
            nickname.as_str(),
            destination.as_str()
        );
        fs::write(dir.join("run.toml"), &content).unwrap();
    }

    fn write_run_toml_with_destination(
        dir: &Utf8Path,
        nickname: &Nickname,
        destination_path: &str,
    ) {
        let requested = if destination_path.contains('/') {
            destination_path.to_owned()
        } else {
            format!("univ/{destination_path}")
        };
        let destination = test_destination_from_run_dir(dir, &requested);
        let content = format!(
            r#"purgery_version = "0.1.0-test"
nickname = "{}"
destination = "{}"
delete_after_import = true
"#,
            nickname.as_str(),
            destination.as_str(),
        );
        fs::write(dir.join("run.toml"), &content).unwrap();
    }

    fn write_run_toml_with_raw_destination(
        dir: &Utf8Path,
        nickname: &Nickname,
        destination_raw: &str,
    ) {
        let content = format!(
            r#"purgery_version = "0.1.0-test"
nickname = "{}"
destination = "{}"
delete_after_import = true
"#,
            nickname.as_str(),
            destination_raw,
        );
        fs::write(dir.join("run.toml"), &content).unwrap();
    }

    /// Helper to create a basic setup with a ready run containing one file.
    #[allow(clippy::too_many_arguments)]
    fn setup_single_file_ready(
        work_dir: &Utf8Path,
        nickname: &Nickname,
        run_id: &RunId,
        destination_path: &str,
        relative_path: &str,
        content: &[u8],
    ) -> (ServerConfig, Utf8PathBuf) {
        let config = test_server_config(work_dir);
        let ready_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        let staged_rel = format!("files/{relative_path}");
        let staged_path = ready_path.join(&staged_rel);
        if let Some(parent) = staged_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&staged_path, content).unwrap();

        write_run_toml_with_destination(&ready_path, nickname, destination_path);

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new(format!("/home/user/{relative_path}")).unwrap(),
                staged_path: NormalizedRelativePath::new(staged_rel.into()).unwrap(),
                relative_path: NormalizedRelativePath::new(relative_path.into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: content.len() as u64,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        (config, staged_path)
    }

    // ── full processing pipeline nickname-free archive paths ──

    #[test]
    fn full_processing_pipeline_uses_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-001".into()).unwrap();

        let (config, staged_file_path) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "univ/videos",
            "test.mp4",
            b"hello world",
        );

        process_run(&config, &nickname, &run_id).unwrap();
        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert!(
            !status.entries[0]
                .final_paths
                .iter()
                .any(|fp| fp.contains("laptop")),
            "final_paths must not contain nickname: {:?}",
            status.entries[0].final_paths
        );
        assert_eq!(
            status.entries[0].final_paths,
            vec![test_storage_root(config.work_dir.as_path())
                .join("univ/videos/test.mp4")
                .as_str()
                .to_owned()],
            "final_paths must contain the exact final destination path"
        );

        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4");
        assert!(final_path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "hello world");
        assert!(!staged_file_path.exists());
    }

    // ── Core pipeline test ──

    #[test]
    fn test_full_processing_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-001".into()).unwrap();

        let (config, staged_file_path) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "videos",
            "test.mp4",
            b"hello world",
        );

        process_run(&config, &nickname, &run_id).unwrap();
        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert!(!status.entries[0].final_paths.is_empty());

        assert!(!staged_file_path.exists());
    }

    #[test]
    fn test_processing_missing_staged_file() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-003".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/missing.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/missing.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("missing.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("failed to read staged metadata"));
    }

    #[test]
    fn test_find_ready_runs_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let root =
            PurgeryRoot::new(Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap())
                .unwrap();
        let runs = find_ready_runs(&root).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn test_find_ready_runs_with_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root =
            PurgeryRoot::new(Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap())
                .unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();

        let run1 = root.run_dir(
            &nickname,
            &RunId::new("run-1".into()).unwrap(),
            RunPhase::Ready,
        );
        let run2 = root.run_dir(
            &nickname,
            &RunId::new("run-2".into()).unwrap(),
            RunPhase::Ready,
        );
        fs::create_dir_all(&run1).unwrap();
        fs::create_dir_all(&run2).unwrap();

        let runs = find_ready_runs(&root).unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn test_nickname_mismatch_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let dir_nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-env-001".into()).unwrap();

        let ready_path = config
            .work_dir
            .run_dir(&dir_nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        // Run config has different nickname than the directory
        let run_config_content = r#"purgery_version = "0.1.0-test"
nickname = "other-machine"
destination = "univ/default"
delete_after_import = true
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: Nickname::new("other-machine".into()).unwrap(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,

                transform: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&config, &dir_nickname, &run_id);
        assert!(result.is_err());

        let failed_path = config
            .work_dir
            .run_dir(&dir_nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("envelope validation failed"));
    }

    #[test]
    fn test_bad_manifest_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-manifest".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        write_run_toml(&ready_path, &nickname);
        fs::write(ready_path.join("manifest.toml"), "not valid toml {{{").unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(result.is_err());

        let failed_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("manifest"));
    }

    #[test]
    fn test_bad_run_config_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-config".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        fs::write(ready_path.join("run.toml"), "not valid toml {{{").unwrap();

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,

                transform: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(result.is_err());

        let failed_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("run config"));
    }

    #[test]
    fn test_build_remote_command() {
        let args = vec!["--input".to_string(), "/path/file.mp4".to_string()];
        let cmd = build_remote_command("my-compress-video", &args);
        assert_eq!(cmd, "my-compress-video '--input' '/path/file.mp4'");
    }

    #[test]
    fn test_build_remote_command_with_spaces() {
        let args = vec![
            "--input".to_string(),
            "/path/with spaces/file.mp4".to_string(),
        ];
        let cmd = build_remote_command("rsync", &args);
        assert_eq!(cmd, "rsync '--input' '/path/with spaces/file.mp4'");
    }

    #[test]
    fn test_transforms_path_with_spaces() {
        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress-video",
                TransformDefinition {
                    name: "compress-video".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".to_owned(),
                    args: vec![],
                    expected_outputs: vec![],
                },
            ),
            logging: Default::default(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let work_area = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_path = work_area.join("some file.mp4");
        fs::write(&work_path, b"test data").unwrap();

        let results = test_apply_transform(&server_config, &work_path);
        assert!(results.is_ok(), "transform with spaces should succeed");
        assert!(results.unwrap().is_empty());
    }

    #[test]
    fn test_transforms_failure_does_not_create_final_output() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.as_str().into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress-video",
                TransformDefinition {
                    name: "compress-video".into(),
                    kind: TransformKind::Subprocess,
                    program: "false".to_owned(),
                    args: vec![],
                    expected_outputs: vec![],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-fail-pp".into()).unwrap();

        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/test.mp4"), b"video content").unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "videos");
        write_run_toml_with_destination(&ready_path, &nickname, "univ/videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("compress-video".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let failed_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0].error.as_ref().unwrap().contains("failed"));

        let final_path = server_config.work_dir.as_path().join("videos/test.mp4");
        assert!(
            !final_path.exists(),
            "failed transform must not create final output"
        );
    }

    #[test]
    fn test_compress_video_verify_output_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let work_area = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_path = work_area.join("video.mp4");
        fs::write(&work_path, b"video").unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress-video",
                TransformDefinition {
                    name: "compress-video".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".to_owned(),
                    args: vec![],
                    expected_outputs: vec![],
                },
            ),
            logging: Default::default(),
        };
        let result = test_apply_transform(&server_config, &work_path);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_transform_produces_only_expected_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let work_area = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_path = work_area.join("video.mp4");
        fs::write(&work_path, b"video").unwrap();

        let compressed = work_area.join("video.Z.webm");
        fs::write(&compressed, b"compressed").unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress-video",
                TransformDefinition {
                    name: "compress-video".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".to_owned(),
                    args: vec![],
                    expected_outputs: vec!["{stem}.Z.webm".into()],
                },
            ),
            logging: Default::default(),
        };
        let result = test_apply_transform(&server_config, &work_path);
        assert!(result.is_ok());
        let outputs = result.unwrap();
        assert_eq!(outputs.len(), 1);
        assert!(
            outputs.contains(&compressed),
            "must include expected output"
        );
        // Server never includes the original work path — only expected outputs
        assert!(
            !outputs.contains(&work_path),
            "server must not include work_path as a transform output"
        );
    }

    #[test]
    fn regular_file_commit_produces_only_expected_final_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-tmp-commit".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &work_dir, &nickname, &run_id, "videos", "test.mp4", b"hello",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4");
        assert!(final_path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "hello");

        let expected = vec![
            config.work_dir.as_path().join("univ"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4"),
            config.work_dir.as_path().join("laptop"),
            config.work_dir.as_path().join("laptop/done"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-tmp-commit"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-tmp-commit/files"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-tmp-commit/files/test.mp4"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-tmp-commit/manifest.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-tmp-commit/progress.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-tmp-commit/run.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-tmp-commit/status.toml"),
            config.work_dir.as_path().join("laptop/processing"),
            config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(config.work_dir.as_path(), &expected);
    }

    // ── Atomic replacement tests ──

    #[test]
    fn test_existing_regular_final_output_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-replace".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "videos",
            "test.mp4",
            b"new content",
        );

        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"old content").unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "new content");
        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done_path.join("status.toml")).unwrap())
                .unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    #[test]
    fn test_regular_file_replaces_existing_empty_directory_like_rsync() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-directory-block".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &work_dir, &nickname, &run_id, "videos", "test.mp4", b"content",
        );
        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4");
        fs::create_dir_all(&final_path).unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "content");
        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done_path.join("status.toml")).unwrap())
                .unwrap();
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    #[test]
    #[cfg(unix)]
    fn test_regular_file_replaces_existing_symlink_like_rsync() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-final-symlink".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "documents",
            "a.txt",
            b"content",
        );
        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/documents/a.txt");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("missing-target", &final_path).unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "content");
        assert!(!fs::symlink_metadata(&final_path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    // ── Work area namespacing test ──

    #[test]
    fn test_work_area_namespacing_no_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-ns".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/a.mp4"), b"video content").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let video_final = test_storage_root(config.work_dir.as_path()).join("univ/videos/a.mp4");
        assert!(video_final.exists());
        assert_eq!(fs::read_to_string(&video_final).unwrap(), "video content");

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    // ── Staged path mismatch test ──

    #[test]
    fn test_manifest_staged_path_mismatch_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-sp-mismatch".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"content").unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/other/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("compress-video".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("staged_path mismatch"));
    }

    #[test]
    fn test_manifest_staged_path_match_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-sp-match".into()).unwrap();

        let (config, _) =
            setup_single_file_ready(&work_dir, &nickname, &run_id, "videos", "a.mp4", b"content");

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    // ── Staged symlink rejection test ──

    #[test]
    fn test_staged_symlink_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-symlink".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();

        let real_file = ready_path.join("files/real.mp4");
        fs::write(&real_file, b"real content").unwrap();
        let staged_link = ready_path.join("files/a.mp4");
        std::os::unix::fs::symlink(&real_file, &staged_link).unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 12,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let final_path = _server_root.join("videos/a.mp4");
        assert!(
            !final_path.exists(),
            "symlink must not be imported to final path"
        );

        let failed_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("kind does not match"));
    }

    // ── Invalid regex test ──

    // ── Work area cleanup tests ──

    #[test]
    fn test_run_state_done_removes_work_area() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-done-wa".into()).unwrap();

        let (config, _) =
            setup_single_file_ready(&work_dir, &nickname, &run_id, "videos", "a.mp4", b"hello");

        process_run(&config, &nickname, &run_id).unwrap();

        let work_area = purgery_core::work_dir(&config.work_dir, &nickname, &run_id);
        assert!(!work_area.exists(), "work area must be removed on Done");
    }

    #[test]
    fn test_run_state_partial_keeps_work_area() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.as_str().into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress-video",
                TransformDefinition {
                    name: "compress-video".into(),
                    kind: TransformKind::Subprocess,
                    program: "false".to_owned(),
                    args: vec![],
                    expected_outputs: vec![],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-partial-wa".into()).unwrap();

        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/test.mp4"), b"video content").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("compress-video".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let failed_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed_path.exists());
        // The work area lives inside the run's processing directory, so it moves
        // with the run to the failed directory when the run is finalized.
        let work_area_after_move = failed_path.join("work");
        assert!(
            work_area_after_move.exists(),
            "work area must be kept for Failed state"
        );
    }

    // ── Transform end-to-end: only expected outputs committed ──

    #[test]
    fn test_transform_commits_only_expected_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        let script_path = tmp.path().join("compress.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\n\
             input=\"$2\"; target_dir=\"$3\"\n\
             stem=\"${input##*/}\"; stem=\"${stem%.*}\"\n\
             mkdir -p \"$target_dir\"\n\
             touch \"$target_dir/$stem.Z.webm\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.as_str().into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress-video",
                TransformDefinition {
                    name: "compress-video".into(),
                    kind: TransformKind::Subprocess,
                    program: script_path.to_string_lossy().to_string(),
                    args: vec![
                        "--input".into(),
                        "{input}".into(),
                        "{target_directory}".into(),
                    ],
                    expected_outputs: vec!["{stem}.Z.webm".into()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-only-expected".into()).unwrap();

        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/video.mp4"), b"video").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/video.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/video.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("video.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("compress-video".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let done_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[0].final_paths.len(), 1);

        let original_final =
            test_storage_root(server_config.work_dir.as_path()).join("univ/videos/video.mp4");
        let compressed_final =
            test_storage_root(server_config.work_dir.as_path()).join("univ/videos/video.Z.webm");
        assert!(
            !original_final.exists(),
            "server must not commit original to final destination"
        );
        assert!(
            compressed_final.exists(),
            "transform script wrote compressed output to target_directory"
        );
    }

    #[test]
    fn test_transform_commits_expected_output() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        let script_path = tmp.path().join("compress.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\n\
             input=\"$2\"; target_dir=\"$3\"\n\
             stem=\"${input##*/}\"; stem=\"${stem%.*}\"\n\
             mkdir -p \"$target_dir\"\n\
             touch \"$target_dir/$stem.Z.webm\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.as_str().into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress-video",
                TransformDefinition {
                    name: "compress-video".into(),
                    kind: TransformKind::Subprocess,
                    program: script_path.to_string_lossy().to_string(),
                    args: vec![
                        "--input".into(),
                        "{input}".into(),
                        "{target_directory}".into(),
                    ],
                    expected_outputs: vec!["{stem}.Z.webm".into()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-expected-only".into()).unwrap();

        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/video.mp4"), b"video").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/video.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/video.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("video.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("compress-video".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let done_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[0].final_paths.len(), 1);

        let original_final =
            test_storage_root(server_config.work_dir.as_path()).join("univ/videos/video.mp4");
        let compressed_final =
            test_storage_root(server_config.work_dir.as_path()).join("univ/videos/video.Z.webm");
        assert!(
            !original_final.exists(),
            "server must not commit original to final destination"
        );
        assert!(
            compressed_final.exists(),
            "transform script wrote compressed output to target_directory"
        );
    }

    // ── begin_run / finish_run tests ──

    #[test]
    fn test_begin_run_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let _root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            transforms: BTreeMap::new(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-begin".into()).unwrap();

        let response_str = begin_run(&server_config, &nickname, &run_id).unwrap();
        let response: purgery_core::BeginRunResponse = toml::from_str(&response_str).unwrap();
        assert_eq!(response.protocol_version, 1);
        assert_eq!(response.nickname, "laptop");
        assert_eq!(response.run_id, "test-begin");

        let incoming_path = Utf8Path::new(&response.incoming_dir);
        assert!(incoming_path.exists(), "incoming dir must exist");
        assert!(
            Utf8Path::new(&response.files_dir).exists(),
            "files dir must exist"
        );
    }

    #[test]
    fn test_finish_run_moves_from_incoming_to_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let _root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            transforms: BTreeMap::new(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-finish".into()).unwrap();

        // Begin the run
        begin_run(&server_config, &nickname, &run_id).unwrap();

        let incoming_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        assert!(incoming_path.exists());

        // Finish it
        finish_run(&server_config, &nickname, &run_id).unwrap();

        assert!(
            !incoming_path.exists(),
            "incoming must be gone after finish"
        );
        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        assert!(ready_path.exists(), "ready dir must exist after finish");
    }

    #[test]
    fn test_read_run_status_from_done() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-status".into()).unwrap();

        let (config, _) =
            setup_single_file_ready(&work_dir, &nickname, &run_id, "videos", "a.mp4", b"data");

        process_run(&config, &nickname, &run_id).unwrap();

        let status = read_run_status(&config, &nickname, &run_id).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.nickname, nickname);
        assert_eq!(status.run_id, run_id);
    }

    #[test]
    fn test_read_run_status_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let _root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            transforms: BTreeMap::new(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("nonexistent".into()).unwrap();

        let result = read_run_status(&server_config, &nickname, &run_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_finish_run_rejects_expired_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let _root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            transforms: BTreeMap::new(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-expired-lease".into()).unwrap();

        begin_run(&server_config, &nickname, &run_id).unwrap();

        let incoming_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        let lease_path = incoming_path.join("lease.toml");
        let mut lease: purgery_core::LeaseFile =
            toml::from_str(&fs::read_to_string(&lease_path).unwrap()).unwrap();
        lease.expires_at_unix_secs = 0;
        fs::write(&lease_path, toml::to_string(&lease).unwrap()).unwrap();

        let result = finish_run(&server_config, &nickname, &run_id);
        assert!(result.is_err(), "finish-run must reject expired lease");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expired") || err.contains("lease"),
            "error: {err}"
        );
    }

    #[test]
    fn test_finish_run_rejects_mismatched_lease_nickname() {
        let tmp = tempfile::tempdir().unwrap();
        let _root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            transforms: BTreeMap::new(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-wrong-nickname".into()).unwrap();

        begin_run(&server_config, &nickname, &run_id).unwrap();

        let incoming_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        let lease_path = incoming_path.join("lease.toml");
        let mut lease: purgery_core::LeaseFile =
            toml::from_str(&fs::read_to_string(&lease_path).unwrap()).unwrap();
        lease.nickname = "wrong-machine".into();
        fs::write(&lease_path, toml::to_string(&lease).unwrap()).unwrap();

        let result = finish_run(&server_config, &nickname, &run_id);
        assert!(
            result.is_err(),
            "finish-run must reject mismatched nickname"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nickname"), "error: {err}");
    }

    #[test]
    fn test_process_once_processes_ready_run_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ready-after-restart".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "documents",
            "a.txt",
            b"ready",
        );

        process_once_raw(&config).unwrap();

        assert!(config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done)
            .exists());
        assert_eq!(
            fs::read_to_string(
                test_storage_root(config.work_dir.as_path()).join("univ/documents/a.txt")
            )
            .unwrap(),
            "ready"
        );
    }

    #[test]
    fn test_process_once_recovers_processing_run_without_status() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-interrupted".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "documents",
            "a.txt",
            b"hello",
        );
        let ready = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.parent().unwrap()).unwrap();
        fs::rename(&ready, &processing).unwrap();
        let stale_work = purgery_core::work_dir(&config.work_dir, &nickname, &run_id);
        fs::create_dir_all(&stale_work).unwrap();
        fs::write(stale_work.join("stale"), b"stale").unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        let done = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done.join("status.toml").exists());
        assert_eq!(
            fs::read_to_string(
                test_storage_root(config.work_dir.as_path()).join("univ/documents/a.txt")
            )
            .unwrap(),
            "hello"
        );
        assert!(!stale_work.exists());
    }

    #[test]
    fn test_process_once_finalizes_processing_run_with_valid_status() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-status".into()).unwrap();
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        let status = RunStatus {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            state: RunState::Partial,
            entries: vec![],
            error: None,
        };
        fs::write(processing.join("status.toml"), status.to_toml().unwrap()).unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        assert!(config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done)
            .exists());
    }

    fn assert_mismatched_processing_status_fails(
        status_nickname: Nickname,
        status_run_id: RunId,
        directory_run_id: &str,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new(directory_run_id.into()).unwrap();
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        let status = RunStatus {
            purgery_version: "0.1.0-test".to_string(),
            run_id: status_run_id,
            nickname: status_nickname,
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(processing.join("status.toml"), status.to_toml().unwrap()).unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        assert!(!config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done)
            .exists());
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let failed_status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert_eq!(failed_status.nickname, nickname);
        assert_eq!(failed_status.run_id, run_id);
        assert_eq!(failed_status.state, RunState::Failed);
        assert_eq!(
            failed_status.error.as_deref(),
            Some("interrupted processing had mismatched status envelope")
        );
    }

    #[test]
    fn test_process_once_fails_processing_run_with_mismatched_status_nickname() {
        assert_mismatched_processing_status_fails(
            Nickname::new("other-machine".into()).unwrap(),
            RunId::new("recover-wrong-nickname".into()).unwrap(),
            "recover-wrong-nickname",
        );
    }

    #[test]
    fn test_process_once_fails_processing_run_with_mismatched_status_run_id() {
        assert_mismatched_processing_status_fails(
            Nickname::new("laptop".into()).unwrap(),
            RunId::new("other-run".into()).unwrap(),
            "recover-wrong-run-id",
        );
    }

    #[test]
    fn test_mismatched_status_recovery_propagates_terminal_move_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("blocked-failed-move".into()).unwrap();
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(&failed).unwrap();
        fs::write(failed.join("existing"), b"occupied").unwrap();
        let mismatched_status = RunStatus {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("other-run".into()).unwrap(),
            nickname: nickname.clone(),
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(
            processing.join("status.toml"),
            mismatched_status.to_toml().unwrap(),
        )
        .unwrap();

        let error = recover_or_process_processing_run(&config, &nickname, &run_id).unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to move run-level failure to failed"));
        assert!(processing.exists());
        let status =
            RunStatus::from_toml(&fs::read_to_string(processing.join("status.toml")).unwrap())
                .unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("interrupted processing had mismatched status envelope")
        );
    }

    #[test]
    fn test_malformed_status_recovery_propagates_failed_status_write_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("blocked-status-write".into()).unwrap();
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.join("status.toml.tmp")).unwrap();
        fs::write(processing.join("status.toml"), "not valid = [toml").unwrap();

        let error = recover_or_process_processing_run(&config, &nickname, &run_id).unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to write temporary run failure status"));
        assert!(processing.exists());
        assert_eq!(
            fs::read_to_string(processing.join("status.toml")).unwrap(),
            "not valid = [toml"
        );
    }

    #[test]
    fn test_process_once_fails_processing_run_with_malformed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-malformed".into()).unwrap();
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        fs::write(processing.join("status.toml"), "not valid = [toml").unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status
            .error
            .as_deref()
            .unwrap_or("")
            .contains("malformed status"));
    }

    #[test]
    fn test_replay_after_final_replacement_without_status_converges() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-committed-output".into()).unwrap();
        let (config, _) =
            setup_single_file_ready(&work_dir, &nickname, &run_id, "documents", "a.txt", b"new");
        let ready = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.parent().unwrap()).unwrap();
        fs::rename(&ready, &processing).unwrap();
        let final_path = _server_root.join("documents/a.txt");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"new").unwrap();
        assert!(!processing.join("status.toml").exists());

        process_once_raw(&config).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "new");
        let done = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Done);
    }

    #[test]
    fn test_repeated_imports_same_destination_are_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();

        for (run, content) in [("repeat-1", b"hello".as_slice()), ("repeat-2", b"hello")] {
            let run_id = RunId::new(run.into()).unwrap();
            let (config, _) = setup_single_file_ready(
                &work_dir,
                &nickname,
                &run_id,
                "documents",
                "a.txt",
                content,
            );
            process_run(&config, &nickname, &run_id).unwrap();
            assert!(config
                .work_dir
                .run_dir(&nickname, &run_id, RunPhase::Done)
                .exists());
        }

        assert_eq!(
            fs::read_to_string(test_storage_root(&work_dir).join("univ/documents/a.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn test_repeated_import_replaces_changed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();

        for (run, content) in [("version-1", b"v1".as_slice()), ("version-2", b"v2")] {
            let run_id = RunId::new(run.into()).unwrap();
            let (config, _) = setup_single_file_ready(
                &work_dir,
                &nickname,
                &run_id,
                "documents",
                "a.txt",
                content,
            );
            process_run(&config, &nickname, &run_id).unwrap();
        }

        assert_eq!(
            fs::read_to_string(test_storage_root(&work_dir).join("univ/documents/a.txt")).unwrap(),
            "v2"
        );
    }

    #[test]
    fn test_gc_collects_abandoned_incoming_upload() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("abandoned-upload".into()).unwrap();
        begin_run(&config, &nickname, &run_id).unwrap();
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(incoming.join("files/univ")).unwrap();
        fs::write(incoming.join("files/partial.txt"), b"partial").unwrap();
        let lease_path = incoming.join("lease.toml");
        let mut lease: purgery_core::LeaseFile =
            toml::from_str(&fs::read_to_string(&lease_path).unwrap()).unwrap();
        lease.expires_at_unix_secs = 0;
        fs::write(&lease_path, toml::to_string(&lease).unwrap()).unwrap();

        run_gc(&config).unwrap();

        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.join("files").exists());
        let status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Failed);
    }

    /// begin-run output must be parseable as BeginRunResponse TOML.
    /// This is a stdout-clean invariant: protocol output must never be
    /// contaminated by log output, and the returned string must always
    /// be valid TOML regardless of logging configuration.
    #[test]
    fn test_begin_run_stdout_is_parseable_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let _root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            transforms: BTreeMap::new(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-stdout-begin".into()).unwrap();

        let response_str = begin_run(&server_config, &nickname, &run_id).unwrap();
        // Must parse as BeginRunResponse — if logging contaminated stdout,
        // this parse would fail.
        let response: purgery_core::BeginRunResponse = toml::from_str(&response_str)
            .expect("begin-run stdout must be valid BeginRunResponse TOML");
        assert_eq!(response.protocol_version, 1);
        assert_eq!(response.nickname, "laptop");
        assert_eq!(response.run_id, "test-stdout-begin");
    }

    /// status output must be parseable as RunStatus TOML.
    #[test]
    fn test_status_stdout_is_parseable_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-stdout-status".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &work_dir, &nickname, &run_id, "videos", "test.mp4", b"hello",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let status = read_run_status(&config, &nickname, &run_id).unwrap();
        let status_str = status.to_toml().unwrap();
        // Must parse back as RunStatus
        let parsed: purgery_core::RunStatus = purgery_core::RunStatus::from_toml(&status_str)
            .expect("status stdout must be valid RunStatus TOML");
        assert_eq!(parsed.state, purgery_core::RunState::Done);
    }

    #[test]
    fn test_rsync_oracle_directory_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();

        let missing = root.join("missing");
        assert_eq!(
            commit_directory_entry(&missing, &root).unwrap(),
            CommitDisposition::Created
        );

        let existing = root.join("existing");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("extra"), "keep").unwrap();
        assert_eq!(
            commit_directory_entry(&existing, &root).unwrap(),
            CommitDisposition::Kept
        );
        assert_eq!(fs::read_to_string(existing.join("extra")).unwrap(), "keep");

        let file = root.join("file");
        fs::write(&file, "old").unwrap();
        assert_eq!(
            commit_directory_entry(&file, &root).unwrap(),
            CommitDisposition::Replaced
        );
        assert!(file.is_dir());

        let symlink = root.join("symlink");
        std::os::unix::fs::symlink("elsewhere", &symlink).unwrap();
        assert_eq!(
            commit_directory_entry(&symlink, &root).unwrap(),
            CommitDisposition::Replaced
        );
        assert!(symlink.is_dir());
    }

    #[test]
    fn test_rsync_oracle_regular_file_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let source = Utf8PathBuf::from_path_buf(tmp.path().join("source")).unwrap();
        let run_id = RunId::new("oracle-file".into()).unwrap();

        for name in ["missing", "file", "symlink", "empty-dir"] {
            fs::write(&source, "new content").unwrap();
            let destination = root.join(name);
            match name {
                "file" => fs::write(&destination, "old").unwrap(),
                "symlink" => std::os::unix::fs::symlink("target", &destination).unwrap(),
                "empty-dir" => fs::create_dir(&destination).unwrap(),
                _ => {}
            }
            commit_regular_file_entry(&source, &destination, &root, &run_id).unwrap();
            assert_eq!(fs::read_to_string(&destination).unwrap(), "new content");
            assert!(!fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink());
        }

        fs::write(&source, "new content").unwrap();
        let nonempty = root.join("nonempty-dir");
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("extra"), "keep").unwrap();
        assert!(commit_regular_file_entry(&source, &nonempty, &root, &run_id).is_err());
        assert_eq!(fs::read_to_string(nonempty.join("extra")).unwrap(), "keep");
    }

    #[test]
    fn test_rsync_oracle_symlink_conflicts_and_literal_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&work_source).unwrap();
        let run_id = RunId::new("oracle-link".into()).unwrap();
        let link_target = Utf8Path::new("../literal-target");

        for name in ["missing", "file", "symlink", "empty-dir"] {
            let destination = root.join(name);
            let source = work_source.join(format!("source-{name}"));
            std::os::unix::fs::symlink(link_target.as_std_path(), &source).unwrap();
            match name {
                "file" => fs::write(&destination, "old").unwrap(),
                "symlink" => std::os::unix::fs::symlink("old-target", &destination).unwrap(),
                "empty-dir" => fs::create_dir(&destination).unwrap(),
                _ => {}
            }
            commit_symlink_entry(&source, &destination, &root, &run_id).unwrap();
            assert_eq!(
                fs::read_link(&destination).unwrap(),
                link_target.as_std_path()
            );
        }

        let nonempty = root.join("nonempty-dir");
        let source_nonempty = work_source.join("source-nonempty");
        std::os::unix::fs::symlink(link_target.as_std_path(), &source_nonempty).unwrap();
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("extra"), "keep").unwrap();
        assert!(commit_symlink_entry(&source_nonempty, &nonempty, &root, &run_id).is_err());
        assert_eq!(fs::read_to_string(nonempty.join("extra")).unwrap(), "keep");
    }

    #[test]
    fn test_rsync_oracle_parent_conflicts_are_resolved_by_directory_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let source = Utf8PathBuf::from_path_buf(tmp.path().join("source")).unwrap();
        let run_id = RunId::new("oracle-parent".into()).unwrap();

        for name in ["file-parent", "symlink-parent"] {
            fs::write(&source, "child").unwrap();
            let parent = root.join(name);
            if name == "file-parent" {
                fs::write(&parent, "old").unwrap();
            } else {
                std::os::unix::fs::symlink("elsewhere", &parent).unwrap();
            }
            commit_directory_entry(&parent, &root).unwrap();
            let child = parent.join("child");
            commit_regular_file_entry(&source, &child, &root, &run_id).unwrap();
            assert_eq!(fs::read_to_string(child).unwrap(), "child");
        }
    }

    #[test]
    fn test_process_run_overlays_directory_file_and_symlink_without_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("tree-overlay".into()).unwrap();
        let ready = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        let staged = ready.join("files/tree");
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("new.txt"), "new").unwrap();
        std::os::unix::fs::symlink("../target", staged.join("link")).unwrap();
        write_run_toml_with_destination(&ready, &nickname, "data");

        let entry = |relative: &str, kind, size, target: Option<&str>| ManifestEntry {
            local_path: ClientLocalPath::new(format!("/source/{relative}")).unwrap(),
            staged_path: NormalizedRelativePath::new(format!("files/{relative}").into()).unwrap(),
            relative_path: NormalizedRelativePath::new(relative.into()).unwrap(),
            kind,
            size,
            mtime_ns: 0,
            sha256: None,
            link_target: target.map(Utf8PathBuf::from),
            transform: None,
        };
        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                entry("tree", ManifestEntryKind::Directory, 0, None),
                entry(
                    "tree/link",
                    ManifestEntryKind::Symlink,
                    0,
                    Some("../target"),
                ),
                entry("tree/new.txt", ManifestEntryKind::RegularFile, 3, None),
            ],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let final_tree = test_storage_root(config.work_dir.as_path()).join("univ/data/tree");
        fs::create_dir_all(&final_tree).unwrap();
        fs::write(final_tree.join("extra.txt"), "keep").unwrap();
        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(
            fs::read_to_string(final_tree.join("new.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(final_tree.join("extra.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::read_link(final_tree.join("link")).unwrap(),
            std::path::Path::new("../target")
        );
        let done = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 3);
        assert_eq!(status.entries[0].kind, ManifestEntryKind::Directory);
        assert_eq!(status.entries[1].kind, ManifestEntryKind::Symlink);
        assert_eq!(status.entries[2].kind, ManifestEntryKind::RegularFile);
    }

    #[test]
    fn test_read_run_status_rejects_mismatched_terminal_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("requested".into()).unwrap();
        let done = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        fs::create_dir_all(&done).unwrap();
        let status = RunStatus {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("different".into()).unwrap(),
            nickname: nickname.clone(),
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(done.join("status.toml"), status.to_toml().unwrap()).unwrap();

        let error = read_run_status(&config, &nickname, &run_id).unwrap_err();
        assert!(error.to_string().contains("status envelope mismatch"));
    }

    fn expected_output_test_plan() -> ServerConfig {
        ServerConfig {
            work_dir: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "generate",
                TransformDefinition {
                    name: "generate".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".into(),
                    args: vec![],
                    expected_outputs: vec!["{stem}.out".into()],
                },
            ),
            logging: Default::default(),
        }
    }

    #[test]
    fn transform_regular_expected_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        fs::write(work_path.with_file_name("input.out"), "output").unwrap();

        let outputs = test_apply_transform(&expected_output_test_plan(), &work_path).unwrap();
        assert_eq!(outputs, vec![work_path.with_file_name("input.out")]);
    }

    #[test]
    fn transform_missing_expected_output_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();

        let error = test_apply_transform(&expected_output_test_plan(), &work_path).unwrap_err();
        assert!(error.contains("expected output not found"));
    }

    #[test]
    fn transform_symlink_expected_output_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        let target = work_path.with_file_name("target.txt");
        fs::write(&target, "secret target contents").unwrap();
        // Place a symlink to the target as the expected output.  The symlink
        // itself must be accepted — Purgery must not follow or reject it.
        std::os::unix::fs::symlink(&target, work_path.with_file_name("input.out")).unwrap();

        let outputs = test_apply_transform(&expected_output_test_plan(), &work_path).unwrap();
        assert!(
            outputs.contains(&work_path.with_file_name("input.out")),
            "symlink expected output must be accepted"
        );
        // The symlink must still point to the original target (not be
        // replaced by the target's content).
        let link = fs::read_link(work_path.with_file_name("input.out")).unwrap();
        assert_eq!(
            link,
            target.as_std_path(),
            "symlink target must be preserved"
        );
    }

    #[test]
    fn transform_directory_expected_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        fs::create_dir(work_path.with_file_name("input.out")).unwrap();

        let outputs = test_apply_transform(&expected_output_test_plan(), &work_path).unwrap();
        assert!(outputs.contains(&work_path.with_file_name("input.out")));
    }

    #[test]
    fn transform_symlink_expected_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        std::os::unix::fs::symlink("some-target", work_path.with_file_name("input.out")).unwrap();

        let outputs = test_apply_transform(&expected_output_test_plan(), &work_path).unwrap();
        assert!(outputs.contains(&work_path.with_file_name("input.out")));
    }

    #[test]
    fn transform_fifo_expected_output_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        // Create a FIFO (named pipe)
        std::process::Command::new("mkfifo")
            .arg(work_path.with_file_name("input.out").as_std_path())
            .status()
            .unwrap();

        let error = test_apply_transform(&expected_output_test_plan(), &work_path).unwrap_err();
        assert!(error.contains("expected output is not a supported entry type"));
    }

    #[test]
    fn prepare_run_rewrites_relative_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let mut config = test_server_config(&work_dir);
        config.transforms.insert(
            "test-step".into(),
            TransformDefinition {
                name: "test-step".into(),
                kind: TransformKind::Subprocess,
                program: "/bin/true".to_string(),
                args: Vec::new(),
                expected_outputs: Vec::new(),
            },
        );
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-relative-dest".into()).unwrap();
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        write_run_toml_with_raw_destination(&incoming, &nickname, "relative/path");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/source/file.txt".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/file.txt".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("file.txt".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 0,
                sha256: None,
                link_target: None,

                transform: Some("test-step".into()),
            }],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let result = prepare_run(&config, &nickname, &run_id);
        assert!(result.is_ok(), "prepare_run must succeed");

        let response_str = result.unwrap();
        let response: purgery_core::PrepareRunResponse = toml::from_str(&response_str).unwrap();
        assert!(
            response.destination.is_some(),
            "relative destination must produce a resolved destination in response"
        );
        let resolved = response.destination.unwrap();
        assert!(
            resolved.starts_with('/'),
            "resolved destination must be absolute, got: {resolved}"
        );
        assert!(
            resolved.ends_with("relative/path"),
            "resolved destination must end with the original relative path, got: {resolved}"
        );

        // run.toml must be atomically rewritten with absolute path.
        let run_config_content = fs::read_to_string(incoming.join("run.toml")).unwrap();
        let run_config: purgery_core::RunConfig = toml::from_str(&run_config_content).unwrap();
        assert!(
            run_config.destination.is_absolute(),
            "rewritten run.toml destination must be absolute"
        );
    }

    #[test]
    fn prepare_run_does_not_rewrite_absolute_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let mut config = test_server_config(&work_dir);
        config.transforms.insert(
            "test-step".into(),
            TransformDefinition {
                name: "test-step".into(),
                kind: TransformKind::Subprocess,
                program: "/bin/true".to_string(),
                args: Vec::new(),
                expected_outputs: Vec::new(),
            },
        );
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-absolute-dest".into()).unwrap();
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        write_run_toml_with_destination(&incoming, &nickname, "absolute/dest");
        let run_config_content = fs::read_to_string(incoming.join("run.toml")).unwrap();
        let run_config: purgery_core::RunConfig = toml::from_str(&run_config_content).unwrap();
        assert!(run_config.destination.is_absolute());
        let original_dest = run_config.destination.as_str().to_owned();

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/source/file.txt".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/file.txt".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("file.txt".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 0,
                sha256: None,
                link_target: None,

                transform: Some("test-step".into()),
            }],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let result = prepare_run(&config, &nickname, &run_id);
        assert!(result.is_ok(), "prepare_run must succeed");

        let response_str = result.unwrap();
        let response: purgery_core::PrepareRunResponse = toml::from_str(&response_str).unwrap();
        assert!(
            response.destination.is_none(),
            "absolute destination must not produce a resolved destination in response"
        );

        // run.toml must be unchanged.
        let final_content = fs::read_to_string(incoming.join("run.toml")).unwrap();
        let final_run_config: purgery_core::RunConfig = toml::from_str(&final_content).unwrap();
        assert_eq!(final_run_config.destination.as_str(), original_dest);
    }

    #[test]
    fn out_of_scope_rule_does_not_process_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = ServerConfig {
            transforms: single_transform(
                "pack",
                TransformDefinition {
                    name: "pack".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".into(),
                    args: vec![],
                    expected_outputs: vec![],
                },
            ),
            ..test_server_config(&work_dir)
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("scoped-processing".into()).unwrap();
        let ready = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);

        // videos/ has a matching file pattern, but the rule is scoped to "pictures"
        fs::create_dir_all(ready.join("files")).unwrap();
        fs::write(ready.join("files/a.mp4"), b"video").unwrap();
        write_run_toml_with_destination(&ready, &nickname, "univ/videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/src/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 100,
                sha256: None,
                link_target: None,

                transform: None,
            }],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        // process_run must succeed — the rule is out of scope for videos
        process_run(&config, &nickname, &run_id).unwrap();
        let done = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        // videos/a.mp4 must be imported as passthrough, not processed by pack
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert!(status.entries[0].transform.is_none());
    }

    // ── Progress context tests ──

    #[test]
    fn processing_progress_has_real_entry_context() {
        // Use direct write_progress to assert entry context is preserved.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        write_progress(&path, &n, &r, "processing_entry", 1, 3, "b.txt", "").unwrap();
        let content = fs::read_to_string(path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(p.state, "processing_entry");
        assert_eq!(p.entry_index, 1);
        assert_eq!(p.entry_total, 3);
        assert_eq!(p.current_entry, "b.txt");
        assert_eq!(p.current_transform, "");
    }

    #[test]
    fn publishing_status_progress_is_written() {
        // Use direct write_progress to assert publishing_status fields.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        write_progress(&path, &n, &r, "publishing_status", 0, 1, "", "").unwrap();
        let content = fs::read_to_string(path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(p.state, "publishing_status");
        assert_eq!(p.entry_index, 0);
        assert_eq!(p.entry_total, 1);
        assert!(p.current_entry.is_empty());
        assert!(p.current_transform.is_empty());
    }

    // ── Progress sentinel and write-failure tests ──

    #[test]
    fn progress_update_has_no_sentinel_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, b"input").unwrap();
        let compressed = work_path.with_file_name("input.out");
        fs::write(&compressed, b"output").unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress",
                TransformDefinition {
                    name: "compress".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".into(),
                    args: vec![],
                    expected_outputs: vec!["{stem}.out".into()],
                },
            ),
            logging: Default::default(),
        };

        let captured = std::sync::Mutex::new(Vec::new());
        let mut callback = |update: &purgery_core::ProgressUpdate| {
            captured.lock().unwrap().push((
                update.state.to_owned(),
                update.entry_index,
                update.entry_total,
                update.current_entry.to_owned(),
                update.current_transform.to_owned(),
            ));
        };

        let (name, _) = server_config
            .transforms
            .iter()
            .next()
            .expect("test plan must have at least one transform");
        let resolved = ResolvedTransform {
            name: name.clone(),
            def: server_config.transforms[name].clone(),
        };
        let parent = work_path.parent().unwrap();
        apply_transform_with_heartbeat(
            &resolved,
            &work_path,
            parent,
            parent,
            std::time::Duration::from_millis(1),
            &mut callback,
            0,
            1,
            "data/input.txt",
        )
        .expect("transforms must succeed");

        let updates = captured.lock().unwrap();
        assert!(
            !updates.is_empty(),
            "must have captured at least one progress update"
        );
        for (state, _ei, et, _ce, _cs) in updates.iter() {
            assert!(
                *et > 0,
                "transform '{state}' must have entry_total > 0, got {et}"
            );
        }
    }

    #[test]
    fn progress_write_failure_does_not_fail_import() {
        let tmp = tempfile::tempdir().unwrap();
        let _work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("progress-fail".into()).unwrap();

        // Use a ready run with one file
        let ready_path = Utf8PathBuf::from_path_buf(tmp.path().join("purgery"))
            .unwrap()
            .join("laptop")
            .join("ready")
            .join(run_id.as_str());
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/file.txt"), b"content").unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "univ/data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/src/file.txt".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/file.txt".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("file.txt".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 7,
                mtime_ns: 100,
                sha256: None,
                link_target: None,

                transform: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let config =
            test_server_config(&Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap());

        // Move from ready to processing
        let processing_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing_path.parent().unwrap()).unwrap();
        fs::rename(&ready_path, &processing_path).unwrap();

        // Pre-create a directory at the progress temp path so the progress write fails
        // (fs::write to an existing directory fails on Unix).
        // This simulates a progress write failure without blocking status writes.
        let progress_tmp = processing_path.join("progress.toml.tmp");
        fs::create_dir(&progress_tmp).unwrap();

        // Processing must still succeed despite progress write failure
        let result = process_processing_run(&config, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "import must succeed even with progress write failure: {:?}",
            result.err()
        );

        // The final file must exist
        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/data/file.txt");
        assert_eq!(
            fs::read_to_string(&final_path).unwrap(),
            "content",
            "file must be imported despite progress failure"
        );
    }

    // ── Publishing status and per-entry progress tests ──

    #[test]
    fn publishing_status_is_run_level_progress() {
        // publishing_status is tested via direct write_progress above.
        // This test is kept for documentation: run-level progress has empty
        // current_entry and current_transform and coherent entry_total.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        write_progress(&path, &n, &r, "publishing_status", 0, 10, "", "").unwrap();
        let content = fs::read_to_string(path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(p.state, "publishing_status");
        assert_eq!(p.entry_index, 0);
        assert_eq!(p.entry_total, 10, "entry_total must be coherent");
        assert!(
            p.current_entry.is_empty() && p.current_transform.is_empty(),
            "run-level progress must have empty entry/step"
        );
    }

    #[test]
    fn per_entry_progress_has_real_context() {
        // Use a progress callback capture to verify entry context is propagated
        // through the transform pipeline.
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, b"input").unwrap();
        let compressed = work_path.with_file_name("input.out");
        fs::write(&compressed, b"output").unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress",
                TransformDefinition {
                    name: "compress".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".into(),
                    args: vec![],
                    expected_outputs: vec!["{stem}.out".into()],
                },
            ),
            logging: Default::default(),
        };

        let captured = std::sync::Mutex::new(Vec::new());
        let mut callback = |update: &purgery_core::ProgressUpdate| {
            captured.lock().unwrap().push((
                update.state.to_owned(),
                update.entry_index,
                update.entry_total,
                update.current_entry.to_owned(),
                update.current_transform.to_owned(),
            ));
        };

        let (name, _) = server_config
            .transforms
            .iter()
            .next()
            .expect("test plan must have at least one transform");
        let resolved = ResolvedTransform {
            name: name.clone(),
            def: server_config.transforms[name].clone(),
        };
        let parent = work_path.parent().unwrap();
        apply_transform_with_heartbeat(
            &resolved,
            &work_path,
            parent,
            parent,
            std::time::Duration::from_millis(5),
            &mut callback,
            42,
            99,
            "test-entry.txt",
        )
        .unwrap();

        let updates = captured.lock().unwrap();
        assert!(!updates.is_empty(), "must have captured progress updates");
        for (state, ei, et, ce, cs) in updates.iter() {
            assert_eq!(*ei, 42, "transform '{state}' must pass entry_index through");
            assert_eq!(*et, 99, "transform '{state}' must pass entry_total through");
            assert_eq!(
                ce.as_str(),
                "test-entry.txt",
                "transform '{state}' must pass current_entry"
            );
            assert!(
                !cs.is_empty(),
                "transform '{state}' must have current_transform"
            );
        }
    }
    // ── Entry index and progress invariant tests ──

    #[test]
    fn progress_tests_do_not_ignore_transform_result() {
        // Regression guard: progress tests must not discard the result of
        // apply_transform_with_heartbeat with let _ = .
        let source = include_str!("lib.rs");
        // Check each line for the bad pattern, skipping this test's own assertion text.
        for (lineno, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed == "let _ = apply_transform_with_heartbeat(" {
                panic!(
                    "line {}: progress tests must not ignore apply_transform_with_heartbeat results;\n\
                     use .unwrap() or .expect() instead",
                    lineno + 1
                );
            }
        }
    }

    #[test]
    fn per_entry_first_entry_allows_index_zero() {
        // A manifest with at least one transformed entry should have
        // transform_started/transform_running/transform_finished with entry_index=0 for
        // the first entry, entry_total>0, and current_entry!="".
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, b"input").unwrap();
        fs::write(work_path.with_file_name("input.out"), b"output").unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress",
                TransformDefinition {
                    name: "compress".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".into(),
                    args: vec![],
                    expected_outputs: vec!["{stem}.out".into()],
                },
            ),
            logging: Default::default(),
        };

        let captured = std::sync::Mutex::new(Vec::new());
        let mut callback = |update: &purgery_core::ProgressUpdate| {
            captured.lock().unwrap().push((
                update.state.to_owned(),
                update.entry_index,
                update.entry_total,
                update.current_entry.to_owned(),
                update.current_transform.to_owned(),
            ));
        };

        let (name, _) = server_config
            .transforms
            .iter()
            .next()
            .expect("test plan must have at least one transform");
        let resolved = ResolvedTransform {
            name: name.clone(),
            def: server_config.transforms[name].clone(),
        };
        let parent = work_path.parent().unwrap();
        apply_transform_with_heartbeat(
            &resolved,
            &work_path,
            parent,
            parent,
            std::time::Duration::from_millis(1),
            &mut callback,
            0,
            1,
            "data/input.txt",
        )
        .expect("transforms must succeed");

        let updates = captured.lock().unwrap();
        for (state, ei, et, ce, _cs) in updates.iter() {
            match state.as_str() {
                "transform_started" | "transform_running" | "transform_finished" => {
                    // Per-entry invariants
                    assert!(
                        *et > 0,
                        "entry_total must be > 0 for per-entry state '{state}'"
                    );
                    assert!(
                        !ce.is_empty(),
                        "current_entry must be non-empty for per-entry state '{state}'"
                    );
                    assert!(ei < et, "entry_index ({ei}) must be < entry_total ({et})");
                    // entry_index = 0 is valid for the first entry
                    if *ei == 0 {
                        // This is fine — 0 is the valid index for the first entry.
                        // The test exists to confirm we don't reject it.
                    }
                }
                _ => {}
            }
        }
        // At least some step updates must have occurred
        assert!(
            updates.iter().any(|(s, _, _, _, _)| matches!(
                s.as_str(),
                "transform_started" | "transform_running" | "transform_finished"
            )),
            "must have at least one transform progress update"
        );
    }

    // ── Progress validation tests ──
    #[test]
    fn per_entry_progress_rejects_entry_total_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "transform_started", 0, 0, "a.txt", "c");
        assert!(result.is_err(), "entry_total=0 must be rejected");
    }

    #[test]
    fn per_entry_progress_rejects_empty_current_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "transform_started", 0, 1, "", "c");
        assert!(result.is_err(), "empty current_entry must be rejected");
    }

    #[test]
    fn per_entry_transform_progress_rejects_empty_current_transform() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "transform_started", 0, 1, "a.txt", "");
        assert!(
            result.is_err(),
            "transform state with empty current_transform must be rejected"
        );
    }

    #[test]
    fn per_entry_progress_rejects_index_out_of_range() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "transform_running", 5, 1, "a.txt", "c");
        assert!(
            result.is_err(),
            "entry_index >= entry_total must be rejected"
        );
    }

    #[test]
    fn processing_entry_rejects_empty_current_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "processing_entry", 0, 1, "", "");
        assert!(
            result.is_err(),
            "processing_entry with empty current_entry must be rejected"
        );
    }

    #[test]
    fn processing_entry_with_empty_transform_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "processing_entry", 0, 1, "a.txt", "");
        assert!(
            result.is_ok(),
            "processing_entry with empty step must succeed"
        );
    }

    #[test]
    fn per_entry_first_entry_index_zero_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "transform_started", 0, 1, "a.txt", "c");
        assert!(result.is_ok(), "entry_index=0 for first entry must succeed");
    }

    #[test]
    fn run_level_progress_with_empty_fields_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        let r1 = write_progress(&path, &n, &r, "processing_started", 0, 2, "", "");
        assert!(
            r1.is_ok(),
            "processing_started with empty fields must succeed"
        );

        let r2 = write_progress(&path, &n, &r, "publishing_status", 0, 2, "", "");
        assert!(
            r2.is_ok(),
            "publishing_status with empty fields must succeed"
        );
    }

    #[test]
    fn unknown_progress_state_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "nonsense", 0, 1, "a.txt", "c");
        assert!(result.is_err(), "unknown state must be rejected");
    }

    #[test]
    fn write_progress_best_effort_with_invalid_state_does_not_panic() {
        // Even with invalid progress, write_progress_best_effort must
        // log a warning and continue, not panic or propagate the error.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        // Invalid: entry_total=0 for per-entry
        write_progress_best_effort(&path, &n, &r, "transform_started", 0, 0, "a.txt", "c");

        // Invalid: unknown state
        write_progress_best_effort(&path, &n, &r, "nonsense", 0, 1, "a.txt", "c");

        // These should not panic. The function logs a warning and returns.
    }

    #[test]
    fn invalid_best_effort_progress_does_not_clobber_valid_progress() {
        // Invalid best-effort progress must not overwrite an existing valid
        // progress.toml because validation rejects the write before any I/O.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        // First, write a valid progress file
        write_progress(&path, &n, &r, "processing_entry", 0, 2, "a.txt", "").unwrap();
        let valid_content = fs::read_to_string(path.join("progress.toml")).unwrap();

        // Now call best-effort with invalid progress that doesn't clobber
        write_progress_best_effort(&path, &n, &r, "transform_started", 0, 0, "a.txt", "c");

        // The file content must be unchanged
        let after_content = fs::read_to_string(path.join("progress.toml")).unwrap();
        assert_eq!(
            after_content, valid_content,
            "invalid best-effort progress must not clobber valid progress file"
        );

        // Also test with unknown state
        write_progress_best_effort(&path, &n, &r, "nonsense", 0, 1, "a.txt", "c");
        let after2 = fs::read_to_string(path.join("progress.toml")).unwrap();
        assert_eq!(
            after2, valid_content,
            "unknown state best-effort must not clobber valid progress"
        );
    }

    #[test]
    fn run_level_progress_may_have_empty_current_entry() {
        // processing_started and publishing_status are run-level.
        // They may have empty current_entry/current_transform.
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("run-level".into()).unwrap();

        // Write processing_started (run-level)
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_started",
            0,
            2,
            "",
            "",
        )
        .unwrap();
        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(p.state, "processing_started");
        assert!(
            p.current_entry.is_empty() && p.current_transform.is_empty(),
            "run-level progress may have empty entry/step"
        );
        assert_eq!(p.entry_total, 2, "run-level progress still has entry_total");
        assert_eq!(p.entry_index, 0, "run-level progress entry_index is 0");
    }

    // ── Progress start-time preservation tests ──

    #[test]
    fn progress_start_time_preserved_only_when_envelope_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-envelope".into()).unwrap();

        // Write an existing progress file with matching envelope
        let old_progress = purgery_core::ProcessingProgress {
            protocol_version: 1,
            purgery_version: "0.1.0-test".to_string(),
            nickname: "laptop".into(),
            run_id: "ts-envelope".into(),
            phase: "processing".into(),
            state: "old".into(),
            entry_index: 0,
            entry_total: 1,
            current_entry: String::new(),
            current_transform: String::new(),
            started_at_unix_secs: 5000,
            updated_at_unix_secs: 5000,
        };
        let old_content = toml::to_string(&old_progress).unwrap();
        fs::write(processing_path.join("progress.toml"), &old_content).unwrap();

        // Matching envelope — started_at should be preserved
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            1,
            "a.txt",
            "",
        )
        .unwrap();
        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(
            p.started_at_unix_secs, 5000,
            "matching envelope must preserve started_at"
        );
    }

    #[test]
    fn progress_start_time_not_preserved_with_mismatched_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-mismatch".into()).unwrap();

        // Write existing progress with DIFFERENT nickname
        let old_progress = purgery_core::ProcessingProgress {
            protocol_version: 1,
            purgery_version: "0.1.0-test".to_string(),
            nickname: "other-machine".into(), // different nickname
            run_id: "ts-mismatch".into(),
            phase: "processing".into(),
            state: "old".into(),
            entry_index: 0,
            entry_total: 1,
            current_entry: String::new(),
            current_transform: String::new(),
            started_at_unix_secs: 5000,
            updated_at_unix_secs: 5000,
        };
        let old_content = toml::to_string(&old_progress).unwrap();
        fs::write(processing_path.join("progress.toml"), &old_content).unwrap();

        // Mismatched envelope — started_at must NOT be preserved (should be fresh)
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            1,
            "a.txt",
            "",
        )
        .unwrap();
        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert!(
            p.started_at_unix_secs >= before,
            "mismatched envelope must initialize fresh started_at, got {} < {}",
            p.started_at_unix_secs,
            before
        );
    }

    #[test]
    fn malformed_existing_progress_does_not_preserve_start_time() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-malformed".into()).unwrap();

        // Write invalid TOML to progress.toml
        fs::write(processing_path.join("progress.toml"), "not valid toml {{{").unwrap();

        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            1,
            "a.txt",
            "",
        )
        .unwrap();
        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert!(
            p.started_at_unix_secs >= before,
            "malformed existing file must initialize fresh started_at, got {} < {}",
            p.started_at_unix_secs,
            before
        );
    }

    // ── Progress timestamp tests ──

    #[test]
    fn progress_timestamp_started_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-stable".into()).unwrap();

        // First write
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_started",
            0,
            3,
            "",
            "",
        )
        .unwrap();
        let content1 = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p1: purgery_core::ProcessingProgress = toml::from_str(&content1).unwrap();
        let first_started = p1.started_at_unix_secs;
        let first_updated = p1.updated_at_unix_secs;

        // Second write (after a short sleep to advance time)
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            3,
            "a.txt",
            "",
        )
        .unwrap();
        let content2 = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p2: purgery_core::ProcessingProgress = toml::from_str(&content2).unwrap();

        // started_at must be stable
        assert_eq!(
            p2.started_at_unix_secs, first_started,
            "started_at must not change between progress writes"
        );
        // updated_at must be >= previous (could be same second in tests)
        assert!(
            p2.updated_at_unix_secs >= first_updated,
            "updated_at must not go backwards: {} < {}",
            p2.updated_at_unix_secs,
            first_updated
        );
    }

    #[test]
    fn progress_timestamp_existing_file_preserves_started_at() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-existing".into()).unwrap();

        // Manually write a progress file with a specific started_at
        let old_progress = purgery_core::ProcessingProgress {
            protocol_version: 1,
            purgery_version: "0.1.0-test".to_string(),
            nickname: "laptop".into(),
            run_id: run_id.as_str().into(),
            phase: "processing".into(),
            state: "old".into(),
            entry_index: 0,
            entry_total: 1,
            current_entry: String::new(),
            current_transform: String::new(),
            started_at_unix_secs: 1000,
            updated_at_unix_secs: 1000,
        };
        let old_content = toml::to_string(&old_progress).unwrap();
        fs::write(processing_path.join("progress.toml"), &old_content).unwrap();

        // Now write a new progress update
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            1,
            "a.txt",
            "",
        )
        .unwrap();

        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(
            p.started_at_unix_secs, 1000,
            "started_at must be preserved from existing file"
        );
        assert!(
            p.updated_at_unix_secs >= 1000,
            "updated_at must be >= existing value"
        );
    }

    #[test]
    fn progress_timestamp_first_write_initializes_both() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-first".into()).unwrap();

        // No progress file exists yet
        assert!(!processing_path.join("progress.toml").exists());

        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_started",
            0,
            2,
            "",
            "",
        )
        .unwrap();

        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        // Both fields must be set to reasonable values (> 0 means some time after epoch)
        assert!(
            p.started_at_unix_secs > 1000000000,
            "started_at must be set to current time on first write, got {}",
            p.started_at_unix_secs
        );
        assert!(
            p.updated_at_unix_secs > 1000000000,
            "updated_at must be set to current time on first write, got {}",
            p.updated_at_unix_secs
        );
        // They should be approximately equal (same write)
        assert!(
            p.updated_at_unix_secs >= p.started_at_unix_secs,
            "updated_at must be >= started_at"
        );
    }

    /// Spec and design docs must describe protocol, behavior, and invariants,
    /// not test process, development process, or agent workflow.
    #[test]
    fn spec_docs_do_not_contain_test_process_guidance() {
        let docs = [
            (
                "docs/protocol.md",
                include_str!("../../../docs/protocol.md"),
            ),
            (
                "docs/design/crash-safety-and-idempotence.md",
                include_str!("../../../docs/design/crash-safety-and-idempotence.md"),
            ),
        ];

        let banned = [
            "Tests must not",
            "tests must not",
            "expected-failing",
            "expected failure",
            "Do not switch branches",
            "agent",
        ];

        for (path, content) in docs {
            for phrase in banned {
                assert!(
                    !content.contains(phrase),
                    "{path} contains process/test guidance phrase: {phrase:?}"
                );
            }
        }
    }

    // ── Output-only final destination tests ──────────────────────────

    /// Collect every path recursively under a root directory.
    fn collect_all_paths_under(root: &Utf8Path) -> Vec<Utf8PathBuf> {
        let mut paths = Vec::new();
        let mut queue = vec![root.to_owned()];
        while let Some(dir) = queue.pop() {
            let entries = match std::fs::read_dir(dir.as_std_path()) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = Utf8PathBuf::from_path_buf(entry.path())
                    .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref()));
                if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                    queue.push(path.clone());
                }
                paths.push(path);
            }
        }
        paths.sort();
        paths
    }

    /// Assert that the tree under root contains exactly the expected paths.
    fn assert_root_contains_exactly(root: &Utf8Path, expected: &[Utf8PathBuf]) {
        let actual = collect_all_paths_under(root);
        let expected_sorted = {
            let mut v: Vec<_> = expected
                .iter()
                .filter(|path| {
                    path.starts_with(root)
                        && path
                            .strip_prefix(root)
                            .ok()
                            .and_then(|relative| relative.components().next())
                            .is_none_or(|component| component.as_str() != "univ")
                })
                .cloned()
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            actual, expected_sorted,
            "root paths mismatch.\nExpected: {expected_sorted:?}\nActual:   {actual:?}"
        );
    }

    #[test]
    fn regular_file_commit_must_not_create_operational_paths_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();

        let final_path = root.join("subdir/file.txt");
        let source = work_source.join("source.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&source, b"hello").unwrap();

        let result = commit_regular_file_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "regular file commit failed: {result:?}");
        assert!(final_path.exists());

        let expected = vec![root.join("subdir"), root.join("subdir/file.txt")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    fn regular_file_commit_allows_final_dotfile() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();

        let final_path = root.join(".hidden-file");
        let source = work_source.join("source.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::write(&source, b"secret").unwrap();

        let result = commit_regular_file_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "dotfile commit failed: {result:?}");
        assert!(final_path.exists());

        let expected = vec![root.join(".hidden-file")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_commit_must_not_create_operational_paths_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();

        fs::create_dir_all(&work_source).unwrap();
        let final_path = root.join("subdir/link");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        let source = work_source.join("srclink");
        std::os::unix::fs::symlink("/some/target", &source).unwrap();

        let result = commit_symlink_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "symlink commit failed: {result:?}");
        assert!(
            std::fs::symlink_metadata(final_path.as_std_path()).is_ok(),
            "symlink was not created at final_path"
        );

        let expected = vec![root.join("subdir"), root.join("subdir/link")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    #[cfg(unix)]
    fn directory_tree_commit_must_not_create_operational_paths_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();

        let source_dir = work_source.join("srcdir");
        let final_dir = root.join("dst");

        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("a.txt"), b"content a").unwrap();
        fs::write(source_dir.join("b.txt"), b"content b").unwrap();
        std::os::unix::fs::symlink("/tmp/target", source_dir.join("c").as_std_path()).unwrap();
        fs::create_dir_all(final_dir.parent().unwrap()).unwrap();

        let result = commit_directory_tree(&source_dir, &final_dir, root.as_path(), &run_id);
        assert!(result.is_ok(), "directory tree commit failed: {result:?}");

        assert!(final_dir.exists());
        assert!(final_dir.join("a.txt").exists());
        assert!(final_dir.join("b.txt").exists());

        let expected = vec![
            root.join("dst"),
            root.join("dst/a.txt"),
            root.join("dst/b.txt"),
            root.join("dst/c"),
        ];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    fn full_processing_run_leaves_only_expected_paths_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-final-only".into()).unwrap();

        let (config, _staged) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "videos",
            "test.mp4",
            b"hello world",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let expected = vec![
            config.work_dir.as_path().join("univ"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4"),
            config.work_dir.as_path().join("laptop"),
            config.work_dir.as_path().join("laptop/done"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-run-final-only"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-run-final-only/files"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-run-final-only/files/test.mp4"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-run-final-only/manifest.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-run-final-only/progress.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-run-final-only/run.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-run-final-only/status.toml"),
            config.work_dir.as_path().join("laptop/processing"),
            config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(config.work_dir.as_path(), &expected);
    }

    #[test]
    fn failed_run_must_not_leave_operational_paths_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-fail".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "videos");
        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/missing.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/missing.mp4".into())
                    .unwrap(),
                relative_path: NormalizedRelativePath::new("missing.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed_path.exists(), "run should be in failed phase");

        let expected: Vec<Utf8PathBuf> = vec![];
        assert_root_contains_exactly(_server_root.as_path(), &expected);
    }

    #[test]
    fn partial_run_work_area_preserved_under_work_dir_only() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-partial".into()).unwrap();

        let config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "always-fail",
                TransformDefinition {
                    name: "always-fail".into(),
                    kind: TransformKind::Subprocess,
                    program: "false".to_owned(),
                    args: vec![],
                    expected_outputs: vec![],
                },
            ),
            logging: Default::default(),
        };

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();
        let staged_dir = ready_path.join("files/videos");
        fs::create_dir_all(&staged_dir).unwrap();
        fs::write(staged_dir.join("a.mp4"), b"video a data").unwrap();
        fs::write(staged_dir.join("b.mp4"), b"video b data").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    local_path: ClientLocalPath::new("/home/user/a.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 13,
                    mtime_ns: 1000000,
                    sha256: None,
                    link_target: None,

                    transform: Some("always-fail".into()),
                },
                ManifestEntry {
                    local_path: ClientLocalPath::new("/home/user/b.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/videos/b.mp4".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("b.mp4".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 13,
                    mtime_ns: 2000000,
                    sha256: None,
                    link_target: None,

                    transform: Some("always-fail".into()),
                },
            ],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed_path.exists());

        // Root must be empty: no entry was successfully committed.
        let expected: Vec<Utf8PathBuf> = vec![];
        assert_root_contains_exactly(_server_root.as_path(), &expected);

        // Work area is preserved under the failed run directory for diagnostics.
        let work_under_failed = failed_path.join("work");
        assert!(
            work_under_failed.exists(),
            "work area must be preserved under failed run directory for diagnostics, \
             expected: {}",
            work_under_failed.as_str()
        );
    }

    #[test]
    fn transform_outputs_produced_in_work_area_before_commit_to_final() {
        // A subprocess creates output files; cwd is the work-area parent so
        // relative-path outputs land inside the work area. Purgery validates
        // expected outputs are under the work area before committing.
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        fs::create_dir_all(&work_dir).unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "echo-args",
                TransformDefinition {
                    name: "echo-args".into(),
                    kind: TransformKind::Subprocess,
                    program: "sh".to_owned(),
                    args: vec![
                        "-c".to_owned(),
                        "mkdir -p $0/_outputs && echo done > $0/_outputs/result.txt".to_owned(),
                        "{target_directory}".to_owned(),
                    ],
                    expected_outputs: vec!["_outputs".to_owned()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-cwd".into()).unwrap();

        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/input.dat"), b"input").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/input.dat".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/input.dat".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("input.dat".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("echo-args".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&server_config, &nickname, &run_id);
        assert!(result.is_ok(), "transform run should succeed: {result:?}");

        let done_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        // Root must contain only the expected final output paths
        let expected = vec![
            server_config.work_dir.as_path().join("univ"),
            test_storage_root(server_config.work_dir.as_path()).join("univ/data"),
            test_storage_root(server_config.work_dir.as_path()).join("univ/data/_outputs"),
            server_config
                .work_dir
                .as_path()
                .join("univ/data/_outputs/result.txt"),
            server_config.work_dir.as_path().join("laptop"),
            server_config.work_dir.as_path().join("laptop/done"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-cwd"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-cwd/files"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-cwd/files/input.dat"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-cwd/manifest.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-cwd/progress.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-cwd/run.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-cwd/status.toml"),
            server_config.work_dir.as_path().join("laptop/processing"),
            server_config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(server_config.work_dir.as_path(), &expected);

        let result_txt = test_storage_root(server_config.work_dir.as_path())
            .join("univ/data/_outputs/result.txt");
        assert!(result_txt.exists());
        assert_eq!(fs::read_to_string(&result_txt).unwrap(), "done\n");
    }

    // ── Replacement and replay tests ─────────────────────────────────

    #[test]
    fn commit_regular_file_replaces_existing_file_without_sibling_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();

        let final_path = root.join("subdir/file.txt");
        let source = work_source.join("source.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"old content").unwrap();
        fs::write(&source, b"new content").unwrap();

        let result = commit_regular_file_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "replace failed: {result:?}");
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "new content");

        let expected = vec![root.join("subdir"), root.join("subdir/file.txt")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    #[cfg(unix)]
    fn commit_symlink_replaces_existing_symlink_without_sibling_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();

        fs::create_dir_all(&work_source).unwrap();
        let final_path = root.join("subdir/link");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/old/target", final_path.as_std_path()).unwrap();

        let source = work_source.join("srclink");
        std::os::unix::fs::symlink("/new/target", &source).unwrap();
        let result = commit_symlink_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "replace failed: {result:?}");

        let actual_target = std::fs::read_link(final_path.as_std_path()).unwrap();
        assert_eq!(
            Utf8PathBuf::from_path_buf(actual_target).unwrap().as_str(),
            "/new/target"
        );

        let expected = vec![root.join("subdir"), root.join("subdir/link")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    fn commit_regular_file_replaces_empty_directory_without_sibling_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();

        let final_path = root.join("subdir/file.txt");
        let source = work_source.join("source.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(&final_path).unwrap(); // empty directory at final path
        fs::write(&source, b"content").unwrap();

        let result = commit_regular_file_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "replace failed: {result:?}");
        assert!(final_path.is_file());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "content");

        let expected = vec![root.join("subdir"), root.join("subdir/file.txt")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    fn commit_regular_file_refuses_non_empty_directory_without_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();

        let final_path = root.join("subdir/file.txt");
        let source = work_source.join("source.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(&final_path).unwrap();
        fs::write(final_path.join("child.txt"), b"child").unwrap(); // non-empty dir
        fs::write(&source, b"content").unwrap();

        let result = commit_regular_file_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(
            result.is_err(),
            "non-empty directory replace must be rejected"
        );
        assert!(result
            .unwrap_err()
            .contains("non-empty destination directory"));

        // The non-empty directory must remain intact
        assert!(final_path.is_dir());
        assert!(final_path.join("child.txt").exists());
        assert_eq!(
            fs::read_to_string(final_path.join("child.txt")).unwrap(),
            "child"
        );
    }

    #[test]
    fn partial_final_file_after_interrupted_materialization_is_overwritten_by_replay() {
        // Simulate the exact-final-path allowance: an interrupted previous
        // materialization left a partial file at the final path. Replay must
        // overwrite it without creating sibling helpers.
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();

        let final_path = root.join("subdir/file.txt");
        let source = work_source.join("source.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&source, b"complete content here").unwrap();

        // Simulate a partial remnant from an interrupted prior attempt
        fs::write(&final_path, b"partial").unwrap();

        let result = commit_regular_file_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "replay commit failed: {result:?}");
        assert_eq!(
            fs::read_to_string(&final_path).unwrap(),
            "complete content here"
        );

        let expected = vec![root.join("subdir"), root.join("subdir/file.txt")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    fn replay_from_processing_rebuilds_work_area_and_converges() {
        // A run in processing/ with a missing status.toml simulates an
        // interrupted processing attempt. process-once must rebuild the
        // work area from staged files and converge to the correct result.
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-replay".into()).unwrap();

        let (config, _staged) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "videos",
            "test.mp4",
            b"replay content",
        );

        // Move from Ready to Processing and write a partial final file to
        // simulate interrupted materialization.
        let processing_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(processing_path.parent().unwrap()).unwrap();
        fs::rename(&ready_path, &processing_path).unwrap();

        // Write a partial file at the final path to simulate an interrupted
        // direct copy from the previous attempt.
        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"partial remnant").unwrap();

        // Replay: process_once_raw handles recovery from processing/ directory
        process_once_raw(&config).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(
            done_path.exists(),
            "run should be in done phase after replay"
        );
        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4");
        assert_eq!(
            fs::read_to_string(&final_path).unwrap(),
            "replay content",
            "replay must overwrite partial remnant with correct content"
        );

        let expected = vec![
            config.work_dir.as_path().join("univ"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4"),
            config.work_dir.as_path().join("laptop"),
            config.work_dir.as_path().join("laptop/done"),
            config.work_dir.as_path().join("laptop/done/test-replay"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay/files"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay/files/test.mp4"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay/manifest.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay/progress.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay/run.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay/status.toml"),
            config.work_dir.as_path().join("laptop/processing"),
            config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(config.work_dir.as_path(), &expected);
    }

    #[test]
    #[cfg(unix)]
    fn transform_symlink_output_committed_without_operational_paths() {
        // A transform subprocess produces a symlink as output.
        // The symlink entry is moved from the work area to the final path.
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        fs::create_dir_all(&work_dir).unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "make-symlink",
                TransformDefinition {
                    name: "make-symlink".into(),
                    kind: TransformKind::Subprocess,
                    program: "sh".to_owned(),
                    args: vec![
                        "-c".to_owned(),
                        "mkdir -p $0 && ln -sf /etc/hostname $0/the-link".to_owned(),
                        "{target_directory}".to_owned(),
                    ],
                    expected_outputs: vec!["the-link".to_owned()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-symlink".into()).unwrap();

        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/input.dat"), b"data").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/input.dat".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/input.dat".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("input.dat".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 4,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("make-symlink".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&server_config, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "transform symlink run should succeed: {result:?}"
        );

        let done_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let expected = vec![
            server_config.work_dir.as_path().join("univ"),
            test_storage_root(server_config.work_dir.as_path()).join("univ/data"),
            test_storage_root(server_config.work_dir.as_path()).join("univ/data/the-link"),
            server_config.work_dir.as_path().join("laptop"),
            server_config.work_dir.as_path().join("laptop/done"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-symlink"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-symlink/files"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-symlink/files/input.dat"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-symlink/manifest.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-symlink/progress.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-symlink/run.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-symlink/status.toml"),
            server_config.work_dir.as_path().join("laptop/processing"),
            server_config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(server_config.work_dir.as_path(), &expected);

        let the_link =
            test_storage_root(server_config.work_dir.as_path()).join("univ/data/the-link");
        assert!(the_link.exists());
        assert!(
            fs::symlink_metadata(&the_link)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
            "the-link must be a symlink at the target directory"
        );
    }

    // ── Move-based final materialization tests ──────────────────────

    #[test]
    fn regular_file_commit_moves_source_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-move".into()).unwrap();

        let final_path = root.join("sub/file.txt");
        let source = work_source.join("source.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"move-me").unwrap();

        let result = commit_regular_file_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "commit failed: {result:?}");

        assert!(
            !source.exists(),
            "source must be consumed after successful materialization"
        );
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "move-me");
        let expected = vec![root.join("sub"), root.join("sub/file.txt")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_commit_moves_source_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-move".into()).unwrap();

        fs::create_dir_all(&root).unwrap();
        let final_path = root.join("sub/link");
        let source = work_source.join("mylink");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/real/target", &source).unwrap();

        let result = commit_symlink_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "commit failed: {result:?}");

        assert!(
            !source.exists(),
            "source symlink must be consumed after successful materialization"
        );
        let actual_target = std::fs::read_link(final_path.as_std_path()).unwrap();
        assert_eq!(actual_target, std::path::Path::new("/real/target"));
        let expected = vec![root.join("sub"), root.join("sub/link")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    #[cfg(unix)]
    fn directory_tree_commit_consumes_source_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-move".into()).unwrap();

        fs::create_dir_all(&root).unwrap();
        let source_dir = work_source.join("srcdir");
        let final_dir = root.join("dst");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("a.txt"), b"aaa").unwrap();
        fs::write(source_dir.join("b.txt"), b"bbb").unwrap();
        std::os::unix::fs::symlink("/some/target", source_dir.join("link")).unwrap();
        fs::create_dir(source_dir.join("sub")).unwrap();
        fs::write(source_dir.join("sub/c.txt"), b"ccc").unwrap();

        let result = commit_directory_tree(&source_dir, &final_dir, root.as_path(), &run_id);
        assert!(result.is_ok(), "commit failed: {result:?}");

        // Source files/univ/symlinks must be consumed
        assert!(!source_dir.join("a.txt").exists());
        assert!(!source_dir.join("b.txt").exists());
        assert!(!source_dir.join("link").exists());
        assert!(!source_dir.join("sub/c.txt").exists());
        assert!(
            !source_dir.join("sub").exists(),
            "empty subdirectory should be removed"
        );
        assert!(
            !source_dir.exists(),
            "empty source directory should be removed"
        );

        // Final tree must contain migrated entries
        assert_eq!(fs::read_to_string(final_dir.join("a.txt")).unwrap(), "aaa");
        assert_eq!(fs::read_to_string(final_dir.join("b.txt")).unwrap(), "bbb");
        assert_eq!(
            std::fs::read_link(final_dir.join("link")).unwrap(),
            std::path::Path::new("/some/target")
        );
        assert_eq!(
            fs::read_to_string(final_dir.join("sub/c.txt")).unwrap(),
            "ccc"
        );

        let mut expected = vec![
            root.join("dst"),
            root.join("dst/a.txt"),
            root.join("dst/b.txt"),
            root.join("dst/link"),
            root.join("dst/sub"),
            root.join("dst/sub/c.txt"),
        ];
        expected.sort();
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    fn regular_file_replaces_existing_file_with_move_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-move".into()).unwrap();

        let final_path = root.join("sub/data.bin");
        let source = work_source.join("source.bin");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"old").unwrap();
        fs::write(&source, b"new").unwrap();

        let result = commit_regular_file_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "commit failed: {result:?}");
        assert_eq!(result.unwrap(), CommitDisposition::Replaced);

        assert!(!source.exists(), "source must be consumed on replacement");
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "new");
        let expected = vec![root.join("sub"), root.join("sub/data.bin")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_replaces_existing_symlink_with_move_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        let work_source = Utf8PathBuf::from_path_buf(tmp.path().join("staging")).unwrap();
        let run_id = RunId::new("test-move".into()).unwrap();

        fs::create_dir_all(&root).unwrap();
        let final_path = root.join("sub/link");
        let source = work_source.join("mylink");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/old/target", &final_path).unwrap();
        std::os::unix::fs::symlink("/new/target", &source).unwrap();

        let result = commit_symlink_entry(&source, &final_path, root.as_path(), &run_id);
        assert!(result.is_ok(), "commit failed: {result:?}");
        assert_eq!(result.unwrap(), CommitDisposition::Replaced);

        assert!(
            !source.exists(),
            "source symlink must be consumed on replacement"
        );
        let actual_target = std::fs::read_link(final_path.as_std_path()).unwrap();
        assert_eq!(actual_target, std::path::Path::new("/new/target"));
        let expected = vec![root.join("sub"), root.join("sub/link")];
        assert_root_contains_exactly(root.as_path(), &expected);
    }

    #[test]
    fn transform_directory_output_with_recursive_descendants() {
        // A transform subprocess produces a directory tree as output.
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        fs::create_dir_all(&work_dir).unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "make-tree",
                TransformDefinition {
                    name: "make-tree".into(),
                    kind: TransformKind::Subprocess,
                    program: "sh".to_owned(),
                    args: vec![
                        "-c".to_owned(),
                        "mkdir -p $0/out/sub && echo a > $0/out/sub/a.txt && echo b > $0/out/b.txt"
                            .to_owned(),
                        "{target_directory}".to_owned(),
                    ],
                    expected_outputs: vec!["out".to_owned()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-dir".into()).unwrap();

        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/input.dat"), b"data").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/input.dat".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/input.dat".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("input.dat".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 4,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("make-tree".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&server_config, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "transform dir run should succeed: {result:?}"
        );

        let done_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let expected = vec![
            server_config.work_dir.as_path().join("univ"),
            test_storage_root(server_config.work_dir.as_path()).join("univ/data"),
            test_storage_root(server_config.work_dir.as_path()).join("univ/data/out"),
            test_storage_root(server_config.work_dir.as_path()).join("univ/data/out/b.txt"),
            test_storage_root(server_config.work_dir.as_path()).join("univ/data/out/sub"),
            server_config
                .work_dir
                .as_path()
                .join("univ/data/out/sub/a.txt"),
            server_config.work_dir.as_path().join("laptop"),
            server_config.work_dir.as_path().join("laptop/done"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-dir"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-dir/files"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-dir/files/input.dat"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-dir/manifest.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-dir/progress.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-dir/run.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-dir/status.toml"),
            server_config.work_dir.as_path().join("laptop/processing"),
            server_config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(server_config.work_dir.as_path(), &expected);

        let out_b = test_storage_root(server_config.work_dir.as_path()).join("univ/data/out/b.txt");
        let out_sub_a =
            test_storage_root(server_config.work_dir.as_path()).join("univ/data/out/sub/a.txt");
        assert!(out_b.exists());
        assert_eq!(fs::read_to_string(&out_b).unwrap(), "b\n");
        assert!(out_sub_a.exists());
        assert_eq!(fs::read_to_string(&out_sub_a).unwrap(), "a\n");
    }

    // ── Staged file preservation tests ────────────────────────────────

    /// Non-transform staged files are immutable replay source and must
    /// not be consumed by final materialization. Only work-area copies are
    /// consumed.
    #[test]
    fn non_transform_staged_file_preserved_after_successful_materialization() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-staged-preserved".into()).unwrap();

        let (config, _staged_a) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "videos",
            "a.mp4",
            b"video a content",
        );

        // Add a second entry that will fail (missing staged file) after the
        // first one succeeds.
        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        let mut manifest: Manifest =
            toml::from_str(&fs::read_to_string(ready_path.join("manifest.toml")).unwrap()).unwrap();
        manifest.entries.push(ManifestEntry {
            local_path: ClientLocalPath::new("/home/user/nonexistent.mp4".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("files/nonexistent.mp4".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("nonexistent.mp4".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 42,
            mtime_ns: 2000000,
            sha256: None,
            link_target: None,
            transform: None,
        });
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(
            done_path.exists(),
            "run with one success and one failure should be in done phase"
        );

        // Status should be partial.
        let status: RunStatus =
            toml::from_str(&fs::read_to_string(done_path.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Partial);

        // Staged file for the successful entry must still exist.
        let staged_after = done_path.join("files/a.mp4");
        assert!(
            staged_after.exists(),
            "staged file for successful entry must be preserved, \
             expected: {}",
            staged_after.as_str()
        );
        assert_eq!(
            fs::read_to_string(&staged_after).unwrap(),
            "video a content"
        );

        // Final output must exist under work_dir.
        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/videos/a.mp4");
        assert!(final_path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "video a content");

        let expected = vec![
            config.work_dir.as_path().join("univ"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos/a.mp4"),
            config.work_dir.as_path().join("laptop"),
            config.work_dir.as_path().join("laptop/done"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-staged-preserved"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-staged-preserved/files"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-staged-preserved/files/a.mp4"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-staged-preserved/manifest.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-staged-preserved/progress.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-staged-preserved/run.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-staged-preserved/status.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-staged-preserved/work"),
            config.work_dir.as_path().join("laptop/processing"),
            config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(config.work_dir.as_path(), &expected);
    }

    /// Non-transform symlink entries must preserve the staged symlink
    /// while the work-area copy is consumed by materialization.
    #[test]
    #[cfg(unix)]
    fn non_transform_staged_symlink_preserved_after_materialization() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-symlink-staged".into()).unwrap();

        let config = test_server_config(&work_dir);
        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        // Create staged symlink under files/.
        let staged_symlink_dir = ready_path.join("files");
        fs::create_dir_all(&staged_symlink_dir).unwrap();
        let staged_symlink = staged_symlink_dir.join("mylink");
        std::os::unix::fs::symlink("/usr/share/data", &staged_symlink).unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    local_path: ClientLocalPath::new("/home/user/mylink".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/mylink".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("mylink".into()).unwrap(),
                    kind: ManifestEntryKind::Symlink,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: Some(Utf8PathBuf::from("/usr/share/data")),

                    transform: None,
                },
                // Second entry fails (missing staged file).
                ManifestEntry {
                    local_path: ClientLocalPath::new("/home/user/missing.dat".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/missing.dat".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("missing.dat".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 42,
                    mtime_ns: 2000000,
                    sha256: None,
                    link_target: None,

                    transform: None,
                },
            ],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let status: RunStatus =
            toml::from_str(&fs::read_to_string(done_path.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Partial);

        // Staged symlink must still exist.
        let staged_after = done_path.join("files/mylink");
        assert!(
            fs::symlink_metadata(staged_after.as_std_path()).is_ok(),
            "staged symlink must be preserved after materialization"
        );
        let staged_target = std::fs::read_link(staged_after.as_std_path()).unwrap();
        assert_eq!(staged_target, std::path::Path::new("/usr/share/data"));

        // Final symlink must exist under work_dir.
        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/data/mylink");
        assert!(
            fs::symlink_metadata(final_path.as_std_path()).is_ok(),
            "final symlink must exist under root"
        );
        let final_target = std::fs::read_link(final_path.as_std_path()).unwrap();
        assert_eq!(final_target, std::path::Path::new("/usr/share/data"));

        let expected = vec![
            config.work_dir.as_path().join("univ"),
            test_storage_root(config.work_dir.as_path()).join("univ/data"),
            test_storage_root(config.work_dir.as_path()).join("univ/data/mylink"),
            config.work_dir.as_path().join("laptop"),
            config.work_dir.as_path().join("laptop/done"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-staged"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-staged/files"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-staged/files/mylink"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-staged/manifest.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-staged/progress.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-staged/run.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-staged/status.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-staged/work"),
            config.work_dir.as_path().join("laptop/processing"),
            config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(config.work_dir.as_path(), &expected);
    }

    /// When a processing run is replayed (e.g. after interrupted
    /// materialization), the staged upload tree is the replay source and
    /// must survive replay without being consumed.
    #[test]
    fn replay_preserves_staged_upload_source() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-replay-staged".into()).unwrap();

        let (config, _staged_file) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "videos",
            "test.mp4",
            b"replay content",
        );

        // Move from Ready to Processing and write a partial final file.
        let processing_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(processing_path.parent().unwrap()).unwrap();
        fs::rename(&ready_path, &processing_path).unwrap();

        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"partial remnant").unwrap();

        // Record the staged file content before replay.
        let staged_before = processing_path.join("files/test.mp4");
        assert!(staged_before.exists());
        let staged_content_before = fs::read_to_string(&staged_before).unwrap();

        // Replay.
        process_once_raw(&config).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        // Staged file must still exist after replay and contain unchanged
        // content.
        let staged_after = done_path.join("files/test.mp4");
        assert!(
            staged_after.exists(),
            "staged upload source must survive replay, expected: {}",
            staged_after.as_str()
        );
        assert_eq!(
            fs::read_to_string(&staged_after).unwrap(),
            staged_content_before,
            "staged file content must be unchanged after replay"
        );

        // Final output converges.
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "replay content");

        let expected = vec![
            config.work_dir.as_path().join("univ"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos"),
            test_storage_root(config.work_dir.as_path()).join("univ/videos/test.mp4"),
            config.work_dir.as_path().join("laptop"),
            config.work_dir.as_path().join("laptop/done"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay-staged"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay-staged/files"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay-staged/files/test.mp4"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay-staged/manifest.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay-staged/progress.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay-staged/run.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-replay-staged/status.toml"),
            config.work_dir.as_path().join("laptop/processing"),
            config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(config.work_dir.as_path(), &expected);
    }

    // ── Work-area consumption tests ───────────────────────────────────

    /// For a non-transform regular file, the work-area copy is consumed
    /// by materialization while the staged original remains.
    #[test]
    fn non_transform_regular_file_work_copy_consumed_staged_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-work-consumed".into()).unwrap();

        let (config, _staged_orig) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "data",
            "doc.txt",
            b"original content",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        // Staged original must still exist.
        let staged_path = done_path.join("files/doc.txt");
        assert!(staged_path.exists(), "staged original must be preserved");
        assert_eq!(
            fs::read_to_string(&staged_path).unwrap(),
            "original content"
        );

        // Work-area copy must have been consumed by materialization.
        // The work dir is under processing/ which has moved to done/, so
        // the correct path is under done/.
        let work_done_root = done_path.join("work");
        // For done runs the work area is removed entirely.
        assert!(
            !work_done_root.exists(),
            "work-area must be removed for done runs"
        );

        // Final path must contain the content.
        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/data/doc.txt");
        assert!(final_path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "original content");

        let expected = vec![
            config.work_dir.as_path().join("univ"),
            test_storage_root(config.work_dir.as_path()).join("univ/data"),
            test_storage_root(config.work_dir.as_path()).join("univ/data/doc.txt"),
            config.work_dir.as_path().join("laptop"),
            config.work_dir.as_path().join("laptop/done"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-work-consumed"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-work-consumed/files"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-work-consumed/files/doc.txt"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-work-consumed/manifest.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-work-consumed/progress.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-work-consumed/run.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-work-consumed/status.toml"),
            config.work_dir.as_path().join("laptop/processing"),
            config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(config.work_dir.as_path(), &expected);
    }

    /// For a non-transform symlink, the work-area symlink copy is
    /// consumed by materialization while the staged original remains.
    #[test]
    #[cfg(unix)]
    fn non_transform_symlink_work_copy_consumed_staged_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-symlink-consumed".into()).unwrap();

        let config = test_server_config(&work_dir);
        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        let staged_dir = ready_path.join("files");
        fs::create_dir_all(&staged_dir).unwrap();
        let staged_symlink = staged_dir.join("the-link");
        std::os::unix::fs::symlink("/etc/config", &staged_symlink).unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/the-link".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/the-link".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("the-link".into()).unwrap(),
                kind: ManifestEntryKind::Symlink,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: Some(Utf8PathBuf::from("/etc/config")),

                transform: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        // Staged original symlink must still exist.
        let staged_after = done_path.join("files/the-link");
        assert!(
            fs::symlink_metadata(staged_after.as_std_path()).is_ok(),
            "staged symlink original must be preserved"
        );
        let staged_target = std::fs::read_link(staged_after.as_std_path()).unwrap();
        assert_eq!(staged_target, std::path::Path::new("/etc/config"));

        // Work area must be removed for done runs.
        let work_done_root = done_path.join("work");
        assert!(!work_done_root.exists());

        // Final symlink must exist.
        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/data/the-link");
        assert!(
            fs::symlink_metadata(final_path.as_std_path()).is_ok(),
            "final symlink must exist"
        );
        let final_target = std::fs::read_link(final_path.as_std_path()).unwrap();
        assert_eq!(final_target, std::path::Path::new("/etc/config"));

        let expected = vec![
            config.work_dir.as_path().join("univ"),
            test_storage_root(config.work_dir.as_path()).join("univ/data"),
            test_storage_root(config.work_dir.as_path()).join("univ/data/the-link"),
            config.work_dir.as_path().join("laptop"),
            config.work_dir.as_path().join("laptop/done"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-consumed"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-consumed/files"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-consumed/files/the-link"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-consumed/manifest.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-consumed/progress.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-consumed/run.toml"),
            config
                .work_dir
                .as_path()
                .join("laptop/done/test-symlink-consumed/status.toml"),
            config.work_dir.as_path().join("laptop/processing"),
            config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(config.work_dir.as_path(), &expected);
    }

    // ── transform archive paths nickname-free ──

    /// For transform outputs, final paths use the requested destination
    /// destination path without nickname.
    #[test]
    fn transform_paths_use_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        fs::create_dir_all(&work_dir).unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "copy-cmd",
                TransformDefinition {
                    name: "copy-cmd".into(),
                    kind: TransformKind::Subprocess,
                    program: "sh".to_owned(),
                    args: vec![
                        "-c".to_owned(),
                        "mkdir -p $0 && cp {input} $0/{file_name}.out".to_owned(),
                        "{target_directory}".to_owned(),
                    ],
                    expected_outputs: vec!["{file_name}.out".to_owned()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-destination".into()).unwrap();

        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/input.bin"), b"binary data").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/input.bin".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/input.bin".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("input.bin".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("copy-cmd".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&server_config, &nickname, &run_id);
        assert!(result.is_ok(), "transform run failed: {result:?}");

        let done_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = purgery_core::RunStatus::from_toml(&status_content).unwrap();
        for fp in status.entries.iter().flat_map(|e| e.final_paths.iter()) {
            assert!(
                !fp.contains("laptop"),
                "final_paths must not contain nickname: {fp}"
            );
        }

        let final_output =
            test_storage_root(server_config.work_dir.as_path()).join("univ/data/input.bin.out");
        assert!(
            final_output.exists(),
            "script writes expected output to target directory"
        );

        let expected = vec![
            test_storage_root(server_config.work_dir.as_path()).join("univ/data"),
            server_config.work_dir.as_path().join("laptop"),
            server_config.work_dir.as_path().join("laptop/done"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-destination"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-destination/files"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-destination/files/input.bin"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-destination/manifest.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-destination/progress.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-destination/run.toml"),
            server_config
                .work_dir
                .as_path()
                .join("laptop/done/test-pp-destination/status.toml"),
            server_config.work_dir.as_path().join("laptop/processing"),
            server_config.work_dir.as_path().join("laptop/ready"),
        ];
        assert_root_contains_exactly(server_config.work_dir.as_path(), &expected);
    }

    /// For transform outputs, the work-area outputs are consumed by
    /// materialization. The staged original still exists but the
    /// work-area output is gone after successful commit.
    #[test]
    fn transform_work_area_outputs_consumed_after_materialization() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        fs::create_dir_all(&work_dir).unwrap();

        let server_config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "copy-cmd",
                TransformDefinition {
                    name: "copy-cmd".into(),
                    kind: TransformKind::Subprocess,
                    program: "sh".to_owned(),
                    args: vec![
                        "-c".to_owned(),
                        "mkdir -p $0 && cp {input} $0/{file_name}.out".to_owned(),
                        "{target_directory}".to_owned(),
                    ],
                    expected_outputs: vec!["{file_name}.out".to_owned()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-consumed".into()).unwrap();

        let ready_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/input.bin"), b"binary data").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/input.bin".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/input.bin".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("input.bin".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,

                transform: Some("copy-cmd".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&server_config, &nickname, &run_id);
        assert!(result.is_ok(), "transform run failed: {result:?}");

        let done_path = server_config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        // Staged original must still exist.
        let staged_after = done_path.join("files/input.bin");
        assert!(staged_after.exists());
        assert_eq!(fs::read_to_string(&staged_after).unwrap(), "binary data");

        // Work area must be removed for done runs (outputs are consumed
        // by materialization).
        let work_done_root = done_path.join("work");
        assert!(!work_done_root.exists());
    }

    // ── exact destination status paths ──────────

    #[test]
    fn full_pipeline_status_uses_exact_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-final-paths".into()).unwrap();

        let (config, _staged_file_path) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "univ/videos",
            "test.mp4",
            b"hello world",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = purgery_core::RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.entries.len(), 1);
        assert!(!status.entries[0].final_paths.is_empty());
        for fp in &status.entries[0].final_paths {
            assert!(
                !fp.contains("laptop"),
                "final_paths must not contain nickname: {fp}"
            );
        }
        assert_eq!(
            status.entries[0].final_paths,
            vec![test_storage_root(config.work_dir.as_path())
                .join("univ/videos/test.mp4")
                .as_str()
                .to_owned()],
            "final_paths must contain the exact final destination path"
        );
    }

    #[test]
    fn prepare_run_rejects_unknown_requested_transform_before_finish() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("host".to_owned()).unwrap();
        let run_id = RunId::new("unknown-step".to_owned()).unwrap();
        let response: purgery_core::BeginRunResponse =
            toml::from_str(&begin_run(&config, &nickname, &run_id).unwrap()).unwrap();
        let incoming = Utf8PathBuf::from(response.incoming_dir);
        write_run_toml_with_destination(&incoming, &nickname, "univ/data");
        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/source/a.txt".to_owned()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.txt".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.txt".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 1,
                mtime_ns: 1,
                sha256: Some("00".repeat(32)),
                link_target: None,

                transform: Some("typo".to_owned()),
            }],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(error.to_string().contains("not defined on server"));
        assert!(incoming.exists());
        assert!(!config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready)
            .exists());
    }

    #[test]
    #[cfg(unix)]
    fn process_transformed_symlink_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-symlink".into()).unwrap();

        let config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "test-step",
                TransformDefinition {
                    name: "test-step".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".to_owned(),
                    args: vec![],
                    expected_outputs: vec![],
                },
            ),
            logging: Default::default(),
        };

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();

        std::os::unix::fs::symlink("/some/target", ready_path.join("files/the-link")).unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "univ/data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/the-link".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/the-link".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("the-link".into()).unwrap(),
                kind: ManifestEntryKind::Symlink,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: Some("/some/target".into()),

                transform: Some("test-step".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "transform symlink run should succeed: {result:?}"
        );

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].status, FileStatus::Imported);

        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/data/the-link");
        assert!(
            !final_path.exists(),
            "transform outputs are not moved to final destination"
        );
    }

    #[test]
    fn process_transformed_directory_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-dir".into()).unwrap();

        let config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "test-step",
                TransformDefinition {
                    name: "test-step".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".to_owned(),
                    args: vec![],
                    expected_outputs: vec![],
                },
            ),
            logging: Default::default(),
        };

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();

        let staged_dir = ready_path.join("files/photos");
        fs::create_dir(&staged_dir).unwrap();
        fs::write(staged_dir.join("photo1.jpg"), b"photo data").unwrap();

        write_run_toml_with_destination(&ready_path, &nickname, "univ/data");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/photos".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/photos".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("photos".into()).unwrap(),
                kind: ManifestEntryKind::Directory,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: None,

                transform: Some("test-step".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "transform directory run should succeed: {result:?}"
        );

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].status, FileStatus::Imported);

        let final_dir = test_storage_root(config.work_dir.as_path()).join("univ/data/photos");
        assert!(
            !final_dir.exists(),
            "transform outputs are not moved to final destination"
        );
    }

    // ── No-commit (skip move) tests for transformed entries ──

    #[test]
    fn transform_empty_expected_outputs_succeeds_with_empty_final_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        let config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "noop",
                TransformDefinition {
                    name: "noop".into(),
                    kind: TransformKind::Subprocess,
                    program: "true".to_owned(),
                    args: vec![],
                    expected_outputs: vec![],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-no-commit".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/data.bin"), b"payload").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/output");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/data.bin".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/data.bin".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("data.bin".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 7,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                transform: Some("noop".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert!(
            status.entries[0].final_paths.is_empty(),
            "empty expected_outputs must produce zero final_paths"
        );

        let expected_final =
            test_storage_root(config.work_dir.as_path()).join("univ/output/data.bin");
        assert!(
            !expected_final.exists(),
            "transformed entry output must not be committed to final destination"
        );
    }

    #[test]
    fn target_directory_placeholder_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        let script_path = tmp.path().join("dst-script.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\n# args: --input {input} --output-dir {target_directory}\n\
             target_dir=\"$4\"\n\
             mkdir -p \"$target_dir\"\n\
             touch \"$target_dir/result\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "place-at-dest",
                TransformDefinition {
                    name: "place-at-dest".into(),
                    kind: TransformKind::Subprocess,
                    program: script_path.to_string_lossy().to_string(),
                    args: vec![
                        "--input".into(),
                        "{input}".into(),
                        "--output-dir".into(),
                        "{target_directory}".into(),
                    ],
                    expected_outputs: vec!["result".into()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-final-dest".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/data.bin"), b"payload").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/output");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/data.bin".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/data.bin".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("data.bin".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 7,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                transform: Some("place-at-dest".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[0].final_paths.len(), 1);

        let expected_final =
            test_storage_root(config.work_dir.as_path()).join("univ/output/result");
        assert!(
            status.entries[0]
                .final_paths
                .contains(&expected_final.as_str().to_owned()),
            "final_paths must record the expected output path"
        );
        assert!(
            expected_final.exists(),
            "script using {{target_directory}} must place output at target directory"
        );
    }

    #[test]
    fn non_transform_entry_still_commits_to_final_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-idemp-commit".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &work_dir,
            &nickname,
            &run_id,
            "univ/output",
            "plain.txt",
            b"plain content",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let final_path = test_storage_root(config.work_dir.as_path()).join("univ/output/plain.txt");
        assert!(
            final_path.exists(),
            "non-transform entry must still be committed to final destination"
        );
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "plain content");

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    #[test]
    fn transform_with_target_directory_and_expected_output() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();

        let script_path = tmp.path().join("compress-dst.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\n# args: --input {input} --output-dir {target_directory}\n\
             input=\"$2\"\n\
             target_dir=\"$4\"\n\
             stem=\"${input##*/}\"\n\
             stem=\"${stem%.*}\"\n\
             mkdir -p \"$target_dir\"\n\
             touch \"$target_dir/${stem}.Z.webm\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "compress",
                TransformDefinition {
                    name: "compress".into(),
                    kind: TransformKind::Subprocess,
                    program: script_path.to_string_lossy().to_string(),
                    args: vec![
                        "--input".into(),
                        "{input}".into(),
                        "--output-dir".into(),
                        "{target_directory}".into(),
                    ],
                    expected_outputs: vec!["{stem}.Z.webm".into()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-dst-out".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/video.mp4"), b"video").unwrap();
        write_run_toml_with_destination(&ready_path, &nickname, "univ/videos");

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/video.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/video.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("video.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                transform: Some("compress".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[0].final_paths.len(), 1);

        let original_final =
            test_storage_root(config.work_dir.as_path()).join("univ/videos/video.mp4");
        let compressed_final =
            test_storage_root(config.work_dir.as_path()).join("univ/videos/video.Z.webm");
        assert!(
            !original_final.exists(),
            "transform: original must not be committed"
        );
        assert!(
            compressed_final.exists(),
            "script using {{target_directory}} placed compressed output at destination"
        );
    }

    // ── prepare-run transform-definition validation ──────────────────

    #[test]
    fn prepare_run_rejects_invalid_expected_outputs_in_transform_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let storage = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let mut config = test_server_config(&work_dir);
        config.transforms = single_transform(
            "compress-video",
            TransformDefinition {
                name: "compress-video".into(),
                kind: TransformKind::Subprocess,
                program: "true".to_owned(),
                args: vec![],
                expected_outputs: vec!["{input}".into()], // INVALID: uses {input} placeholder
            },
        );
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-prepare-bad-output".into()).unwrap();

        let incoming_path = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming_path).unwrap();

        let dest = storage.join("univ/videos");
        let run_config_content = format!(
            r#"purgery_version = "0.1.0-test"
nickname = "{}"
destination = "{}"
delete_after_import = true
"#,
            nickname.as_str(),
            dest.as_str()
        );
        fs::write(incoming_path.join("run.toml"), &run_config_content).unwrap();

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                transform: Some("compress-video".into()),
            }],
        };
        fs::write(
            incoming_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = prepare_run(&config, &nickname, &run_id);
        assert!(
            result.is_err(),
            "prepare_run must reject invalid expected_outputs"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid") || err.contains("expected_output"),
            "error must reference expected_output validation, got: {err}"
        );
    }

    #[test]
    fn apply_transform_rejects_invalid_definition_before_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let mut config = test_server_config(&work_dir);
        config.transforms = single_transform(
            "bad-transform",
            TransformDefinition {
                name: "bad-transform".into(),
                kind: TransformKind::Subprocess,
                program: "".to_owned(),
                args: vec![],
                expected_outputs: vec!["output.txt".into()],
            },
        );

        let work_path = work_dir.join("work").join("test.txt");
        fs::create_dir_all(work_path.parent().unwrap()).unwrap();
        fs::write(&work_path, b"test data").unwrap();

        let result = test_apply_transform(&config, &work_path);
        assert!(
            result.is_err(),
            "apply_transform must reject invalid definition before spawning"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("definition is invalid") || err.contains("program is empty"),
            "error must indicate transform definition validation, got: {err}"
        );
    }

    #[test]
    fn transform_absolute_expected_output_outside_destination_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_path = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_dir = tmp_path.join("purgery");
        let output_dir = tmp_path.join("other-output");
        fs::create_dir_all(&output_dir).unwrap();

        let script_path = tmp_path.join("write-abs.sh");
        let output_path = output_dir.join("result.webm");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nmkdir -p {:?} && touch {:?}\n",
                output_dir.as_str(),
                output_path.as_str()
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let config = ServerConfig {
            work_dir: PurgeryRoot::new(work_dir.to_owned()).unwrap(),
            gc: Default::default(),
            transforms: single_transform(
                "write-absolute",
                TransformDefinition {
                    name: "write-absolute".into(),
                    kind: TransformKind::Subprocess,
                    program: script_path.as_str().to_owned(),
                    args: vec![],
                    expected_outputs: vec![output_path.as_str().to_owned()],
                },
            ),
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-abs-outside".into()).unwrap();

        let ready_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();
        fs::write(ready_path.join("files/data.bin"), b"payload").unwrap();

        let destination_dir = tmp_path.join("archive");
        let run_config_content = format!(
            r#"purgery_version = "0.1.0-test"
nickname = "{}"
destination = "{}"
delete_after_import = true
"#,
            nickname.as_str(),
            destination_dir.as_str()
        );
        fs::write(ready_path.join("run.toml"), &run_config_content).unwrap();

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/home/user/data.bin".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/data.bin".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("data.bin".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 7,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                transform: Some("write-absolute".into()),
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(
            status.entries[0].final_paths,
            vec![output_path.as_str().to_owned()],
            "final_paths must record the absolute expected output path"
        );
        assert!(
            output_path.exists(),
            "transform must create the output at the absolute path outside destination"
        );
        // Guard: the destination dir must NOT contain the output
        assert!(
            !destination_dir.join("result.webm").exists(),
            "output must NOT be inside destination directory"
        );
    }

    // ── purgery-version rejection in prepare-run ─────────────────────

    #[test]
    fn prepare_run_rejects_missing_purgery_version_in_run_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let mut config = test_server_config(&work_dir);
        config.transforms.insert(
            "test-step".into(),
            TransformDefinition {
                name: "test-step".into(),
                kind: TransformKind::Subprocess,
                program: "/bin/true".to_string(),
                args: Vec::new(),
                expected_outputs: Vec::new(),
            },
        );
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pv-missing-run".into()).unwrap();
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        // Write run.toml WITHOUT purgery_version
        fs::write(
            incoming.join("run.toml"),
            format!(
                r#"nickname = "{}"
destination = "/tmp/dest"
delete_after_import = true
"#,
                nickname.as_str()
            ),
        )
        .unwrap();

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/source/file.txt".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/file.txt".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("file.txt".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                transform: Some("test-step".into()),
            }],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("run config"),
            "error must mention run config, got: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_incompatible_purgery_version_in_run_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let mut config = test_server_config(&work_dir);
        config.transforms.insert(
            "test-step".into(),
            TransformDefinition {
                name: "test-step".into(),
                kind: TransformKind::Subprocess,
                program: "/bin/true".to_string(),
                args: Vec::new(),
                expected_outputs: Vec::new(),
            },
        );
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pv-incompat-run".into()).unwrap();
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        // Write run.toml with incompatible purgery_version
        fs::write(
            incoming.join("run.toml"),
            format!(
                r#"purgery_version = "2.0.0"
nickname = "{}"
destination = "/tmp/dest"
delete_after_import = true
"#,
                nickname.as_str()
            ),
        )
        .unwrap();

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/source/file.txt".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/file.txt".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("file.txt".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                transform: Some("test-step".into()),
            }],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("run config"),
            "error must mention run config, got: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_missing_purgery_version_in_manifest_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let mut config = test_server_config(&work_dir);
        config.transforms.insert(
            "test-step".into(),
            TransformDefinition {
                name: "test-step".into(),
                kind: TransformKind::Subprocess,
                program: "/bin/true".to_string(),
                args: Vec::new(),
                expected_outputs: Vec::new(),
            },
        );
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pv-missing-manifest".into()).unwrap();
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        write_run_toml_with_raw_destination(&incoming, &nickname, "/tmp/dest");

        // Write manifest.toml WITHOUT purgery_version
        fs::write(
            incoming.join("manifest.toml"),
            r#"run_id = "test-pv-missing-manifest"
nickname = "laptop"
[[entries]]
local_path = "/source/file.txt"
staged_path = "files/file.txt"
relative_path = "file.txt"
kind = "regular_file"
size = 13
mtime_ns = 0
transform = "test-step"
"#,
        )
        .unwrap();

        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("manifest"),
            "error must mention manifest, got: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_incompatible_purgery_version_in_manifest_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let mut config = test_server_config(&work_dir);
        config.transforms.insert(
            "test-step".into(),
            TransformDefinition {
                name: "test-step".into(),
                kind: TransformKind::Subprocess,
                program: "/bin/true".to_string(),
                args: Vec::new(),
                expected_outputs: Vec::new(),
            },
        );
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pv-incompat-manifest".into()).unwrap();
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        write_run_toml_with_raw_destination(&incoming, &nickname, "/tmp/dest");

        // Write manifest.toml with incompatible purgery_version
        fs::write(
            incoming.join("manifest.toml"),
            r#"purgery_version = "2.0.0"
run_id = "test-pv-incompat-manifest"
nickname = "laptop"
[[entries]]
local_path = "/source/file.txt"
staged_path = "files/file.txt"
relative_path = "file.txt"
kind = "regular_file"
size = 13
mtime_ns = 0
transform = "test-step"
"#,
        )
        .unwrap();

        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("manifest"),
            "error must mention manifest, got: {error}"
        );
    }

    // ── recovery with incompatible status.toml ──────────────────────

    #[test]
    fn recovery_refuses_incompatible_status_and_leaves_it_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-incompat-status".into()).unwrap();
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();

        // Write status.toml with incompatible purgery_version
        let original_content = r#"purgery_version = "2.0.0"
run_id = "recover-incompat-status"
nickname = "laptop"
state = "done"
"#;
        fs::write(processing.join("status.toml"), original_content).unwrap();

        let error = recover_or_process_processing_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("incompatible"),
            "error must describe version incompatibility, got: {error}"
        );

        // Verify the original status.toml is unchanged (not replaced)
        let status_content = fs::read_to_string(processing.join("status.toml")).unwrap();
        assert_eq!(status_content, original_content);
    }

    // ── process_once_raw does not overwrite incompatible status ─────

    #[test]
    fn process_once_raw_preserves_incompatible_processing_status() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("raw-incompat".into()).unwrap();
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();

        // Write status.toml with incompatible purgery_version
        let original_content = r#"purgery_version = "2.0.0"
run_id = "raw-incompat"
nickname = "laptop"
state = "done"
"#;
        fs::write(processing.join("status.toml"), original_content).unwrap();

        // process_once_raw must not move the run to failed or overwrite the status
        let result = process_once_raw(&config);
        assert!(result.is_ok(), "process_once_raw must succeed: {result:?}");

        // Processing directory unchanged
        assert!(
            processing.exists(),
            "processing directory must remain in place"
        );
        let status_content = fs::read_to_string(processing.join("status.toml")).unwrap();
        assert_eq!(status_content, original_content);

        // No failed or done directory was created
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.exists(), "must NOT move to failed");
        let done = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(!done.exists(), "must NOT move to done");
    }

    // ── GC rejects incompatible lease version ────────────────────────

    #[test]
    fn gc_rejects_incompatible_lease_version() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("gc-lease-incompat".into()).unwrap();

        // Create incoming directory with lease.toml that has
        // incompatible purgery_version
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("lease.toml"),
            r#"purgery_version = "2.0.0"
protocol_version = 1
nickname = "laptop"
run_id = "gc-lease-incompat"
expires_at_unix_secs = 9999999999999
"#,
        )
        .unwrap();
        // Also put a file in the incoming dir to check it's not removed
        fs::create_dir_all(incoming.join("files")).unwrap();
        fs::write(incoming.join("files").join("staged.mp4"), b"content").unwrap();

        // GC must NOT collect or move the incoming run when lease
        // has incompatible version.
        let result = run_gc(&config);
        assert!(result.is_ok(), "run_gc must succeed: {result:?}");

        // Incoming run must still be present
        assert!(
            incoming.exists(),
            "incoming run must NOT be collected when lease version is incompatible"
        );
        assert!(
            incoming.join("files").join("staged.mp4").exists(),
            "staged files must NOT be removed when lease version is incompatible"
        );
        // No failed or quarantine directory should exist
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.exists(), "must NOT move to failed");
    }

    #[test]
    fn gc_rejects_missing_lease_version() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("gc-lease-missing".into()).unwrap();

        // Create incoming directory with old-style lease.toml that has
        // NO purgery_version
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("lease.toml"),
            r#"protocol_version = 1
nickname = "laptop"
run_id = "gc-lease-missing"
expires_at_unix_secs = 9999999999999
"#,
        )
        .unwrap();
        fs::create_dir_all(incoming.join("files")).unwrap();
        fs::write(incoming.join("files").join("staged.mp4"), b"content").unwrap();

        // GC must NOT collect when lease is missing purgery_version
        let result = run_gc(&config);
        assert!(result.is_ok(), "run_gc must succeed: {result:?}");

        // Verify nothing was moved or removed
        assert!(incoming.exists(), "incoming run must remain in place");
        assert!(
            incoming.join("files").join("staged.mp4").exists(),
            "staged files must not be removed"
        );
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.exists(), "must NOT move to failed");
    }

    #[test]
    fn gc_still_collects_expired_compatible_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("gc-lease-expired".into()).unwrap();

        // Create incoming directory with compatible but expired lease
        let incoming = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("lease.toml"),
            r#"purgery_version = "0.1.0-test"
protocol_version = 1
nickname = "laptop"
run_id = "gc-lease-expired"
expires_at_unix_secs = 1
"#,
        )
        .unwrap();

        // GC must collect the expired compatible lease
        let result = run_gc(&config);
        assert!(result.is_ok(), "run_gc must succeed: {result:?}");

        // Incoming run should be moved to failed
        assert!(
            !incoming.exists(),
            "incoming run must be collected when lease is expired and compatible"
        );
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(
            failed.exists(),
            "expired compatible lease must move to failed"
        );
    }

    // ── process_run_target behavior ──────────────────────────────────

    #[test]
    fn process_run_target_failure_moves_to_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("tgt-fail".into()).unwrap();

        // Create a ready run with good run.toml but missing manifest.toml
        let ready = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready).unwrap();
        write_run_toml(&ready, &nickname);

        let result = process_run_target(&config, &nickname, &run_id);
        assert!(
            result.is_err(),
            "process_run_target must fail for missing manifest"
        );

        // Run must NOT be left in ready or processing
        assert!(
            !ready.exists(),
            "failed ready run must be moved out of ready"
        );
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        assert!(
            !processing.exists(),
            "failed run must not be left in processing"
        );

        // Run must be moved to failed
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed.exists(), "failed run must be in failed directory");
        assert!(
            failed.join("status.toml").exists(),
            "failed run must have a status file"
        );
    }

    #[test]
    fn process_run_target_incompatible_ready_left_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("tgt-incompat".into()).unwrap();

        // Create a ready run with incompatible purgery_version in run.toml
        let ready = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready).unwrap();
        fs::write(
            ready.join("run.toml"),
            format!(
                r#"purgery_version = "99.0.0"
nickname = "{}"
destination = "/tmp/dest"
delete_after_import = true
"#,
                nickname.as_str()
            ),
        )
        .unwrap();

        let result = process_run_target(&config, &nickname, &run_id);
        assert!(
            result.is_err(),
            "process_run_target must fail for incompatible run"
        );

        // Incompatible ready run must be left in place
        assert!(
            ready.exists(),
            "incompatible ready run must remain in ready"
        );
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.exists(), "must NOT move to failed");
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        assert!(!processing.exists(), "must NOT move to processing");
    }

    #[test]
    fn process_run_target_idempotent_on_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("tgt-term".into()).unwrap();

        // Create a done run with valid terminal status
        let done = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        fs::create_dir_all(&done).unwrap();
        let status = RunStatus {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(done.join("status.toml"), status.to_toml().unwrap()).unwrap();

        // process_run_target on a terminal run must succeed (idempotent)
        let result = process_run_target(&config, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "process_run_target on terminal run must succeed"
        );

        // Terminal directory must still exist unchanged
        assert!(done.exists(), "terminal run must still exist");
        assert!(
            done.join("status.toml").exists(),
            "terminal status must still exist"
        );
    }

    #[test]
    fn process_run_target_not_found_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("tgt-nonexistent".into()).unwrap();

        let result = process_run_target(&config, &nickname, &run_id);
        assert!(
            result.is_err(),
            "process_run_target must fail for nonexistent run"
        );
        assert!(
            result.unwrap_err().to_string().contains("not found"),
            "error must mention 'not found'"
        );
    }

    #[test]
    fn process_run_target_ready_normal_path() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("tgt-normal".into()).unwrap();

        // Create a ready run with valid inputs and a transform defined
        let mut config_with_transform = config.clone();
        config_with_transform.transforms.insert(
            "test-step".into(),
            TransformDefinition {
                name: "test-step".into(),
                kind: TransformKind::Subprocess,
                program: "/bin/true".to_string(),
                args: Vec::new(),
                expected_outputs: Vec::new(),
            },
        );
        let ready = config_with_transform
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready).unwrap();
        write_run_toml_with_raw_destination(&ready, &nickname, "/tmp/dest");
        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                local_path: ClientLocalPath::new("/source/file.txt".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/file.txt".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("file.txt".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                transform: Some("test-step".into()),
            }],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let result = process_run_target(&config_with_transform, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "normal ready run should be processed successfully"
        );

        // Run should end up in done or failed
        assert!(!ready.exists(), "ready run must be claimed");
        let failed = config_with_transform
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let done = config_with_transform
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(
            failed.exists() || done.exists(),
            "processed run must end up in done or failed"
        );
    }

    #[test]
    fn process_run_target_processing_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("tgt-processing".into()).unwrap();

        // Create a processing run with no terminal status (simulating
        // an active transform that has not yet completed).
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();

        // Hold the processor lock from this test to simulate another
        // active processor.
        let lock = crate::phases::try_lock_run_dir_processor(&processing)
            .unwrap()
            .expect("must be able to acquire test lock");

        // No status.toml — this simulates an actively processing run.
        // process_run_target must NOT recover or replay it.
        let result = process_run_target(&config, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "processing run must be a no-op when lock is held: {result:?}"
        );

        // Processing directory unchanged
        assert!(processing.exists(), "processing directory must remain");
        // No terminal status written
        assert!(
            !processing.join("status.toml").exists(),
            "must not write status for processing run"
        );
        // Not moved to failed
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.exists(), "must NOT move processing run to failed");

        // Release lock before the directory is cleaned up by tmp
        drop(lock);
    }

    #[test]
    fn process_once_loses_claim_race_does_not_move_processing_to_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("race-loser".into()).unwrap();

        // Simulate the race: process-once lists this as ready...
        let ready = config.work_dir.run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready).unwrap();
        write_run_toml(&ready, &nickname);
        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        // ...but process-run claimed it before process-once could.
        // Remove ready and create processing to simulate the race loss.
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        fs::remove_dir_all(&ready).unwrap();

        // process-once calls claim_ready_run which sees processing, not ready.
        let outcome = crate::process::claim_ready_run(&config, &nickname, &run_id);
        assert!(
            matches!(
                outcome,
                crate::process::ReadyClaimOutcome::AlreadyProcessing
            ),
            "claim must return AlreadyProcessing when processing exists: {outcome:?}"
        );

        // Processing run must remain intact
        assert!(processing.exists(), "processing directory must remain");
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.exists(), "must NOT move processing run to failed");
    }

    // ── progress/status version distinction in run-state ────────────

    #[test]
    fn run_state_progress_reports_incompatible_version() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("progress-version".into()).unwrap();

        // Processing phase with progress.toml that has incompatible
        // purgery_version
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        fs::write(
            processing.join("progress.toml"),
            r#"purgery_version = "2.0.0"
protocol_version = 1
nickname = "laptop"
run_id = "progress-version"
phase = "processing"
state = "processing_entry"
entry_index = 0
entry_total = 1
current_entry = "files/a.txt"
current_transform = "test-step"
started_at_unix_secs = 1000
updated_at_unix_secs = 1000
"#,
        )
        .unwrap();

        let response = run_state(&config, &nickname, &run_id).unwrap();
        assert_eq!(
            response.progress_status.as_deref(),
            Some("incompatible_version")
        );
        assert!(response.message.contains("incompatible"));
    }

    #[test]
    fn run_state_progress_reports_missing_version() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("progress-missing".into()).unwrap();

        // Processing phase with progress.toml that has NO purgery_version
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        fs::write(
            processing.join("progress.toml"),
            r#"protocol_version = 1
nickname = "laptop"
run_id = "progress-missing"
phase = "processing"
state = "processing_entry"
entry_index = 0
entry_total = 1
current_entry = "files/a.txt"
current_transform = "test-step"
started_at_unix_secs = 1000
updated_at_unix_secs = 1000
"#,
        )
        .unwrap();

        let response = run_state(&config, &nickname, &run_id).unwrap();
        assert_eq!(
            response.progress_status.as_deref(),
            Some("incompatible_version")
        );
        assert!(response.message.contains("missing"));
    }

    #[test]
    fn run_state_progress_reports_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("progress-malformed".into()).unwrap();

        // Processing phase with malformed progress.toml
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        fs::write(processing.join("progress.toml"), "not valid toml {{{").unwrap();

        let response = run_state(&config, &nickname, &run_id).unwrap();
        assert_eq!(response.progress_status.as_deref(), Some("malformed"));
    }

    // ── recovery with missing-version status.toml ──────────────────

    #[test]
    fn recovery_refuses_missing_version_status() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-missing-pv-status".into()).unwrap();
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();

        // Write status.toml WITHOUT purgery_version (old style)
        let original_content = r#"run_id = "recover-missing-pv-status"
nickname = "laptop"
state = "done"
"#;
        fs::write(processing.join("status.toml"), original_content).unwrap();

        // Call the recovery function directly
        let error = recover_or_process_processing_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            matches!(&error, RecoveryError::IncompatibleStatus { .. }),
            "must return IncompatibleStatus for missing version"
        );

        // Verify the original status.toml is unchanged (not replaced)
        assert!(processing.exists());
        let status_content = fs::read_to_string(processing.join("status.toml")).unwrap();
        assert_eq!(status_content, original_content);
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.exists(), "must NOT move to failed");
    }

    #[test]
    fn process_once_raw_preserves_missing_version_status() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let _server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&work_dir);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("raw-missing-pv".into()).unwrap();
        let processing = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();

        // Write status.toml WITHOUT purgery_version
        let original_content = r#"run_id = "raw-missing-pv"
nickname = "laptop"
state = "done"
"#;
        fs::write(processing.join("status.toml"), original_content).unwrap();

        // process_once_raw must not move the run to failed or overwrite
        let result = process_once_raw(&config);
        assert!(result.is_ok(), "process_once_raw must succeed: {result:?}");

        assert!(processing.exists());
        let status_content = fs::read_to_string(processing.join("status.toml")).unwrap();
        assert_eq!(status_content, original_content);
        let failed = config
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.exists(), "must NOT move to failed");
    }
}
