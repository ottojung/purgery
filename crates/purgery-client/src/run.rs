use anyhow::{Context, Result};
use purgery_core::{
    BeginRunResponse, ClientRunPhase, ClientRunState, DestinationPath, DurableCleanupState,
    Manifest, Nickname, PrepareRunResponse, RunConfig, RunId, RunStateResponse, RunStatus,
};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::classify;
use crate::cleanup;
use crate::runner::RemoteRunner;
use crate::split;
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

#[allow(clippy::too_many_arguments)]
fn start_heartbeat(
    runner: RemoteRunner,
    host: String,
    server_cmd: String,
    nickname: Nickname,
    run_id: RunId,
    interval_secs: u64,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
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
                state.store(HB_FAILED, Ordering::Relaxed);
                return Err(e);
            }
            state.store(HB_HEALTHY, Ordering::Relaxed);
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
                        let s: purgery_core::RunStatus = toml::from_str(ts)
                            .with_context(|| "failed to parse persisted status")?;
                        if s.nickname != nickname || s.run_id != run_id {
                            anyhow::bail!(
                                "persisted terminal status envelope does not match: \
                                 expected {}/{} but got {}/{}",
                                nickname.as_str(),
                                run_id.as_str(),
                                s.nickname.as_str(),
                                s.run_id.as_str()
                            );
                        }
                        s
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
    } else if run_config.delete_after_import && !manifest.entries.is_empty() {
        anyhow::bail!(
            "cleanup state file '{cleanup_filename}' is required for run {}/{} \
             with delete_after_import but is missing or corrupt",
            nickname.as_str(),
            run_id.as_str()
        );
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

#[cfg_attr(not(test), allow(dead_code))]
fn validate_source_exists(source: &str) -> Result<()> {
    let path = Path::new(source);
    if std::fs::symlink_metadata(path).is_err() {
        anyhow::bail!("source path does not exist: {source}");
    }
    Ok(())
}

// Tracked via AtomicU8 rather than AtomicBool so is_healthy can
// distinguish "not yet started" from "healthy" from "failed".
const HB_NOT_STARTED: u8 = 0;
const HB_HEALTHY: u8 = 1;
const HB_FAILED: u8 = 2;

struct HeartbeatGuard {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<()>>>,
    result: Option<Result<()>>,
    state: Arc<AtomicU8>,
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
        let state = Arc::new(AtomicU8::new(HB_NOT_STARTED));
        let handle = start_heartbeat(
            runner,
            host,
            server_cmd,
            nickname,
            run_id,
            interval_secs,
            Arc::clone(&stop),
            Arc::clone(&state),
        );
        Self {
            stop,
            handle: Some(handle),
            result: None,
            state,
        }
    }

    fn is_healthy(&self) -> Result<()> {
        // Wait for at least one heartbeat attempt so we see a definitive
        // health signal, then check without stopping the thread.
        while self.state.load(Ordering::Relaxed) == HB_NOT_STARTED {
            std::thread::sleep(Duration::from_millis(1));
        }
        if self.state.load(Ordering::Relaxed) == HB_FAILED {
            Err(anyhow::anyhow!("heartbeat failed"))
        } else {
            Ok(())
        }
    }

    fn stop_and_join(&mut self) {
        // Wait for at least one heartbeat attempt so the join captures a
        // definitive result rather than the thread exiting immediately.
        while self.state.load(Ordering::Relaxed) == HB_NOT_STARTED {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            self.result = Some(match handle.join() {
                Ok(r) => r.map_err(|e| anyhow::anyhow!("{e:#}")),
                Err(_) => Err(anyhow::anyhow!("heartbeat thread panicked")),
            });
        }
    }

    #[cfg(test)]
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
    let run_id = RunId::generate();
    run_sync_with_run_id(runner, args, &run_id)
}

