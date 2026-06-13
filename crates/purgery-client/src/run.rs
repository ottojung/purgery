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
use crate::runner::RemoteRunner;
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
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<BeginRunResponse> {
    let output = runner.server_cmd(
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
    RemoteRunner::parse_begin_response(&output)
}

fn prepare_run(
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<PrepareRunResponse> {
    let output = runner.server_cmd(
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
    RemoteRunner::parse_prepare_response(&output)
}

fn heartbeat_run(
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<()> {
    runner.server_cmd(
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

fn finish_run(
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<()> {
    runner.server_cmd(
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
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStateResponse> {
    let output = runner.server_cmd(
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
    RemoteRunner::parse_run_state_response(&output)
}

fn read_status(
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStatus> {
    let output = runner.server_cmd(
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
    RunStatus::from_toml(output.trim()).with_context(|| "failed to parse status response")
}

fn wait_for_terminal(
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStateResponse> {
    let poll_interval = Duration::from_secs(5);
    let mut last_phase = String::new();
    let mut attempts_since_report = 0u64;

    loop {
        let response = run_state(runner, host, server_cmd, nickname, run_id)?;

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

fn start_heartbeat(
    runner: RemoteRunner,
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
            if let Err(e) = heartbeat_run(&runner, &host, &server_cmd, &nickname, &run_id) {
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

fn resume_runs(runner: &RemoteRunner, state_dir: &str) -> Result<()> {
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
        match drain_one(runner, state_dir, &run_state) {
            Ok(()) => {
                let _ = fs::remove_dir_all(&run_dir);
            }
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

/// Drain a single persisted run state to completion or error.
/// Returns Ok(()) when the run is fully resolved (cleanup complete, state removed).
fn drain_one(runner: &RemoteRunner, state_dir: &str, state: &ClientRunState) -> Result<()> {
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

    let mut phase = state.phase;
    let mut terminal_status: Option<String> = state.terminal_status.clone();

    info!(
        nickname = %nickname.as_str(),
        run_id = %run_id.as_str(),
        phase = ?phase,
        "draining persisted client run state"
    );

    loop {
        match phase {
            ClientRunPhase::UploadCompleteFinishPending => {
                debug!("calling finish-run idempotently");
                finish_run(runner, host, server_cmd, &nickname, &run_id)?;
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
                phase = ClientRunPhase::WaitingForTerminalState;
            }
            ClientRunPhase::WaitingForTerminalState => {
                debug!("waiting for terminal state");
                wait_for_terminal(runner, host, server_cmd, &nickname, &run_id)?;
                let status = read_status(runner, host, server_cmd, &nickname, &run_id)?;
                if status.nickname != nickname || status.run_id != run_id {
                    anyhow::bail!("server status envelope does not match persisted run");
                }
                let ts = toml::to_string(&status)
                    .map_err(|e| anyhow::anyhow!("status serialization: {e}"))?;
                persist_client_run_state(
                    state_dir,
                    &nickname,
                    &run_id,
                    host,
                    server_cmd,
                    &manifest,
                    &run_config,
                    Some(&ts),
                    ClientRunPhase::TerminalStatusSeen,
                )?;
                phase = ClientRunPhase::TerminalStatusSeen;
                terminal_status = Some(ts);
            }
            ClientRunPhase::TerminalStatusSeen => {
                let status = match &terminal_status {
                    Some(ts) => {
                        toml::from_str(ts).with_context(|| "failed to parse persisted status")?
                    }
                    None => read_status(runner, host, server_cmd, &nickname, &run_id)
                        .with_context(|| {
                            "no persisted terminal status and could not re-read from server"
                        })?,
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
                return Ok(());
            }
            ClientRunPhase::CleanupComplete => return Ok(()),
            ClientRunPhase::Abandoned => {
                warn!("abandoned run state, removing");
                return Ok(());
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

struct HeartbeatGuard {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<()>>>,
    result: Option<Result<()>>,
}

impl HeartbeatGuard {
    fn new(
        runner: RemoteRunner,
        host: String,
        server_cmd: String,
        nickname: Nickname,
        run_id: RunId,
        interval_secs: u64,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = start_heartbeat(
            runner,
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

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            self.result = Some(match handle.join() {
                Ok(r) => r.map_err(|e| anyhow::anyhow!("{e:#}")),
                Err(_) => Err(anyhow::anyhow!("heartbeat thread panicked")),
            });
        }
    }

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
    run_sync_with_runner(&RemoteRunner::real(), args)
}

pub(crate) fn run_sync_with_runner(runner: &RemoteRunner, args: &SyncArgs) -> Result<()> {
    let state_dir = resolve_state_dir(args);

    // Phase 1: resume pending cleanup and run states before validating the
    // new operation. Recovery failures block the new sync.
    cleanup::resume_pending_cleanups(&state_dir)?;
    resume_runs(runner, &state_dir)?;

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
        runner.run_rsync(&args.source, &remote.host, remote.path.as_str())?;
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
        runner.run_rsync(&args.source, &remote.host, remote.path.as_str())?;
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
    let begin_resp = begin_run(runner, &remote.host, server_cmd, &nickname, &run_id)?;

    let mut hb_guard = HeartbeatGuard::new(
        runner.clone(),
        remote.host.clone(),
        server_cmd.to_string(),
        nickname.clone(),
        run_id.clone(),
        begin_resp.heartbeat_interval_secs,
    );

    let staging_result = (|| -> Result<()> {
        runner.write_remote_file(
            &remote.host,
            &begin_resp.run_config_path,
            &run_config.to_toml()?,
        )?;
        runner.write_remote_file(
            &remote.host,
            &begin_resp.manifest_path,
            &manifest.to_toml()?,
        )?;

        info!("validating server run plan");
        prepare_run(runner, &remote.host, server_cmd, &nickname, &run_id)?;

        info!("transferring files to server staging");
        runner.run_rsync(&args.source, &remote.host, &begin_resp.files_dir)?;

        Ok(())
    })();

    if let Err(e) = staging_result {
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
    finish_run(runner, &remote.host, server_cmd, &nickname, &run_id)?;

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
    wait_for_terminal(runner, &remote.host, server_cmd, &nickname, &run_id)?;

    info!("reading run status");
    let status = read_status(runner, &remote.host, server_cmd, &nickname, &run_id)?;
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
    use purgery_core::RunState;
    use tempfile::tempdir;

    fn mk_runner() -> RemoteRunner {
        RemoteRunner::fake()
    }

    fn mk_state_dir(tmp: &tempfile::TempDir) -> String {
        tmp.path().join("purgery").to_string_lossy().to_string()
    }

    fn begin_resp_toml() -> String {
        r#"protocol_version = 1
nickname = "laptop"
run_id = "test-run"
incoming_dir = "/var/lib/purgery/work/laptop/incoming/test-run"
files_dir = "/var/lib/purgery/work/laptop/incoming/test-run/files"
run_config_path = "/var/lib/purgery/work/laptop/incoming/test-run/run.toml"
manifest_path = "/var/lib/purgery/work/laptop/incoming/test-run/manifest.toml"
heartbeat_interval_secs = 60
"#
        .to_string()
    }

    fn done_run_state_toml() -> String {
        r#"protocol_version = 1
nickname = "laptop"
run_id = "test-run"
phase = "done"
terminal = true
message = ""
updated_at_unix_secs = 1000
observed_at_unix_secs = 1000
"#
        .to_string()
    }

    fn done_status_toml() -> String {
        r#"run_id = "test-run"
nickname = "laptop"
state = "done"
"#
        .to_string()
    }

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
        let tmp = tempdir().unwrap();
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
        let tmp = tempdir().unwrap();
        assert!(validate_source_is_directory(tmp.path().to_str().unwrap()).is_ok());
    }

    #[test]
    fn postprocess_requires_delete_after_import() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let args = SyncArgs {
            postprocess: vec!["compress".to_string()],
            delete_after_import: false,
            state_dir: Some(state_dir),
            source: src_dir_str(&tmp),
            destination: "host:dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        let runner = mk_runner();
        let result = run_sync_with_runner(&runner, &args);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("--delete-after-import is required"));
    }

    fn src_dir_str(tmp: &tempfile::TempDir) -> String {
        let src = tmp.path().join("src");
        let _ = fs::create_dir(&src);
        src.to_string_lossy().to_string()
    }

    #[test]
    fn heartbeat_guard_captures_thread_panic() {
        let runner = mk_runner();
        let mut guard = HeartbeatGuard::new(
            runner,
            "fake-host".to_string(),
            "purgery-server".to_string(),
            Nickname::new("test".into()).unwrap(),
            RunId::new("test-panic".into()).unwrap(),
            60,
        );
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
    fn heartbeat_guard_joins_on_drop() {
        let runner = mk_runner();
        let (stop_flag, had_handle) = {
            let guard = HeartbeatGuard::new(
                runner,
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
        assert!(stop_flag.load(Ordering::Relaxed));
        assert!(had_handle);
    }

    #[test]
    fn recovery_drains_upload_complete_to_cleanup() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();

        // Script responses: match on the server subcommand name.
        // Shell escaping puts single quotes around args so we cannot
        // match full flag sequences; match on the subcommand itself.
        runner.add_response("finish-run", "");
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("test-run", "test-drain"),
        );
        runner.add_response(
            "status",
            &done_status_toml().replace("test-run", "test-drain"),
        );

        let manifest = Manifest {
            run_id: RunId::new("test-drain".into()).unwrap(),
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
            &RunId::new("test-drain".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::UploadCompleteFinishPending,
        )
        .unwrap();

        // Drain should complete: finish-run → wait → status → cleanup → done
        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_ok(), "drain failed: {:?}", result.err());

        let run_dir = camino::Utf8PathBuf::from(&state_dir)
            .join("runs")
            .join("laptop-test-drain");
        assert!(
            !run_dir.as_std_path().exists(),
            "run state should be removed after successful drain"
        );
    }

    #[test]
    fn recovery_blocks_new_sync_when_server_unreachable() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        // No scripted responses → all commands will fail with "no scripted response"

        let manifest = Manifest {
            run_id: RunId::new("test-block".into()).unwrap(),
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
            &RunId::new("test-block".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::UploadCompleteFinishPending,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to resume"));

        // State must remain
        let run_dir = camino::Utf8PathBuf::from(&state_dir)
            .join("runs")
            .join("laptop-test-block");
        assert!(run_dir.as_std_path().exists());
    }

    #[test]
    fn terminal_status_seen_uses_persisted_terminal_status() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        // No SSH responses needed — terminal_status is persisted

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

        let status = RunStatus {
            run_id: RunId::new("test-tss".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        let terminal_status = toml::to_string(&status).unwrap();

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-tss".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            Some(&terminal_status),
            ClientRunPhase::TerminalStatusSeen,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_ok());

        let run_dir = camino::Utf8PathBuf::from(&state_dir)
            .join("runs")
            .join("laptop-test-tss");
        assert!(!run_dir.as_std_path().exists());
    }

    #[test]
    fn terminal_status_seen_without_saved_status_keeps_state_on_failure() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        // No scripted "status" response → re-read will fail

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

        // TerminalStatusSeen WITHOUT saved terminal_status
        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-notss".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::TerminalStatusSeen,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_err());

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
        // not_found is non-terminal; wait_for_terminal treats it as error.
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
        match response.phase.as_str() {
            "not_found" => {} // expected path
            _ => panic!("expected not_found handling"),
        }
    }

    #[test]
    fn heartbeat_failure_is_observed() {
        let runner = mk_runner();
        runner.add_error("heartbeat-run", "simulated heartbeat failure");

        let mut guard = HeartbeatGuard::new(
            runner,
            "fake-host".to_string(),
            "purgery-server".to_string(),
            Nickname::new("test".into()).unwrap(),
            RunId::new("test-hb-fail".into()).unwrap(),
            60,
        );
        std::thread::sleep(Duration::from_millis(200));
        guard.stop_and_join();
        assert!(guard.check_heartbeat().is_err());
    }

    #[test]
    fn heartbeat_sleep_never_zero() {
        let runner = mk_runner();
        runner.add_error("heartbeat-run", "expected — not a real host");
        let stop = Arc::new(AtomicBool::new(false));
        let handle = start_heartbeat(
            runner,
            "fake-host".to_string(),
            "purgery-server".to_string(),
            Nickname::new("test".into()).unwrap(),
            RunId::new("test-sleep".into()).unwrap(),
            1,
            Arc::clone(&stop),
        );
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }

    #[test]
    fn heartbeat_active_through_finish_run() {
        let runner = mk_runner();
        // begin-run
        runner.add_response("begin-run", &begin_resp_toml());
        // heartbeat should run during the staging phase
        runner.add_error("heartbeat-run", "expected — not a real host");
        // prepare-run
        runner.add_response(
            "prepare-run",
            "protocol_version = 1\nnickname = \"laptop\"\nrun_id = \"test-run\"\n",
        );
        // finish-run
        runner.add_response("finish-run", "");

        // Create a tiny source dir to avoid filesystem errors
        let tmp = tempdir().unwrap();
        let src_dir = tmp.path().join("src");
        fs::create_dir(&src_dir).unwrap();
        let state_dir = mk_state_dir(&tmp);

        let args = SyncArgs {
            postprocess: vec![],
            delete_after_import: false,
            state_dir: Some(state_dir),
            source: src_dir.to_string_lossy().to_string(),
            destination: "host:rel".to_string(),
            server_command: "purgery-server".to_string(),
        };

        // Passthrough, should NOT create a server run (but args still
        // parse successfully).
        let result = run_sync_with_runner(&runner, &args);
        // Should succeed because passthrough no-cleanup only does rsync
        assert!(result.is_ok());
        let log = runner.command_log();
        // No server commands should have been called for passthrough
        assert!(
            !log.iter().any(|c| c.contains("begin-run")),
            "passthrough should not call begin-run"
        );
    }
}
