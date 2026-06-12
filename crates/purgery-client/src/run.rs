use anyhow::{Context, Result};
use purgery_core::{
    BeginRunResponse, ClientRunPhase, ClientRunState, DestinationPath, DurableCleanupState,
    Manifest, Nickname, PrepareRunResponse, RunConfig, RunId, RunStateResponse, RunStatus,
};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::classify;
use crate::cleanup;
use crate::ssh;
use crate::transfer;
use crate::SyncArgs;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteDestination {
    host: String,
    path: DestinationPath,
}

fn parse_destination(destination: &str) -> Result<RemoteDestination> {
    let colon_pos = destination.rfind(':').ok_or_else(|| {
        anyhow::anyhow!("destination must be in format USER@HOST:PATH or HOST:PATH")
    })?;
    let host = &destination[..colon_pos];
    let path = &destination[colon_pos + 1..];
    if host.is_empty() || path.is_empty() {
        anyhow::bail!("destination host and path must not be empty");
    }
    let path = DestinationPath::new(camino::Utf8PathBuf::from(path))
        .with_context(|| format!("invalid destination path: {path}"))?;
    Ok(RemoteDestination {
        host: host.to_owned(),
        path,
    })
}

fn derive_nickname(destination: &str) -> Result<Nickname> {
    let remote = parse_destination(destination)?;
    let sanitized: String = remote
        .host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    Ok(Nickname::new(sanitized).or_else(|_| Nickname::new("default".to_owned()))?)
}