pub(crate) fn run_sync_with_run_id(
    runner: &RemoteRunner,
    args: &SyncArgs,
    run_id: &RunId,
) -> Result<()> {
    let state_dir = resolve_state_dir(args);

    // Phase 1: resume pending cleanup and run states before validating the
    // new operation. Recovery failures block the new sync.
    cleanup::resume_pending_cleanups(&state_dir)?;
    resume_runs(runner, &state_dir)?;

    // Phase 2: normalize the source. This validates existence, rejects "/",
    // resolves "."/"..", and strips trailing slashes so every downstream
    // path uses the same normalized operation_path and source_entry_name.
    let source_spec = classify::normalize_source(&args.source)?;

    let has_postprocess = !args.postprocess.is_empty();
    if has_postprocess && !args.delete_after_import {
        anyhow::bail!("--delete-after-import is required when --postprocess is used");
    }

    if let Some(ref _pattern) = args.split {
        return run_split(runner, args, run_id, &state_dir, &source_spec);
    }

    let remote = parse_destination(&args.destination)?;
    let nickname = derive_nickname(&args.destination)?;

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
        runner.run_rsync(
            &source_spec.operation_path,
            &remote.host,
            remote.path.as_str(),
        )?;
        info!("sync complete");
        return Ok(());
    }

    let manifest = classify::build_manifest(&source_spec, run_id, &nickname, &args.postprocess)?;
    let cleanup_state_path = if args.delete_after_import {
        let entries = classify::capture_cleanup_identity(&source_spec)?;
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
        runner.run_rsync(
            &source_spec.operation_path,
            &remote.host,
            remote.path.as_str(),
        )?;
        if let Some(ref state_path) = cleanup_state_path {
            cleanup::confirm_all_imports(state_path)?;
            cleanup::process_cleanup_state_file(state_path)?;
        }
        info!("sync complete");
        return Ok(());
    }

    // Postprocess: server run flow with heartbeat and crash-safe persistence
    let server_cmd = &args.server_command;
    let mut run_config = RunConfig {
        nickname: nickname.clone(),
        destination: remote.path.clone(),
        delete_after_import: true,
    };

    info!("starting server run");
    let begin_resp = begin_run(runner, &remote.host, server_cmd, &nickname, run_id)?;

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
        let prepare_resp = prepare_run(runner, &remote.host, server_cmd, &nickname, run_id)?;

        if let Some(ref dest) = prepare_resp.destination {
            run_config = RunConfig {
                nickname: nickname.clone(),
                destination: DestinationPath::new(camino::Utf8PathBuf::from(dest))
                    .with_context(|| "server returned invalid resolved destination")?,
                delete_after_import: true,
            };
        }

        info!("transferring files to server staging");
        runner.run_rsync(
            &source_spec.operation_path,
            &remote.host,
            &begin_resp.files_dir,
        )?;

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
        run_id,
        &remote.host,
        server_cmd,
        &manifest,
        &run_config,
        None,
        ClientRunPhase::UploadCompleteFinishPending,
    )?;

    // Check heartbeat health without stopping it. If the lease expired
    // during staging, bail before finish-run and leave recoverable state.
    hb_guard.is_healthy()?;

    info!("finishing server run");
    finish_run(runner, &remote.host, server_cmd, &nickname, run_id)?;

    // Stop heartbeat only after finish-run succeeded — the lease must
    // cover the entire incoming phase.
    hb_guard.stop_and_join();

    // Update persisted state to WaitingForTerminalState
    persist_client_run_state(
        &state_dir,
        &nickname,
        run_id,
        &remote.host,
        server_cmd,
        &manifest,
        &run_config,
        None,
        ClientRunPhase::WaitingForTerminalState,
    )?;

    info!("waiting for server processing");
    wait_for_terminal(runner, &remote.host, server_cmd, &nickname, run_id)?;

    info!("reading run status");
    let status = read_status(runner, &remote.host, server_cmd, &nickname, run_id)?;
    if status.nickname != nickname || status.run_id != *run_id {
        anyhow::bail!("server status envelope does not match requested run");
    }

    let terminal_status =
        toml::to_string(&status).map_err(|e| anyhow::anyhow!("status serialization: {e}"))?;
    persist_client_run_state(
        &state_dir,
        &nickname,
        run_id,
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
        run_id,
        &remote.host,
        server_cmd,
        &manifest,
        &run_config,
        None,
        ClientRunPhase::CleanupComplete,
    )?;
    remove_client_run_state(&state_dir, &nickname, run_id);

    info!(state = %status.state.as_str(), "sync complete");
    Ok(())
}