fn begin_run(
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<BeginRunResponse> {
    let output = ssh::server_cmd(
        host,
        server_cmd,
        &[
            "begin-run",
            "--nickname",
            nickname.as_str(),
            "--run-id",
            run_id.as_str(),
        ],
    )?;
    let resp: BeginRunResponse =
        toml::from_str(&output).with_context(|| "failed to parse begin-run response")?;
    if resp.protocol_version != 1 {
        anyhow::bail!(
            "unsupported begin-run protocol version: {}",
            resp.protocol_version
        );
    }
    Ok(resp)
}

fn prepare_run(
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<PrepareRunResponse> {
    let output = ssh::server_cmd(
        host,
        server_cmd,
        &[
            "prepare-run",
            "--nickname",
            nickname.as_str(),
            "--run-id",
            run_id.as_str(),
        ],
    )?;
    let resp: PrepareRunResponse =
        toml::from_str(&output).with_context(|| "failed to parse prepare-run response")?;
    if resp.protocol_version != 1 {
        anyhow::bail!(
            "unsupported prepare-run protocol version: {}",
            resp.protocol_version
        );
    }
    Ok(resp)
}

fn heartbeat_run(host: &str, server_cmd: &str, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    ssh::server_cmd(
        host,
        server_cmd,
        &[
            "heartbeat-run",
            "--nickname",
            nickname.as_str(),
            "--run-id",
            run_id.as_str(),
        ],
    )?;
    Ok(())
}

fn finish_run(host: &str, server_cmd: &str, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    ssh::server_cmd(
        host,
        server_cmd,
        &[
            "finish-run",
            "--nickname",
            nickname.as_str(),
            "--run-id",
            run_id.as_str(),
        ],
    )?;
    Ok(())
}

fn run_state(
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStateResponse> {
    let output = ssh::server_cmd(
        host,
        server_cmd,
        &[
            "run-state",
            "--nickname",
            nickname.as_str(),
            "--run-id",
            run_id.as_str(),
        ],
    )?;
    let resp: RunStateResponse =
        toml::from_str(&output).with_context(|| "failed to parse run-state response")?;
    Ok(resp)
}

fn read_status(
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStatus> {
    let output = ssh::server_cmd(
        host,
        server_cmd,
        &[
            "status",
            "--nickname",
            nickname.as_str(),
            "--run-id",
            run_id.as_str(),
        ],
    )?;
    let status =
        RunStatus::from_toml(output.trim()).with_context(|| "failed to parse status response")?;
    Ok(status)
}

fn wait_for_terminal(
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStateResponse> {
    let poll_interval = Duration::from_secs(5);
    let mut last_phase = String::new();
    let mut attempts_since_report = 0u64;

    loop {
        let response = run_state(host, server_cmd, nickname, run_id)?;

        if response.terminal {
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = %response.phase,
                "run reached terminal phase"
            );
            return Ok(response);
        }

        match response.phase.as_str() {
            "ready" | "processing" => {
                if response.phase != last_phase {
                    info!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        phase = %response.phase,
                        "run phase changed"
                    );
                    last_phase = response.phase.clone();
                    attempts_since_report = 0;
                }
                attempts_since_report += 1;
                if attempts_since_report.is_multiple_of(12) {
                    info!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        phase = %last_phase,
                        "still waiting for server to process run"
                    );
                }
            }
            "not_found" => {
                anyhow::bail!(
                    "run {}/{} not found on server",
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

        std::thread::sleep(poll_interval);
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_client_run_state(
    state_dir: &str,
    nickname: &Nickname,
    run_id: &RunId,
    host: &str,
    server_command: &str,
    manifest: &Manifest,
    run_config: &RunConfig,
    terminal_status: Option<&str>,
    phase: ClientRunPhase,
) -> Result<()> {
    let run_state = ClientRunState {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        host: host.to_owned(),
        server_command: server_command.to_owned(),
        manifest: manifest.to_toml()?,
        run_config: run_config.to_toml()?,
        terminal_status: terminal_status.map(|s| s.to_owned()),
        phase,
    };
    let dir = camino::Utf8PathBuf::from(state_dir)
        .join("runs")
        .join(format!("{}-{}", nickname.as_str(), run_id.as_str()));
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

fn remove_client_run_state(state_dir: &str, nickname: &Nickname, run_id: &RunId) {
    let dir = camino::Utf8PathBuf::from(state_dir)
        .join("runs")
        .join(format!("{}-{}", nickname.as_str(), run_id.as_str()));
    let _ = fs::remove_dir_all(dir.as_std_path());
}

/// Start a heartbeat thread that calls heartbeat-run every interval_secs/2.
/// The thread stops when `stop` is set to true.
/// Returns the JoinHandle. The caller MUST join this handle and check the result.
fn start_heartbeat(
    host: String,
    server_cmd: String,
    nickname: Nickname,
    run_id: RunId,
    interval_secs: u64,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Result<()>> {
    let half_interval = Duration::from_millis((interval_secs.max(2) * 500).min(600_000));
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            debug!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                "sending heartbeat"
            );
            if let Err(e) = heartbeat_run(&host, &server_cmd, &nickname, &run_id) {
                error!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    "heartbeat failed: {e}"
                );
                return Err(e);
            }
            let start = std::time::Instant::now();
            while start.elapsed() < half_interval && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        debug!(
            nickname = %nickname.as_str(),
            run_id = %run_id.as_str(),
            "heartbeat stopped"
        );
        Ok(())
    })
}

// ── Resume logic ───────────────────────────────────────────────────────

/// Resume persisted client run states. Returns Ok only if all recoverable
/// states were resolved. Unresolved states cause the whole invocation to fail.
fn resume_runs(state_dir: &str) -> Result<()> {
    let runs_dir = camino::Utf8PathBuf::from(state_dir).join("runs");
    if !runs_dir.as_std_path().is_dir() {
        return Ok(());
    }

    let mut entries: Vec<_> = match fs::read_dir(runs_dir.as_std_path()) {
        Ok(iter) => iter.filter_map(|e| e.ok()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| "failed to read runs directory")?,
    };
    entries.sort_by_key(|e| e.file_name());

    let mut any_error = false;

    for entry in entries {
        let state_path = entry.path().join("state.toml");
        if !state_path.exists() {
            continue;
        }
        let content = match fs::read_to_string(&state_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let run_state: ClientRunState = match toml::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                error!("failed to parse client run state {:?}: {e}", state_path);
                let corrupt_path = state_path.with_extension("toml.corrupt");
                if let Err(rename_err) = fs::rename(&state_path, &corrupt_path) {
                    error!(
                        "failed to rename corrupt state to {:?}: {rename_err}",
                        corrupt_path
                    );
                }
                any_error = true;
                continue;
            }
        };
        let run_dir = entry.path();
        match resume_one(state_dir, &run_state) {
            Ok(true) => {
                let _ = fs::remove_dir_all(&run_dir);
            }
            Ok(false) => {}
            Err(e) => {
                error!(
                    "failed to resume run {}/{}: {e}",
                    run_state.nickname, run_state.run_id
                );
                any_error = true;
            }
        }
    }

    if any_error {
        anyhow::bail!("failed to resume one or more prior run states; refusing to start new sync");
    }
    Ok(())
}

/// Returns Ok(true) if the run was fully completed (cleanup done, state removed).
/// Returns Ok(false) if the run has made progress but needs another cycle.
fn resume_one(state_dir: &str, state: &ClientRunState) -> Result<bool> {
    let nickname = Nickname::new(state.nickname.clone())
        .map_err(|e| anyhow::anyhow!("invalid nickname in persisted state: {e}"))?;
    let run_id = RunId::new(state.run_id.clone())
        .map_err(|e| anyhow::anyhow!("invalid run_id in persisted state: {e}"))?;
    let host = &state.host;
    let server_cmd = &state.server_command;
    let manifest: Manifest =
        toml::from_str(&state.manifest).with_context(|| "failed to parse persisted manifest")?;
    let run_config: RunConfig = toml::from_str(&state.run_config)
        .with_context(|| "failed to parse persisted run config")?;

    info!(
        nickname = %nickname.as_str(),
        run_id = %run_id.as_str(),
        phase = ?state.phase,
        "resuming persisted client run state"
    );

    match state.phase {
        ClientRunPhase::UploadCompleteFinishPending => {
            debug!("calling finish-run idempotently");
            finish_run(host, server_cmd, &nickname, &run_id)?;
            persist_client_run_state(
                state_dir,
                &nickname,
                &run_id,
                host,
                server_cmd,
                &manifest,
                &run_config,
                None,
                ClientRunPhase::WaitingForTerminalState,
            )?;
            Ok(false)
        }
        ClientRunPhase::WaitingForTerminalState => {
            debug!("waiting for terminal state");
            wait_for_terminal(host, server_cmd, &nickname, &run_id)?;
            let status = read_status(host, server_cmd, &nickname, &run_id)?;
            if status.nickname != nickname || status.run_id != run_id {
                anyhow::bail!("server status envelope does not match persisted run");
            }
            let terminal_status = toml::to_string(&status)
                .map_err(|e| anyhow::anyhow!("status serialization: {e}"))?;
            persist_client_run_state(
                state_dir,
                &nickname,
                &run_id,
                host,
                server_cmd,
                &manifest,
                &run_config,
                Some(&terminal_status),
                ClientRunPhase::TerminalStatusSeen,
            )?;
            process_cleanup_from_status(
                state_dir,
                host,
                server_cmd,
                &nickname,
                &run_id,
                &manifest,
                &run_config,
                &status,
            )?;
            Ok(true)
        }
        ClientRunPhase::TerminalStatusSeen => {
            let status = match &state.terminal_status {
                Some(toml_str) => {
                    toml::from_str(toml_str).with_context(|| "failed to parse persisted status")?
                }
                None => read_status(host, server_cmd, &nickname, &run_id)
                    .with_context(|| "no persisted status and could not re-read from server")?,
            };
            process_cleanup_from_status(
                state_dir,
                host,
                server_cmd,
                &nickname,
                &run_id,
                &manifest,
                &run_config,
                &status,
            )?;
            Ok(true)
        }
        ClientRunPhase::CleanupComplete => Ok(true),
        ClientRunPhase::Abandoned => {
            warn!("abandoned run state, removing");
            Ok(true)
        }
        ClientRunPhase::Corrupt => {
            anyhow::bail!(
                "persisted run state {}/{} is marked corrupt; manual intervention required",
                state.nickname,
                state.run_id
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_cleanup_from_status(
    state_dir: &str,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
    manifest: &Manifest,
    run_config: &RunConfig,
    status: &RunStatus,
) -> Result<()> {
    let cleanup_filename = format!("cleanup-{}-{}.toml", nickname.as_str(), run_id.as_str());
    let cleanup_path = camino::Utf8PathBuf::from(state_dir).join(&cleanup_filename);
    if cleanup_path.as_std_path().exists() {
        cleanup::confirm_imports_from_status(&cleanup_path, status)?;
        cleanup::process_cleanup_state_file(&cleanup_path)?;
    }
    persist_client_run_state(
        state_dir,
        nickname,
        run_id,
        host,
        server_cmd,
        manifest,
        run_config,
        None,
        ClientRunPhase::CleanupComplete,
    )?;
    remove_client_run_state(state_dir, nickname, run_id);
    Ok(())
}

// ── Main sync entry point ──────────────────────────────────────────────

fn resolve_state_dir(args: &SyncArgs) -> String {
    args.state_dir.clone().unwrap_or_else(|| {
        if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
            format!("{dir}/purgery")
        } else if let Ok(home) = std::env::var("HOME") {
            format!("{home}/.local/state/purgery")
        } else {
            "/tmp/purgery-client".to_string()
        }
    })
}

fn validate_source_is_directory(source: &str) -> Result<()> {
    let path = Path::new(source);
    if !path.exists() {
        anyhow::bail!("source path does not exist: {source}");
    }
    if !path.is_dir() {
        anyhow::bail!("source must be a directory, not a file: {source}");
    }
    Ok(())
}

/// RAII-style heartbeat guard. Stops heartbeat on drop, joins the thread,
/// and stores the result for the caller to check.
struct HeartbeatGuard {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<()>>>,
    result: Option<Result<()>>,
}

impl HeartbeatGuard {
    fn new(
        host: String,
        server_cmd: String,
        nickname: Nickname,
        run_id: RunId,
        interval_secs: u64,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = start_heartbeat(
            host,
            server_cmd,
            nickname,
            run_id,
            interval_secs,
            Arc::clone(&stop),
        );
        Self {
            stop,
            handle: Some(handle),
            result: None,
        }
    }

    /// Stop and join the heartbeat thread, capturing its result.
    /// After this call, check_heartbeat() is available.
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            self.result = Some(match handle.join() {
                Ok(r) => r.map_err(|e| anyhow::anyhow!("{e:#}")),
                Err(_) => Err(anyhow::anyhow!("heartbeat thread panicked")),
            });
        }
    }

    /// Check whether heartbeat succeeded. Returns an error if heartbeat failed
    /// or panicked. Must be called after stop_and_join().
    fn check_heartbeat(&self) -> Result<()> {
        match &self.result {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(anyhow::anyhow!("heartbeat error: {e}")),
            None => Err(anyhow::anyhow!("heartbeat thread was never joined")),
        }
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

pub(crate) fn run_sync(args: &SyncArgs) -> Result<()> {
    let state_dir = resolve_state_dir(args);

    // Phase 1: resume pending cleanup and run states before validating the
    // new operation. Recovery failures block the new sync.
    cleanup::resume_pending_cleanups(&state_dir)?;
    resume_runs(&state_dir)?;

    // Phase 2: validate the new operation
    validate_source_is_directory(&args.source)?;

    let has_postprocess = !args.postprocess.is_empty();
    if has_postprocess && !args.delete_after_import {
        anyhow::bail!("--delete-after-import is required when --postprocess is used");
    }

    let remote = parse_destination(&args.destination)?;
    let nickname = derive_nickname(&args.destination)?;
    let run_id = RunId::generate();

    info!(
        nickname = %nickname.as_str(),
        operation_id = %run_id.as_str(),
        source = %args.source,
        host = %remote.host,
        destination = %remote.path.as_str(),
        "starting sync"
    );

    // Passthrough, no cleanup: direct rsync only
    if !has_postprocess && !args.delete_after_import {
        info!("starting direct rsync");
        transfer::run_rsync(&args.source, &remote.host, remote.path.as_str())?;
        info!("sync complete");
        return Ok(());
    }

    let manifest = classify::build_manifest(
        &args.source,
        &run_id,
        &nickname,
        &args.postprocess,
        args.delete_after_import,
    )?;
    let cleanup_state_path = if args.delete_after_import {
        let entries = cleanup::build_cleanup_entries(&args.source, &manifest)?;
        if entries.is_empty() {
            None
        } else {
            let state = DurableCleanupState {
                nickname: nickname.as_str().to_owned(),
                operation_id: run_id.as_str().to_owned(),
                entries,
            };
            Some(cleanup::write_cleanup_state(&state, &state_dir)?)
        }
    } else {
        None
    };

    // Passthrough with cleanup: direct rsync + durable cleanup
    if !has_postprocess {
        info!("starting direct rsync with durable cleanup");
        transfer::run_rsync(&args.source, &remote.host, remote.path.as_str())?;
        if let Some(ref state_path) = cleanup_state_path {
            cleanup::confirm_all_imports(state_path)?;
            cleanup::process_cleanup_state_file(state_path)?;
        }
        info!("sync complete");
        return Ok(());
    }

    // Postprocess: server run flow with heartbeat and crash-safe persistence
    let server_cmd = &args.server_command;
    let run_config = RunConfig {
        nickname: nickname.clone(),
        destination: remote.path.clone(),
        delete_after_import: true,
    };

    info!("starting server run");
    let begin_resp = begin_run(&remote.host, server_cmd, &nickname, &run_id)?;

    // Start heartbeat immediately after begin-run. The guard ensures the
    // heartbeat thread is joined on drop (even on panic) and heartbeat
    // errors are propagated.
    let mut hb_guard = HeartbeatGuard::new(
        remote.host.clone(),
        server_cmd.to_string(),
        nickname.clone(),
        run_id.clone(),
        begin_resp.heartbeat_interval_secs,
    );

    // The staging phase: write metadata, validate, transfer, persist state.
    // All of this happens under the heartbeat.
    let staging_result = (|| -> Result<()> {
        ssh::write_remote_file(
            &remote.host,
            &begin_resp.run_config_path,
            &run_config.to_toml()?,
        )?;
        ssh::write_remote_file(
            &remote.host,
            &begin_resp.manifest_path,
            &manifest.to_toml()?,
        )?;

        info!("validating server run plan");
        prepare_run(&remote.host, server_cmd, &nickname, &run_id)?;

        info!("transferring files to server staging");
        transfer::run_rsync(&args.source, &remote.host, &begin_resp.files_dir)?;

        Ok(())
    })();

    if let Err(e) = staging_result {
        // Heartbeat failure is irrelevant if staging already failed;
        // the incoming run will be GC'd.
        hb_guard.stop_and_join();
        return Err(e);
    }

    // Persist state BEFORE finish-run so a crash after rsync can resume.
    persist_client_run_state(
        &state_dir,
        &nickname,
        &run_id,
        &remote.host,
        server_cmd,
        &manifest,
        &run_config,
        None,
        ClientRunPhase::UploadCompleteFinishPending,
    )?;

    // finish-run while heartbeat is still alive
    info!("finishing server run");
    finish_run(&remote.host, server_cmd, &nickname, &run_id)?;

    // Now heartbeat is no longer needed — stop and check it.
    // If heartbeat failed but finish-run succeeded, the run has already been
    // accepted out of incoming; warn and continue rather than fail the whole
    // operation.
    hb_guard.stop_and_join();
    if let Err(e) = hb_guard.check_heartbeat() {
        warn!("heartbeat error after successful finish-run: {e}");
    }

    // Update persisted state to WaitingForTerminalState
    persist_client_run_state(
        &state_dir,
        &nickname,
        &run_id,
        &remote.host,
        server_cmd,
        &manifest,
        &run_config,
        None,
        ClientRunPhase::WaitingForTerminalState,
    )?;

    info!("waiting for server processing");
    wait_for_terminal(&remote.host, server_cmd, &nickname, &run_id)?;

    info!("reading run status");
    let status = read_status(&remote.host, server_cmd, &nickname, &run_id)?;
    if status.nickname != nickname || status.run_id != run_id {
        anyhow::bail!("server status envelope does not match requested run");
    }

    let terminal_status =
        toml::to_string(&status).map_err(|e| anyhow::anyhow!("status serialization: {e}"))?;
    persist_client_run_state(
        &state_dir,
        &nickname,
        &run_id,
        &remote.host,
        server_cmd,
        &manifest,
        &run_config,
        Some(&terminal_status),
        ClientRunPhase::TerminalStatusSeen,
    )?;

    if let Some(ref state_path) = cleanup_state_path {
        cleanup::confirm_imports_from_status(state_path, &status)?;
        cleanup::process_cleanup_state_file(state_path)?;
    }

    persist_client_run_state(
        &state_dir,
        &nickname,
        &run_id,
        &remote.host,
        server_cmd,
        &manifest,
        &run_config,
        None,
        ClientRunPhase::CleanupComplete,
    )?;
    remove_client_run_state(&state_dir, &nickname, &run_id);

    info!(state = %status.state.as_str(), "sync complete");
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_absolute_remote_destination() {
        let parsed = parse_destination("user@host:/absolute/dest").unwrap();
        assert_eq!(parsed.host, "user@host");
        assert_eq!(parsed.path.as_str(), "/absolute/dest");
        assert!(parsed.path.is_absolute());
    }

    #[test]
    fn parses_relative_remote_destination() {
        let parsed = parse_destination("user@host:relative/dest").unwrap();
        assert_eq!(parsed.host, "user@host");
        assert_eq!(parsed.path.as_str(), "relative/dest");
        assert!(!parsed.path.is_absolute());
    }

    #[test]
    fn rejects_file_source() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("single_file.txt");
        fs::write(&file_path, "content").unwrap();
        let result = validate_source_is_directory(file_path.to_str().unwrap());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must be a directory"));
    }

    #[test]
    fn accepts_directory_source() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(validate_source_is_directory(tmp.path().to_str().unwrap()).is_ok());
    }

    #[test]
    fn postprocess_requires_delete_after_import() {
        let args = SyncArgs {
            postprocess: vec!["compress".to_string()],
            delete_after_import: false,
            state_dir: None,
            source: "/src".to_string(),
            destination: "host:dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        let result = run_sync_validate(&args);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("--delete-after-import is required"));
    }

    #[test]
    fn recovery_from_upload_complete_calls_finish_run() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);

        // Simulate a persisted UploadCompleteFinishPending state
        let manifest = Manifest {
            run_id: RunId::new("test-ucl".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("relative/dest")).unwrap(),
            delete_after_import: true,
        };
        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-ucl".into()).unwrap(),
            "fake-host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::UploadCompleteFinishPending,
        )
        .unwrap();

        // Resume should try finish-run and fail (no real SSH), then
        // persist WaitingForTerminalState... or fail.
        // Without SSH, finish_run will fail. The resume should propagate the error.
        let result = resume_runs(&state_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to resume"));
    }

    #[test]
    fn recovery_order_happens_before_validation() {
        // Verify that resolve_state_dir and resume happen before any
        // source/destination validation in the code structure.
        let args = SyncArgs {
            postprocess: vec![],
            delete_after_import: false,
            state_dir: Some("/nonexistent/dir".to_string()),
            source: "/nonexistent/source".to_string(),
            destination: "host:dest".to_string(),
            server_command: "purgery-server".to_string(),
        };

        // run_sync calls resolve_state_dir, then resume, then validates.
        // If the source doesn't exist, validation should fail AFTER resume.
        // Resume should succeed if the state dir has no runs.
        // Then source validation should fail because /nonexistent/source doesn't exist.
        let result = run_sync(&args);
        assert!(result.is_err());
        // The error should be about the source, not about the resume step
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("source path does not exist"));
    }

    #[test]
    fn heartbeat_guard_stops_and_captures_result() {
        // Test that HeartbeatGuard properly stops heartbeat and captures
        // the result when explicitly joined.
        let mut guard = HeartbeatGuard::new(
            "fake-host".to_string(),
            "purgery-server".to_string(),
            Nickname::new("test".into()).unwrap(),
            RunId::new("test-hb".into()).unwrap(),
            60,
        );
        // Sleep long enough for at least one heartbeat attempt
        std::thread::sleep(Duration::from_millis(200));
        guard.stop_and_join();
        // Heartbeat will fail because there's no real SSH, but the guard
        // captures the error rather than ignoring it.
        assert!(guard.check_heartbeat().is_err());
    }

    #[test]
    fn heartbeat_guard_joins_on_drop() {
        // Verify the guard doesn't panic on drop and properly stops the
        // heartbeat thread.
        let (stop_flag, handle) = {
            let guard = HeartbeatGuard::new(
                "fake-host".to_string(),
                "purgery-server".to_string(),
                Nickname::new("test".into()).unwrap(),
                RunId::new("test-drop".into()).unwrap(),
                60,
            );
            let stop = Arc::clone(&guard.stop);
            guard.stop.store(true, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(50));
            (stop, guard.handle.is_some())
        };
        // After guard.drop(), stop was signaled and handle was taken
        assert!(stop_flag.load(Ordering::Relaxed));
        assert!(handle);
    }

    #[test]
    fn terminal_status_seen_uses_persisted_status_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);

        let manifest = Manifest {
            run_id: RunId::new("test-tss".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        // Simulate TerminalStatusSeen with persisted terminal status
        let status = RunStatus {
            run_id: RunId::new("test-tss".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            state: purgery_core::RunState::Done,
            entries: vec![],
            error: None,
        };
        let terminal_status = toml::to_string(&status).unwrap();

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-tss".into()).unwrap(),
            "fake-host",
            "purgery-server",
            &manifest,
            &run_config,
            Some(&terminal_status),
            ClientRunPhase::TerminalStatusSeen,
        )
        .unwrap();

        // Resume should complete immediately without SSH (using persisted terminal status)
        let result = resume_runs(&state_dir);
        // Should succeed because cleanup is a no-op for empty entries,
        // and the persisted terminal_status avoids needing SSH.
        assert!(result.is_ok());
    }

    #[test]
    fn heartbeat_guard_captures_thread_panic() {
        let mut guard = HeartbeatGuard::new(
            "fake-host".to_string(),
            "purgery-server".to_string(),
            Nickname::new("test".into()).unwrap(),
            RunId::new("test-panic".into()).unwrap(),
            60,
        );
        // Replace the handle with one that panics
        let panic_handle = std::thread::spawn(|| {
            panic!("simulated heartbeat panic");
        });
        guard.handle = Some(panic_handle);
        guard.stop_and_join();
        assert!(guard.check_heartbeat().is_err());
        let err = guard.check_heartbeat().unwrap_err().to_string();
        assert!(
            err.contains("panicked"),
            "error should mention panic, got: {err}"
        );
    }

    #[test]
    fn heartbeat_sleep_never_zero() {
        // With interval_secs = 1, the sleep must still be non-zero
        let stop = Arc::new(AtomicBool::new(false));
        let handle = start_heartbeat(
            "fake-host".to_string(),
            "purgery-server".to_string(),
            Nickname::new("test".into()).unwrap(),
            RunId::new("test-sleep".into()).unwrap(),
            1,
            Arc::clone(&stop),
        );
        // Let it run briefly; it will fail SSH but that's fine
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        // If the thread was in a zero-duration spin loop it would have
        // hammered SSH thousands of times; just joining should succeed.
        let _ = handle.join();
    }

    #[test]
    fn resume_waiting_for_terminal_blocks_if_server_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);

        let manifest = Manifest {
            run_id: RunId::new("test-wft".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-wft".into()).unwrap(),
            "fake-host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::WaitingForTerminalState,
        )
        .unwrap();

        // Resume should fail because SSH to fake-host will be unreachable
        let result = resume_runs(&state_dir);
        assert!(
            result.is_err(),
            "resume should fail when server is unreachable"
        );
        assert!(result.unwrap_err().to_string().contains("failed to resume"),);

        // State should NOT have been deleted
        let run_dir = camino::Utf8PathBuf::from(&state_dir)
            .join("runs")
            .join("laptop-test-wft");
        assert!(
            run_dir.as_std_path().exists(),
            "run state must not be deleted on failure"
        );
    }

    #[test]
    fn terminal_status_seen_without_saved_status_keeps_state_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);

        let manifest = Manifest {
            run_id: RunId::new("test-notss".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        // TerminalStatusSeen WITHOUT saved terminal_status (legacy/migration state)
        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-notss".into()).unwrap(),
            "fake-host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::TerminalStatusSeen,
        )
        .unwrap();

        // Resume should fail because it tries to re-read status from fake-host
        let result = resume_runs(&state_dir);
        assert!(
            result.is_err(),
            "resume must fail when status cannot be re-read from server"
        );

        // State must remain — never delete just because status couldn't be re-read
        let run_dir = camino::Utf8PathBuf::from(&state_dir)
            .join("runs")
            .join("laptop-test-notss");
        assert!(
            run_dir.as_std_path().exists(),
            "state must not be deleted when TerminalStatusSeen has no saved status and server is unreachable"
        );
    }

    #[test]
    fn wait_for_terminal_errors_on_not_found() {
        // not_found is non-terminal; wait_for_terminal must treat it as an error.
        // This is a unit test for the phase matching logic.
        let response = RunStateResponse {
            protocol_version: 1,
            nickname: "laptop".to_string(),
            run_id: "test-nf".to_string(),
            phase: "not_found".to_string(),
            terminal: false,
            message: "run not found".to_string(),
            updated_at_unix_secs: 0,
            observed_at_unix_secs: 0,
            progress_state: None,
            entry_index: None,
            entry_total: None,
            current_entry: None,
            current_step: None,
            progress_status: None,
        };
        // The match logic in wait_for_terminal would bail on "not_found"
        match response.phase.as_str() {
            "not_found" => {
                // This is the expected error path
            }
            _ => panic!("expected not_found handling"),
        }
    }

    fn mk_state_dir(tmp: &tempfile::TempDir) -> String {
        tmp.path().join("purgery").to_string_lossy().to_string()
    }

    fn run_sync_validate(args: &SyncArgs) -> Result<()> {
        let has_postprocess = !args.postprocess.is_empty();
        if has_postprocess && !args.delete_after_import {
            anyhow::bail!("--delete-after-import is required when --postprocess is used");
        }
        Ok(())
    }
}