/// Handle --split mode.
fn run_split(
    runner: &RemoteRunner,
    args: &SyncArgs,
    _run_id: &RunId,
    state_dir: &str,
    source_spec: &classify::SourceSpec,
) -> Result<()> {
    let has_postprocess = !args.postprocess.is_empty();
    let entry_roots =
        split::discover_split_entries(&source_spec.operation_path, args.split.as_deref().unwrap())
            .map_err(|e| anyhow::anyhow!("split discovery failed: {e}"))?;
    if entry_roots.is_empty() {
        info!("split pattern matched nothing");
        return Ok(());
    }
    let target = parse_destination(&args.destination)?;
    if !has_postprocess && !args.delete_after_import {
        return run_passthrough_split(
            runner,
            &source_spec.operation_path,
            args.split.as_deref().unwrap(),
            &target.host,
            target.path.as_str(),
        );
    }
    let base_dest = &args.destination;
    for root in &entry_roots {
        let suffix = split::split_target_suffix(&source_spec.operation_path, &root.path);
        let split_dest = format!("{}{}", base_dest, suffix);
        info!(source = %root.path, destination = %split_dest, "processing split entry");
        let split_args = SyncArgs {
            postprocess: args.postprocess.clone(),
            delete_after_import: args.delete_after_import,
            split: None,
            state_dir: Some(state_dir.to_owned()),
            server_command: args.server_command.clone(),
            source: root.path.clone(),
            destination: split_dest,
        };
        let split_run_id = RunId::generate();
        run_sync_with_run_id(runner, &split_args, &split_run_id)?;
    }
    Ok(())
}

fn run_passthrough_split(
    runner: &RemoteRunner,
    source: &str,
    pattern: &str,
    host: &str,
    remote_dir: &str,
) -> Result<()> {
    let filters = split::build_split_filters(pattern);
    match filters {
        None => {
            // --split "." : source entry itself matched, use ordinary rsync.
            info!("passthrough split: transferring source entry");
            runner.run_rsync(source, host, remote_dir)?;
        }
        Some(f) => {
            info!(
                "passthrough split: transferring with filter rules: includes={:?} exclude={}",
                f.include_rules, f.exclude_rule
            );
            runner.run_rsync_filter_transfer(
                source,
                host,
                remote_dir,
                &f.include_rules,
                &f.exclude_rule,
            )?;
        }
    }
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use purgery_core::RunState;
    use std::sync::Mutex;
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
    fn rejects_nonexistent_source() {
        let result = validate_source_exists("/nonexistent/path");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn accepts_file_source() {
        let tmp = tempdir().unwrap();
        let file_path = tmp.path().join("single_file.txt");
        fs::write(&file_path, "content").unwrap();
        assert!(validate_source_exists(file_path.to_str().unwrap()).is_ok());
    }

    #[test]
    fn accepts_directory_source() {
        let tmp = tempdir().unwrap();
        assert!(validate_source_exists(tmp.path().to_str().unwrap()).is_ok());
    }

    #[test]
    fn postprocess_requires_delete_after_import() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let args = SyncArgs {
            postprocess: vec!["compress".to_string()],
            delete_after_import: false,
            split: None,
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
        // Replace the handle and mark state as healthy so
        // stop_and_join doesn't hang waiting for the original thread.
        guard.handle = Some(panic_handle);
        guard.state.store(HB_HEALTHY, Ordering::Relaxed);
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
        let state = Arc::new(AtomicU8::new(HB_NOT_STARTED));
        let handle = start_heartbeat(
            runner,
            "fake-host".to_string(),
            "purgery-server".to_string(),
            Nickname::new("test".into()).unwrap(),
            RunId::new("test-sleep".into()).unwrap(),
            1,
            Arc::clone(&stop),
            Arc::clone(&state),
        );
        while state.load(Ordering::Relaxed) == HB_NOT_STARTED {
            std::thread::sleep(Duration::from_millis(1));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }

    #[test]
    fn heartbeat_active_through_finish_run() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        let begin = begin_resp_toml().replace(
            "heartbeat_interval_secs = 60",
            "heartbeat_interval_secs = 1",
        );
        runner.add_response("begin-run", &begin);
        runner.add_response(
            "prepare-run",
            "protocol_version = 1\nnickname = \"laptop\"\nrun_id = \"test-run\"\n",
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("run-state", &done_run_state_toml());
        let status_toml =
            "run_id = \"test-run\"\nnickname = \"laptop\"\nstate = \"done\"\n".to_string();
        runner.add_response("status", &status_toml);
        // Prove finish-run is called with heartbeat still alive: set a hook
        // that records when the guard is stopped, then verify the guard was
        // alive during finish-run.
        let finish_called = Arc::new(AtomicBool::new(false));
        let finish_called_clone = Arc::clone(&finish_called);
        runner.set_finish_run_hook(Box::new(move || {
            finish_called_clone.store(true, Ordering::Relaxed);
        }));
        // Add finish-run response AFTER the hook (responses consumed FIFO)
        runner.add_response("finish-run", "");

        let result = run_sync_with_run_id(&runner, &args, &run_id);
        assert!(result.is_ok(), "sync must succeed");

        // finish-run hook fired → finish-run was called
        assert!(finish_called.load(Ordering::Relaxed));
        // heartbeat must have run — if it had failed, is_healthy would
        // have rejected it before finish-run
        let log = runner.command_log();
        assert!(log.iter().any(|c| c.contains("heartbeat-run")));
    }

    fn src_with_file(tmp: &tempfile::TempDir) -> String {
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("file.txt"), "data").unwrap();
        src.to_string_lossy().to_string()
    }

    fn postprocess_args(tmp: &tempfile::TempDir, state_dir: &str) -> SyncArgs {
        SyncArgs {
            postprocess: vec!["transform".to_string()],
            delete_after_import: true,
            split: None,
            state_dir: Some(state_dir.to_owned()),
            source: src_with_file(tmp),
            destination: "laptop:rel".to_string(),
            server_command: "purgery-server".to_string(),
        }
    }

    #[test]
    fn write_error_during_staging_stops_client() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_write_error("run.toml", "simulated write failure");

        let result = run_sync_with_runner(&runner, &args);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated write failure"));
    }

    #[test]
    fn write_error_prevents_prepare_rsync_finish() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_write_error("manifest.toml", "write failed");

        let result = run_sync_with_runner(&runner, &args);
        assert!(result.is_err());

        let log = runner.command_log();
        assert!(log.iter().any(|c| c.contains("begin-run")));
        assert!(!log.iter().any(|c| c.contains("prepare-run")));
        assert!(
            !log.iter().any(|c| c.contains("rsync")),
            "rsync should not run after write failure"
        );
        assert!(!log.iter().any(|c| c.contains("finish-run")));
    }

    #[test]
    fn rsync_error_during_staging_stops_client() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            "protocol_version = 1\nnickname = \"laptop\"\nrun_id = \"test-run\"\n",
        );
        runner.add_rsync_error("laptop", "simulated rsync failure");

        let result = run_sync_with_runner(&runner, &args);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated rsync failure"));
    }

    #[test]
    fn rsync_error_prevents_finish_run() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            "protocol_version = 1\nnickname = \"laptop\"\nrun_id = \"test-run\"\n",
        );
        runner.add_rsync_error("laptop", "simulated rsync failure");

        let result = run_sync_with_runner(&runner, &args);
        assert!(result.is_err());

        let log = runner.command_log();
        assert!(log.iter().any(|c| c.contains("prepare-run")));
        assert!(!log.iter().any(|c| c.contains("finish-run")));
    }

    #[test]
    fn write_error_before_persist_does_not_write_state() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_write_error("run.toml", "write failed");

        let _ = run_sync_with_runner(&runner, &args);

        let runs_dir = camino::Utf8PathBuf::from(&state_dir).join("runs");
        assert!(
            !runs_dir.as_std_path().exists()
                || runs_dir.as_std_path().read_dir().unwrap().count() == 0,
            "no persisted run state should exist before UploadCompleteFinishPending"
        );
    }

    #[test]
    fn finish_run_failure_leaves_upload_complete_pending() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            "protocol_version = 1\nnickname = \"laptop\"\nrun_id = \"test-run\"\n",
        );
        runner.add_error("finish-run", "simulated finish failure");

        let result = run_sync_with_runner(&runner, &args);
        assert!(result.is_err());

        // Check that UploadCompleteFinishPending was persisted
        let runs_dir = camino::Utf8PathBuf::from(&state_dir).join("runs");
        assert!(runs_dir.as_std_path().exists());
        let mut found = false;
        for entry in fs::read_dir(runs_dir.as_std_path()).unwrap() {
            let entry = entry.unwrap();
            let state_path = entry.path().join("state.toml");
            if state_path.exists() {
                let content = fs::read_to_string(&state_path).unwrap();
                if content.contains("upload_complete_finish_pending") {
                    found = true;
                }
            }
        }
        assert!(found, "UploadCompleteFinishPending state must be persisted");
    }

    #[test]
    fn heartbeat_failure_before_finish_run_prevents_finish_run() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);

        let begin = begin_resp_toml().replace(
            "heartbeat_interval_secs = 60",
            "heartbeat_interval_secs = 1",
        );
        runner.add_response("begin-run", &begin);
        runner.add_response(
            "prepare-run",
            "protocol_version = 1\nnickname = \"laptop\"\nrun_id = \"test-run\"\n",
        );
        runner.add_error("heartbeat-run", "simulated heartbeat failure");

        let result = run_sync_with_runner(&runner, &args);
        assert!(result.is_err());
        let err_text = result.unwrap_err().to_string();
        assert!(
            err_text.contains("heartbeat"),
            "error must mention heartbeat, got: {err_text}"
        );

        let log = runner.command_log();
        assert!(log.iter().any(|c| c.contains("begin-run")));
        assert!(log.iter().any(|c| c.contains("prepare-run")));
        assert!(log.iter().any(|c| c.contains("rsync")));
        assert!(
            !log.iter().any(|c| c.contains("finish-run")),
            "finish-run must NOT be called when heartbeat failed"
        );

        // UploadCompleteFinishPending must be persisted for recovery
        let runs_dir = camino::Utf8PathBuf::from(&state_dir).join("runs");
        assert!(runs_dir.as_std_path().exists());
        let mut found = false;
        for entry in fs::read_dir(runs_dir.as_std_path()).unwrap() {
            let entry = entry.unwrap();
            let state_path = entry.path().join("state.toml");
            if state_path.exists() {
                if let Ok(content) = fs::read_to_string(&state_path) {
                    if content.contains("upload_complete_finish_pending") {
                        found = true;
                    }
                }
            }
        }
        assert!(
            found,
            "UploadCompleteFinishPending must be persisted when heartbeat fails"
        );
    }

    #[test]
    fn heartbeat_runs_concurrently_with_staging() {
        // Uses a blocking rsync hook to prove the heartbeat thread fires
        // before finish-run, without relying on scheduling timing.
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        let begin = begin_resp_toml().replace(
            "heartbeat_interval_secs = 60",
            "heartbeat_interval_secs = 1",
        );
        runner.add_response("begin-run", &begin);
        runner.add_response(
            "prepare-run",
            "protocol_version = 1\nnickname = \"laptop\"\nrun_id = \"test-run\"\n",
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("run-state", &done_run_state_toml());
        let status_toml =
            "run_id = \"test-run\"\nnickname = \"laptop\"\nstate = \"done\"\n".to_string();
        runner.add_response("status", &status_toml);
        runner.add_response("finish-run", "");

        // Block rsync so we can verify heartbeat has fired while staging
        // is still in progress.
        let rsync_reached = Arc::new(AtomicBool::new(false));
        let rsync_reached_clone = Arc::clone(&rsync_reached);
        let proceed = Arc::new(AtomicBool::new(false));
        let proceed_clone = Arc::clone(&proceed);
        runner.set_rsync_hook(Box::new(move || {
            rsync_reached_clone.store(true, Ordering::Relaxed);
            while !proceed_clone.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
        }));

        // Run sync in background thread
        let runner_for_sync = runner.clone();
        let result = Arc::new(Mutex::new(None::<Result<()>>));
        let result_clone = Arc::clone(&result);
        let sync_handle = std::thread::spawn(move || {
            *result_clone.lock().unwrap() =
                Some(run_sync_with_run_id(&runner_for_sync, &args, &run_id));
        });

        // Wait for rsync hook to fire (staging in progress)
        while !rsync_reached.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }

        // Heartbeat thread started at HeartbeatGuard creation (after
        // begin-run). By now it should have fired at least once.
        std::thread::sleep(Duration::from_millis(50));
        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("heartbeat-run")),
            "heartbeat must have fired while rsync is blocked (staging in progress)"
        );

        // Let rsync complete
        proceed.store(true, Ordering::Relaxed);
        sync_handle.join().unwrap();

        let final_result = result.lock().unwrap().take().unwrap();
        assert!(
            final_result.is_ok(),
            "sync should succeed when heartbeat is healthy"
        );

        let final_log = runner.command_log();
        let finish_pos = final_log.iter().position(|c| c.contains("finish-run"));
        assert!(finish_pos.is_some(), "finish-run must be called");
        let hb_positions: Vec<_> = final_log
            .iter()
            .enumerate()
            .filter(|(_, c)| c.contains("heartbeat-run"))
            .map(|(i, _)| i)
            .collect();
        assert!(
            !hb_positions.is_empty(),
            "heartbeat must have run at least once"
        );
        assert!(
            hb_positions.iter().all(|&p| p < finish_pos.unwrap()),
            "all heartbeats must occur before finish-run"
        );
    }

    #[test]
    fn client_updates_run_config_with_resolved_destination() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = postprocess_args(&tmp, &state_dir);
        let run_id = RunId::new("test-resolved-dest".into()).unwrap();

        let begin = begin_resp_toml().replace(
            "heartbeat_interval_secs = 60",
            "heartbeat_interval_secs = 1",
        );
        runner.add_response("begin-run", &begin);
        // Server returns a resolved absolute destination.
        runner.add_response(
            "prepare-run",
            "protocol_version = 1\nnickname = \"laptop\"\nrun_id = \"test-resolved-dest\"\n\
             destination = \"/server/resolved/absolute/path\"\n",
        );
        runner.add_response("heartbeat-run", "");
        // Make finish-run fail so UploadCompleteFinishPending persists.
        runner.add_error("finish-run", "simulated finish failure");

        let result = run_sync_with_run_id(&runner, &args, &run_id);
        assert!(result.is_err(), "sync must fail when finish-run fails");

        // The persisted UploadCompleteFinishPending state must contain
        // the resolved absolute destination from prepare-run.
        let runs_dir = camino::Utf8PathBuf::from(&state_dir).join("runs");
        let mut found = false;
        for entry in fs::read_dir(runs_dir.as_std_path()).unwrap() {
            let entry = entry.unwrap();
            let state_path = entry.path().join("state.toml");
            if state_path.exists() {
                let content = fs::read_to_string(&state_path).unwrap();
                if content.contains("/server/resolved/absolute/path") {
                    found = true;
                }
            }
        }
        assert!(
            found,
            "persisted UploadCompleteFinishPending must contain resolved destination"
        );
    }

    // ── Source normalization integration tests ──

    #[test]
    fn trailing_slash_source_uses_normalized_operand_in_rsync() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("Videos");
        fs::create_dir(&src).unwrap();
        // Use trailing slash to exercise normalization.
        let src_slash = format!("{}/", src.to_str().unwrap());
        let args = SyncArgs {
            postprocess: vec![],
            delete_after_import: false,
            split: None,
            state_dir: Some(state_dir),
            source: src_slash,
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        run_sync_with_runner(&runner, &args).unwrap();
        let log = runner.command_log();
        assert_eq!(
            log.len(),
            1,
            "passthrough should produce exactly one command"
        );
        let rsync_cmd = &log[0];
        // rsync source operand must NOT have trailing slash.
        let after_sep: Vec<&str> = rsync_cmd.split(" -- ").collect();
        assert_eq!(after_sep.len(), 2);
        let operands: Vec<&str> = after_sep[1].split_whitespace().collect();
        let source_op = operands[0];
        assert!(
            !source_op.ends_with('/'),
            "rsync source operand must not have trailing slash, got: {source_op}"
        );
    }

    #[test]
    fn trailing_slash_passthrough_cleanup_uses_normalized_name() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("Videos");
        fs::create_dir(&src).unwrap();
        let src_slash = format!("{}/", src.to_str().unwrap());
        let args = SyncArgs {
            postprocess: vec![],
            delete_after_import: true,
            split: None,
            state_dir: Some(state_dir.clone()),
            source: src_slash,
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        run_sync_with_runner(&runner, &args).unwrap();
        // Cleanup state file must use the normalized source entry name.
        let cleanup_files: Vec<_> =
            fs::read_dir(camino::Utf8PathBuf::from(&state_dir).as_std_path())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("cleanup-"))
                })
                .collect();
        assert!(
            !cleanup_files.is_empty(),
            "cleanup state file must be created"
        );
    }

    #[test]
    fn postprocess_trailing_slash_stages_as_files_source_name() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("Videos");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.mp4"), "data").unwrap();
        let src_slash = format!("{}/", src.to_str().unwrap());
        let args = SyncArgs {
            postprocess: vec!["transform".to_string()],
            delete_after_import: true,
            split: None,
            state_dir: Some(state_dir.clone()),
            source: src_slash,
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        let run_id = RunId::new("test-run".into()).unwrap();

        // Destination is "host:/dest" → nickname = "host"
        let begin = begin_resp_toml().replace("laptop", "host").replace(
            "heartbeat_interval_secs = 60",
            "heartbeat_interval_secs = 1",
        );
        runner.add_response("begin-run", &begin);
        runner.add_response(
            "prepare-run",
            "protocol_version = 1\nnickname = \"host\"\nrun_id = \"test-run\"\n",
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("laptop", "host"),
        );
        let status_toml =
            "run_id = \"test-run\"\nnickname = \"host\"\nstate = \"done\"\n".to_string();
        runner.add_response("status", &status_toml);
        runner.add_response("finish-run", "");

        run_sync_with_run_id(&runner, &args, &run_id).unwrap();

        let written = runner.written_files();
        let manifest_content = written
            .values()
            .find(|v| v.contains("files/Videos"))
            .expect("manifest must contain staged_path files/Videos");
        assert!(
            manifest_content.contains("files/Videos"),
            "staged path must be files/Videos for trailing-slash source"
        );
    }

    #[test]
    fn root_path_rejected_in_all_modes() {
        // Pure passthrough
        {
            let tmp = tempdir().unwrap();
            let runner = mk_runner();
            let args = SyncArgs {
                postprocess: vec![],
                delete_after_import: false,
                split: None,
                state_dir: Some(mk_state_dir(&tmp)),
                source: "/".to_string(),
                destination: "host:/dest".to_string(),
                server_command: "purgery-server".to_string(),
            };
            let result = run_sync_with_runner(&runner, &args);
            assert!(
                result.is_err(),
                "/ must be rejected in pure passthrough mode"
            );
            assert!(result.unwrap_err().to_string().contains("root"));
        }
        // Passthrough cleanup
        {
            let tmp = tempdir().unwrap();
            let runner = mk_runner();
            let args = SyncArgs {
                postprocess: vec![],
                delete_after_import: true,
                split: None,
                state_dir: Some(mk_state_dir(&tmp)),
                source: "/".to_string(),
                destination: "host:/dest".to_string(),
                server_command: "purgery-server".to_string(),
            };
            let result = run_sync_with_runner(&runner, &args);
            assert!(
                result.is_err(),
                "/ must be rejected in passthrough cleanup mode"
            );
            assert!(result.unwrap_err().to_string().contains("root"));
        }
        // Postprocess
        {
            let tmp = tempdir().unwrap();
            let runner = mk_runner();
            let args = SyncArgs {
                postprocess: vec!["transform".to_string()],
                delete_after_import: true,
                split: None,
                state_dir: Some(mk_state_dir(&tmp)),
                source: "/".to_string(),
                destination: "host:/dest".to_string(),
                server_command: "purgery-server".to_string(),
            };
            let result = run_sync_with_runner(&runner, &args);
            assert!(result.is_err(), "/ must be rejected in postprocess mode");
            assert!(result.unwrap_err().to_string().contains("root"));
        }
        // Split
        {
            let tmp = tempdir().unwrap();
            let runner = mk_runner();
            let args = SyncArgs {
                postprocess: vec![],
                delete_after_import: false,
                split: Some("*.mp4".to_string()),
                state_dir: Some(mk_state_dir(&tmp)),
                source: "/".to_string(),
                destination: "host:/dest".to_string(),
                server_command: "purgery-server".to_string(),
            };
            let result = run_sync_with_runner(&runner, &args);
            assert!(result.is_err(), "/ must be rejected in split mode");
            assert!(result.unwrap_err().to_string().contains("root"));
        }
    }

    #[test]
    fn pure_passthrough_split_transfers_only_selected_roots() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.mp4"), "mp4").unwrap();
        fs::write(src.join("b.txt"), "txt").unwrap();
        fs::create_dir(src.join("sub")).unwrap();
        fs::write(src.join("sub/c.mp4"), "mp4").unwrap();

        let args = SyncArgs {
            postprocess: vec![],
            delete_after_import: false,
            split: Some("*.mp4".to_string()),
            state_dir: Some(state_dir),
            source: src.to_str().unwrap().to_string(),
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        run_sync_with_runner(&runner, &args).unwrap();

        let log = runner.command_log();
        assert_eq!(
            log.len(),
            1,
            "pure passthrough split must use exactly one rsync process"
        );
        assert!(
            !log.iter().any(|c| c.contains("ssh")),
            "pure passthrough split must not use ssh"
        );

        let rule_sets = runner.filter_rule_sets();
        assert_eq!(rule_sets.len(), 1, "must record one rule set");
        let (includes, exclude) = &rule_sets[0];
        assert!(
            includes.contains(&"*/".to_string()),
            "must include '*/' for directory traversal"
        );
        assert!(
            includes.contains(&"*.mp4/***".to_string()),
            "must include '*.mp4/***' for directory payload"
        );
        assert!(
            includes.contains(&"*.mp4".to_string()),
            "must include '*.mp4' for entry selection"
        );
        assert_eq!(exclude, "*", "must exclude '*'");
    }

    #[test]
    fn pure_passthrough_split_no_server_run_no_cleanup() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.mp4"), "data").unwrap();

        let args = SyncArgs {
            postprocess: vec![],
            delete_after_import: false,
            split: Some("*.mp4".to_string()),
            state_dir: Some(state_dir.clone()),
            source: src.to_str().unwrap().to_string(),
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        run_sync_with_runner(&runner, &args).unwrap();

        let log = runner.command_log();
        assert!(
            !log.iter().any(|c| c.contains("ssh")),
            "pure passthrough split must not create a server run"
        );
        // No cleanup state files should exist
        let cleanup_files: Vec<_> =
            fs::read_dir(camino::Utf8PathBuf::from(&state_dir).as_std_path())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("cleanup-"))
                })
                .collect();
        assert!(
            cleanup_files.is_empty(),
            "pure passthrough split must not create cleanup state"
        );
    }

    #[test]
    fn passthrough_split_dot_uses_ordinary_rsync_no_trailing_slash() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.mp4"), "data").unwrap();

        let args = SyncArgs {
            postprocess: vec![],
            delete_after_import: false,
            split: Some(".".to_string()),
            state_dir: Some(state_dir),
            source: src.to_str().unwrap().to_string(),
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        run_sync_with_runner(&runner, &args).unwrap();

        let log = runner.command_log();
        assert_eq!(log.len(), 1, "must use exactly one rsync process");
        let cmd = &log[0];
        // --split "." uses ordinary rsync; source should NOT have trailing slash
        assert!(cmd.contains("--recursive"), "must include --recursive");
        assert!(cmd.contains("--archive"), "must include --archive");
        assert!(
            cmd.contains(format!("-- {}", src.to_str().unwrap()).as_str()),
            "source operand should not have trailing slash for --split '.'"
        );
    }

    #[test]
    fn passthrough_split_filter_mode_uses_source_with_trailing_slash() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.mp4"), "data").unwrap();
        fs::write(src.join("b.txt"), "text").unwrap();

        let args = SyncArgs {
            postprocess: vec![],
            delete_after_import: false,
            split: Some("*.mp4".to_string()),
            state_dir: Some(state_dir),
            source: src.to_str().unwrap().to_string(),
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        run_sync_with_runner(&runner, &args).unwrap();

        let log = runner.command_log();
        assert_eq!(log.len(), 1);
        let cmd = &log[0];
        // Filter mode: source operand must have trailing slash
        assert!(
            cmd.contains(format!(" {}/", src.to_str().unwrap()).as_str()),
            "source operand must have trailing slash in filter mode"
        );
        assert!(
            cmd.contains("--include='*/'"),
            "must include directory traversal rule"
        );
        assert!(cmd.contains("--exclude='*'"), "must include exclude rule");
        assert!(
            cmd.contains("--prune-empty-dirs"),
            "must include -m to prune traversal scaffolding"
        );
    }

    #[test]
    fn passthrough_split_star_does_not_transfer_unrelated_files() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.mp4"), "mp4-content").unwrap();
        fs::write(src.join("b.txt"), "txt-content").unwrap();
        fs::write(src.join("sub/c.mp4"), "nested-mp4").unwrap();

        let args = SyncArgs {
            postprocess: vec![],
            delete_after_import: false,
            split: Some("*.mp4".to_string()),
            state_dir: Some(state_dir),
            source: src.to_str().unwrap().to_string(),
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };
        run_sync_with_runner(&runner, &args).unwrap();

        let log = runner.command_log();
        assert_eq!(log.len(), 1);
        let cmd = &log[0];
        assert!(cmd.contains("--include='*.mp4'"));
        assert!(cmd.contains("--exclude='*'"));
    }
}
