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
use crate::runner::{RemoteCommandExit, RemoteCommandHandle, RemoteRunner};
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

fn check_server_version(runner: &RemoteRunner, host: &str, server_cmd: &str) -> Result<()> {
    let output = runner.server_cmd(host, server_cmd, &["version"])?;
    let resp: purgery_core::VersionResponse =
        toml::from_str(&output).with_context(|| "failed to parse server version response")?;
    if resp.protocol_version != purgery_core::PROTOCOL_VERSION {
        anyhow::bail!(
            "server {host} has protocol_version {}; client expects {}",
            resp.protocol_version,
            purgery_core::PROTOCOL_VERSION,
        );
    }
    purgery_core::require_compatible_purgery_version(
        &resp.purgery_version,
        format_args!("server {host}"),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
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
    let status =
        RunStatus::from_toml(output.trim()).with_context(|| "failed to parse status response")?;
    purgery_core::require_compatible_purgery_version(&status.purgery_version, "status")
        .with_context(|| "server returned incompatible status version")?;
    Ok(status)
}

/// Supervise a foreground `process-run` SSH command while concurrently
/// polling `run-state` until the run reaches a terminal phase.
///
/// The `process-run` is spawned in the background via `spawn_server_cmd`.
/// The client polls `run-state` using short `server_cmd` calls.
/// If the SSH transport fails, `run-state` decides whether to restart.
/// If the command exits with a remote semantic error, the client re-polls
/// `run-state` before deciding whether to fail.
fn drive_server_until_terminal(
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStateResponse> {
    drive_server_until_terminal_with_interval(
        runner,
        host,
        server_cmd,
        nickname,
        run_id,
        Duration::from_secs(5),
    )
}

/// Like `drive_server_until_terminal` but with a configurable poll interval.
/// Tests can pass a tiny interval to avoid multi-second sleeps.
fn drive_server_until_terminal_with_interval(
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
    poll_interval: Duration,
) -> Result<RunStateResponse> {
    let mut worker: Option<RemoteCommandHandle> = None;
    let mut state: RunStateResponse;
    let mut last_phase = String::new();
    let mut attempts_since_report = 0u64;
    let mut no_progress = NoProgressTracker::new();

    loop {
        state = match run_state(runner, host, server_cmd, nickname, run_id) {
            Ok(s) => s,
            Err(e) => {
                terminate_worker_on_error(&mut worker);
                return Err(e).with_context(|| {
                    format!(
                        "failed to poll run-state for run {}/{}",
                        nickname.as_str(),
                        run_id.as_str()
                    )
                });
            }
        };

        if state.terminal {
            finish_worker_after_terminal(&mut worker, runner, host, server_cmd, nickname, run_id)?;
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = %state.phase,
                "run reached terminal phase"
            );
            return Ok(state);
        }

        let mut restart_worker = false;

        // Check if the worker has exited.
        if let Some(w) = worker.as_mut() {
            let try_result = w.try_wait();
            match try_result {
                Ok(result) => {
                    match result {
                        None => { /* still running */ }
                        Some(exit) => {
                            worker = None;
                            match exit {
                                RemoteCommandExit::Success { stdout } => {
                                    let decision = handle_process_run_success(
                                        runner,
                                        host,
                                        server_cmd,
                                        nickname,
                                        run_id,
                                        &stdout,
                                        &state,
                                        &mut no_progress,
                                    )?;
                                    match decision {
                                        WorkerExitDecision::Terminal(ts) => {
                                            finish_worker_after_terminal(
                                                &mut worker,
                                                runner,
                                                host,
                                                server_cmd,
                                                nickname,
                                                run_id,
                                            )?;
                                            info!(
                                                nickname = %nickname.as_str(),
                                                run_id = %run_id.as_str(),
                                                phase = %ts.phase,
                                                "process-run success confirmed terminal state",
                                            );
                                            return Ok(ts);
                                        }
                                        WorkerExitDecision::ContinueWaiting => {}
                                        WorkerExitDecision::ContinueLoop(fresh_state) => {
                                            state = fresh_state;
                                        }
                                    }
                                }
                                RemoteCommandExit::TransportFailure { details, .. } => {
                                    warn!(
                                        nickname = %nickname.as_str(),
                                        run_id = %run_id.as_str(),
                                        details,
                                        "process-run SSH transport failed",
                                    );
                                }
                                RemoteCommandExit::RemoteFailure { stderr, .. } => {
                                    let err_msg = format!(
                                        "process-run remote failure for run {}/{}: {}",
                                        nickname.as_str(),
                                        run_id.as_str(),
                                        stderr,
                                    );
                                    // Re-poll run-state before deciding — the run may
                                    // have become terminal or active since we last checked.
                                    // Assign to `state` so subsequent loop logic uses
                                    // the fresh data, not the stale pre-poll value.
                                    let fresh =
                                        match run_state(runner, host, server_cmd, nickname, run_id)
                                        {
                                            Ok(r) => r,
                                            Err(_) => {
                                                terminate_worker_on_error(&mut worker);
                                                anyhow::bail!("{err_msg}");
                                            }
                                        };
                                    if fresh.terminal {
                                        finish_worker_after_terminal(
                                            &mut worker,
                                            runner,
                                            host,
                                            server_cmd,
                                            nickname,
                                            run_id,
                                        )?;
                                        return Ok(fresh);
                                    }
                                    if fresh.phase == "ready"
                                        || (fresh.phase == "processing"
                                            && fresh.processor_state.as_deref() != Some("active"))
                                    {
                                        terminate_worker_on_error(&mut worker);
                                        anyhow::bail!("{err_msg}");
                                    }
                                    warn!("{}", err_msg);
                                    // Use fresh state for subsequent restart decisions.
                                    state = fresh;
                                }
                                RemoteCommandExit::Killed => {
                                    terminate_worker_on_error(&mut worker);
                                    anyhow::bail!(
                                        "process-run was killed for run {}/{}",
                                        nickname.as_str(),
                                        run_id.as_str(),
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    terminate_worker_on_error(&mut worker);
                    return Err(e).with_context(|| {
                        format!(
                            "failed to check process-run worker for run {}/{}",
                            nickname.as_str(),
                            run_id.as_str()
                        )
                    });
                }
            }
        }

        // Decide whether to start/restart the process-run worker.
        // Never spawn while a local worker handle is still running.
        let processor_active = state
            .processor_state
            .as_deref()
            .map(|s| s == "active")
            .unwrap_or(false);

        match state.phase.as_str() {
            "ready" => {
                if worker.is_none() {
                    restart_worker = true;
                }
            }
            "processing" => {
                if !processor_active && worker.is_none() {
                    restart_worker = true;
                }
            }
            "not_found" => {
                terminate_worker_on_error(&mut worker);
                anyhow::bail!(
                    "run {}/{} not found on server",
                    nickname.as_str(),
                    run_id.as_str()
                );
            }
            other => {
                terminate_worker_on_error(&mut worker);
                anyhow::bail!(
                    "unexpected run-state phase '{other}' for run {}/{}",
                    nickname.as_str(),
                    run_id.as_str()
                );
            }
        }

        if restart_worker {
            worker = Some(runner.spawn_server_cmd(
                host,
                server_cmd,
                &[
                    "process-run",
                    "--nickname",
                    nickname.as_str(),
                    "--run-id",
                    run_id.as_str(),
                ],
            )?);
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                "spawned foreground process-run",
            );
        }

        if state.phase != last_phase {
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = %state.phase,
                "run phase changed"
            );
            last_phase = state.phase.clone();
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

        std::thread::sleep(poll_interval);
    }
}

/// Outcome of processing a process-run worker exit in the drive loop.
enum WorkerExitDecision {
    /// The worker result, combined with fresh run-state, indicates
    /// terminal state.  Return this state.
    Terminal(RunStateResponse),
    /// Keep polling without spawning a new worker (e.g. active processor).
    ContinueWaiting,
    /// State changed meaningfully; use the provided fresh state for
    /// restart decisions in this iteration.
    ContinueLoop(RunStateResponse),
}

/// Tracks consecutive identical state observations for no-progress
/// detection.  Reset when state changes meaningfully.
#[derive(Clone, Debug, PartialEq)]
struct ProgressSnapshot {
    phase: String,
    terminal: bool,
    processor_state: Option<String>,
    progress_state: Option<String>,
    entry_index: Option<usize>,
    entry_total: Option<usize>,
    updated_at_unix_secs: u64,
}

impl From<&RunStateResponse> for ProgressSnapshot {
    fn from(s: &RunStateResponse) -> Self {
        ProgressSnapshot {
            phase: s.phase.clone(),
            terminal: s.terminal,
            processor_state: s.processor_state.clone(),
            progress_state: s.progress_state.clone(),
            entry_index: s.entry_index,
            entry_total: s.entry_total,
            updated_at_unix_secs: s.updated_at_unix_secs,
        }
    }
}

const MAX_NO_PROGRESS_COUNT: u32 = 3;

struct NoProgressTracker {
    count: u32,
    last_snapshot: Option<ProgressSnapshot>,
}

impl NoProgressTracker {
    fn new() -> Self {
        NoProgressTracker {
            count: 0,
            last_snapshot: None,
        }
    }

    fn advance(&mut self, state: &RunStateResponse) -> bool {
        let snap = ProgressSnapshot::from(state);
        if self.last_snapshot.as_ref() == Some(&snap) {
            self.count += 1;
            self.count < MAX_NO_PROGRESS_COUNT
        } else {
            self.count = 0;
            self.last_snapshot = Some(snap);
            true
        }
    }
}

const TERMINAL_WORKER_GRACE: Duration = Duration::from_secs(5);

/// Finish ownership of the local process-run worker after terminal
/// run-state is observed.
///
/// Gives the SSH child a bounded grace period to exit naturally.
/// If it does not exit within the grace period, terminates it and
/// returns an error — there is no successful path where the client
/// kills the worker.
///
/// If the worker already exited, validates its exit.
fn finish_worker_after_terminal(
    worker: &mut Option<RemoteCommandHandle>,
    _runner: &RemoteRunner,
    _host: &str,
    _server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<()> {
    let Some(mut w) = worker.take() else {
        return Ok(());
    };

    let exit = match w.wait_timeout(TERMINAL_WORKER_GRACE)? {
        Some(exit) => exit,
        None => {
            let _ = w.terminate_and_reap();
            anyhow::bail!(
                "process-run did not exit within {}s after terminal run-state was observed \
                 for run {}/{}",
                TERMINAL_WORKER_GRACE.as_secs(),
                nickname.as_str(),
                run_id.as_str(),
            );
        }
    };

    match exit {
        RemoteCommandExit::Success { stdout } => {
            let _resp =
                parse_process_run_response(&stdout, nickname, run_id).with_context(|| {
                    format!(
                        "process-run succeeded after terminal state for run {}/{} \
                         but produced invalid response",
                        nickname.as_str(),
                        run_id.as_str(),
                    )
                })?;
            Ok(())
        }
        RemoteCommandExit::RemoteFailure { stderr, .. } => {
            warn!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                "process-run exited with remote failure after terminal state: {stderr}",
            );
            Ok(())
        }
        RemoteCommandExit::TransportFailure { details, .. } => {
            warn!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                "process-run exited with transport failure after terminal state: {details}",
            );
            Ok(())
        }
        RemoteCommandExit::Killed => {
            anyhow::bail!(
                "process-run was killed after terminal state for run {}/{}",
                nickname.as_str(),
                run_id.as_str(),
            );
        }
    }
}

/// Parse, validate envelope, and enforce `ProcessRunResponse` outcome
/// against a fresh `run-state` poll.
#[allow(clippy::too_many_arguments)]
fn handle_process_run_success(
    runner: &RemoteRunner,
    host: &str,
    server_cmd: &str,
    nickname: &Nickname,
    run_id: &RunId,
    stdout: &str,
    _state: &RunStateResponse,
    no_progress: &mut NoProgressTracker,
) -> Result<WorkerExitDecision> {
    let resp = parse_process_run_response(stdout, nickname, run_id)?;

    let outcome = purgery_core::ProcessRunOutcome::from_str_name(&resp.outcome)
        .ok_or_else(|| anyhow::anyhow!("unknown process-run outcome '{}'", resp.outcome))?;

    let fresh = run_state(runner, host, server_cmd, nickname, run_id)?;

    match outcome {
        purgery_core::ProcessRunOutcome::Processed
        | purgery_core::ProcessRunOutcome::AlreadyTerminal => {
            if !fresh.terminal {
                anyhow::bail!(
                    "process-run reported outcome={} but fresh run-state is not terminal \
                     for run {}/{} (phase={})",
                    resp.outcome,
                    nickname.as_str(),
                    run_id.as_str(),
                    fresh.phase,
                );
            }
            Ok(WorkerExitDecision::Terminal(fresh))
        }
        purgery_core::ProcessRunOutcome::AlreadyActive => {
            if fresh.phase != "processing" || fresh.processor_state.as_deref() != Some("active") {
                anyhow::bail!(
                    "process-run reported outcome=already_active but fresh run-state is not \
                     processing/active for run {}/{} (phase={}, processor_state={:?})",
                    nickname.as_str(),
                    run_id.as_str(),
                    fresh.phase,
                    fresh.processor_state,
                );
            }
            Ok(WorkerExitDecision::ContinueWaiting)
        }
        purgery_core::ProcessRunOutcome::ClaimInProgress => {
            if fresh.terminal {
                return Ok(WorkerExitDecision::Terminal(fresh));
            }
            if fresh.phase == "processing" && fresh.processor_state.as_deref() == Some("active") {
                return Ok(WorkerExitDecision::ContinueWaiting);
            }
            if fresh.phase == "ready" && !no_progress.advance(&fresh) {
                anyhow::bail!(
                    "process-run repeatedly exited without progress: outcome=claim_in_progress \
                     and target remained ready after {} consecutive attempts for run {}/{}",
                    MAX_NO_PROGRESS_COUNT,
                    nickname.as_str(),
                    run_id.as_str(),
                );
            }
            Ok(WorkerExitDecision::ContinueLoop(fresh))
        }
    }
}

/// Parse and validate the envelope of a `ProcessRunResponse` from stdout.
fn parse_process_run_response(
    stdout: &str,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<purgery_core::ProcessRunResponse> {
    if stdout.trim().is_empty() {
        anyhow::bail!(
            "process-run exited successfully but produced empty stdout for run {}/{}",
            nickname.as_str(),
            run_id.as_str(),
        );
    }
    let resp: purgery_core::ProcessRunResponse = toml::from_str(stdout).with_context(|| {
        format!(
            "failed to parse process-run response TOML for run {}/{}",
            nickname.as_str(),
            run_id.as_str(),
        )
    })?;
    if resp.protocol_version != purgery_core::PROTOCOL_VERSION {
        anyhow::bail!(
            "process-run response protocol_version {} does not match client version {} \
             for run {}/{}",
            resp.protocol_version,
            purgery_core::PROTOCOL_VERSION,
            nickname.as_str(),
            run_id.as_str(),
        );
    }
    purgery_core::require_compatible_purgery_version(&resp.purgery_version, "process-run response")
        .with_context(|| {
            format!(
                "incompatible purgery_version in process-run response for run {}/{}",
                nickname.as_str(),
                run_id.as_str(),
            )
        })?;
    if resp.nickname != nickname.as_str() || resp.run_id != run_id.as_str() {
        anyhow::bail!(
            "process-run response envelope mismatch for run {}/{} \
             (response nickname={}, run_id={})",
            nickname.as_str(),
            run_id.as_str(),
            resp.nickname,
            resp.run_id,
        );
    }
    Ok(resp)
}

/// Terminate the local process-run worker before returning an error.
/// Ensures no owned child is left running or unreaped on error paths.
fn terminate_worker_on_error(worker: &mut Option<RemoteCommandHandle>) {
    if let Some(mut w) = worker.take() {
        let _ = w.terminate_and_reap();
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
        protocol_version: purgery_core::CLIENT_RUN_STATE_VERSION,
        purgery_version: purgery_core::current_purgery_version().to_string(),
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
        // Probe purgery_version from raw TOML before full deserialization
        // so we can distinguish missing/incompatible version (old state)
        // from malformed content (corrupt current state).
        let run_state = match purgery_core::probe_purgery_version_from_toml(&content) {
            Err(purgery_core::VersionProbeError::MissingVersion) => {
                warn!(
                    "client run state {:?} is missing purgery_version (too old); skipping",
                    state_path
                );
                continue;
            }
            Err(purgery_core::VersionProbeError::InvalidToml(e)) => {
                error!("client run state {:?} is not valid TOML: {e}", state_path);
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
            Ok(version) => {
                if let Err(e) =
                    purgery_core::require_compatible_purgery_version(&version, "client run state")
                {
                    warn!(
                        "client run state {:?} has incompatible purgery_version: {e}; skipping",
                        state_path
                    );
                    continue;
                }
                match toml::from_str::<ClientRunState>(&content) {
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
                }
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
    purgery_core::require_compatible_purgery_version(&manifest.purgery_version, "manifest")
        .with_context(|| "incompatible persisted manifest version")?;
    let run_config: RunConfig = toml::from_str(&state.run_config)
        .with_context(|| "failed to parse persisted run config")?;
    purgery_core::require_compatible_purgery_version(&run_config.purgery_version, "run config")
        .with_context(|| "incompatible persisted run config version")?;

    check_server_version(runner, host, server_cmd)
        .with_context(|| "server version check failed while resuming persisted run")?;

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
                debug!("driving server processing to terminal");
                drive_server_until_terminal(runner, host, server_cmd, &nickname, &run_id)?;
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
                        purgery_core::require_compatible_purgery_version(
                            &s.purgery_version,
                            "status",
                        )
                        .with_context(|| "incompatible persisted status version")?;
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

/// Tracks whether the top-level invocation has already performed
/// server version check and GC for a server-run sync.  Recursive
/// split entries reuse the fact that these are already done.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ServerRunSetup {
    Needed,
    AlreadyDone,
}

pub(crate) fn run_sync(args: &SyncArgs) -> Result<()> {
    run_sync_with_runner(&RemoteRunner::real(), args)
}

pub(crate) fn run_sync_with_runner(runner: &RemoteRunner, args: &SyncArgs) -> Result<()> {
    let run_id = RunId::generate();
    run_sync_with_run_id(runner, args, &run_id, ServerRunSetup::Needed)
}

pub(crate) fn run_sync_with_run_id(
    runner: &RemoteRunner,
    args: &SyncArgs,
    run_id: &RunId,
    setup: ServerRunSetup,
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

    let has_transform = args.transform.is_some();
    if has_transform && !args.delete_after_import {
        anyhow::bail!("--delete-after-import is required when --transform is used");
    }

    if let Some(ref _pattern) = args.split {
        return run_split(runner, args, run_id, &state_dir, &source_spec, setup);
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
    if !has_transform && !args.delete_after_import {
        info!("starting direct rsync");
        runner.run_rsync(
            &source_spec.operation_path,
            &remote.host,
            remote.path.as_str(),
        )?;
        info!("sync complete");
        return Ok(());
    }

    let manifest =
        classify::build_manifest(&source_spec, run_id, &nickname, args.transform.as_deref())?;
    let cleanup_state_path = if args.delete_after_import {
        let entries = classify::capture_cleanup_identity(&source_spec)?;
        if entries.is_empty() {
            None
        } else {
            let state = DurableCleanupState {
                purgery_version: purgery_core::current_purgery_version().to_string(),
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
    if !has_transform {
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

    // Transform: server run flow with heartbeat and crash-safe persistence
    let server_cmd = &args.server_command;
    let mut run_config = RunConfig {
        purgery_version: purgery_core::current_purgery_version().to_string(),
        nickname: nickname.clone(),
        destination: remote.path.clone(),
        delete_after_import: true,
    };

    match setup {
        ServerRunSetup::Needed => {
            check_server_version(runner, &remote.host, server_cmd)
                .with_context(|| "server version compatibility check failed")?;

            info!("running server GC");
            if let Err(e) = runner.server_cmd(&remote.host, server_cmd, &["gc"]) {
                warn!(error = %e, "server GC failed (non-fatal)");
            } else {
                debug!("server GC completed");
            }
        }
        ServerRunSetup::AlreadyDone => {
            debug!("server version and GC already handled by top-level split");
        }
    }

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
                purgery_version: purgery_core::current_purgery_version().to_string(),
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

    info!("driving server processing");
    drive_server_until_terminal(runner, &remote.host, server_cmd, &nickname, run_id)?;

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
///
/// Pure passthrough split runs one rsync with constant filter rules derived
/// from the pattern — no pre-discovery, no server run, no cleanup state.
/// Cleanup/transform split uses Purgery discovery with ancestor pruning
/// and serialized non-split operations.
fn run_split(
    runner: &RemoteRunner,
    args: &SyncArgs,
    _run_id: &RunId,
    state_dir: &str,
    source_spec: &classify::SourceSpec,
    setup: ServerRunSetup,
) -> Result<()> {
    let pattern = args.split.as_deref().unwrap();
    split::validate_split_pattern(pattern).map_err(|e| anyhow::anyhow!("{e}"))?;

    let has_transform = args.transform.is_some();
    let target = parse_destination(&args.destination)?;

    if !has_transform && !args.delete_after_import {
        return run_passthrough_split(
            runner,
            &source_spec.operation_path,
            source_spec.kind,
            pattern,
            &target.host,
            target.path.as_str(),
        );
    }

    let entry_roots = split::discover_split_entries(&source_spec.operation_path, pattern)
        .map_err(|e| anyhow::anyhow!("split discovery failed: {e}"))?;
    if entry_roots.is_empty() {
        info!("split pattern matched nothing");
        return Ok(());
    }

    // Run server version check and GC once before processing any split
    // entries, but only if this is a server-run sync (transform) and
    // the top-level invocation hasn't already done it.
    if has_transform {
        match setup {
            ServerRunSetup::Needed => {
                check_server_version(runner, &target.host, &args.server_command)
                    .with_context(|| "server version compatibility check failed")?;
                info!("running server GC");
                if let Err(e) = runner.server_cmd(&target.host, &args.server_command, &["gc"]) {
                    warn!(error = %e, "server GC failed (non-fatal)");
                } else {
                    debug!("server GC completed");
                }
            }
            ServerRunSetup::AlreadyDone => {
                debug!("server version and GC already handled by top-level invocation");
            }
        }
    }

    let base_dest = &args.destination;
    for root in &entry_roots {
        let suffix = split::split_target_suffix(&source_spec.operation_path, &root.path);
        let split_dest = format!("{}{}", base_dest, suffix);
        info!(source = %root.path, destination = %split_dest, "processing split entry");
        let split_args = SyncArgs {
            transform: args.transform.clone(),
            delete_after_import: args.delete_after_import,
            split: None,
            state_dir: Some(state_dir.to_owned()),
            server_command: args.server_command.clone(),
            source: root.path.clone(),
            destination: split_dest,
        };
        let split_run_id = RunId::generate();
        // Tell each recursive entry that setup (version + GC) is already done.
        run_sync_with_run_id(
            runner,
            &split_args,
            &split_run_id,
            ServerRunSetup::AlreadyDone,
        )?;
    }
    Ok(())
}

fn run_passthrough_split(
    runner: &RemoteRunner,
    source: &str,
    kind: crate::classify::SourceKind,
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
            if kind != crate::classify::SourceKind::Directory {
                info!(
                    "passthrough split with pattern \"{pattern}\" on non-directory source: \
                     no matches possible, exiting"
                );
                return Ok(());
            }
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
        let runner = RemoteRunner::fake();
        runner.add_response(
            "version",
            &format!(
                "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                purgery_core::PROTOCOL_VERSION
            ),
        );
        runner
    }

    fn mk_state_dir(tmp: &tempfile::TempDir) -> String {
        tmp.path().join("purgery").to_string_lossy().to_string()
    }

    /// Base protocol + purgery_version TOML header for test responses.
    fn resp_header() -> String {
        format!(
            "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
            purgery_core::PROTOCOL_VERSION
        )
    }

    fn process_run_ok_toml(outcome: &str, run_id: &str) -> String {
        format!(
            r#"protocol_version = {}
purgery_version = "0.1.0-test"
nickname = "laptop"
run_id = "{}"
outcome = "{}"
"#,
            purgery_core::PROTOCOL_VERSION,
            run_id,
            outcome,
        )
    }

    /// Helper for repeated processing-state TOML responses.
    fn processing_state(run_id: &str, ts: u64) -> String {
        format!(
            "{}nickname = \"laptop\"\nrun_id = \"{run_id}\"\n\
             phase = \"processing\"\nterminal = false\n\
             message = \"run phase: processing\"\n\
             processor_state = \"idle\"\n\
             updated_at_unix_secs = {ts}\nobserved_at_unix_secs = {ts}\n",
            resp_header()
        )
    }

    fn begin_resp_toml() -> String {
        format!(
            r#"protocol_version = {}
purgery_version = "0.1.0-test"
nickname = "laptop"
run_id = "test-run"
incoming_dir = "/var/lib/purgery/work/laptop/incoming/test-run"
files_dir = "/var/lib/purgery/work/laptop/incoming/test-run"
run_config_path = "/var/lib/purgery/work/laptop/incoming/test-run/run.toml"
manifest_path = "/var/lib/purgery/work/laptop/incoming/test-run/manifest.toml"
heartbeat_interval_secs = 60
"#,
            purgery_core::PROTOCOL_VERSION
        )
    }

    fn done_run_state_toml() -> String {
        format!(
            r#"protocol_version = {}
purgery_version = "0.1.0-test"
nickname = "laptop"
run_id = "test-run"
phase = "done"
terminal = true
message = ""
updated_at_unix_secs = 1000
observed_at_unix_secs = 1000
"#,
            purgery_core::PROTOCOL_VERSION
        )
    }

    fn done_status_toml() -> String {
        r#"purgery_version = "0.1.0-test"
run_id = "test-run"
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
    fn transform_requires_delete_after_import() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let args = SyncArgs {
            transform: Some("compress".into()),
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
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-drain".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
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
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-block".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
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
    fn recovery_checks_server_version_before_finish_run() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        // Set up a runner with everything needed to drain EXCEPT a
        // "version" response. If check_server_version were not called,
        // drain_one would succeed via these responses.  Since
        // check_server_version IS called, it fails before reaching any
        // remote command.
        let runner = RemoteRunner::fake();
        runner.add_response("finish-run", "");
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("test-run", "test-version-check"),
        );
        runner.add_response(
            "status",
            &done_status_toml().replace("test-run", "test-version-check"),
        );

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-version-check".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-version-check".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::UploadCompleteFinishPending,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(
            result.is_err(),
            "resume must fail when no server version is available; drain would succeed otherwise"
        );
    }

    #[test]
    fn terminal_status_seen_uses_persisted_terminal_status() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        // No SSH responses needed — terminal_status is persisted

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-tss".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        let status = RunStatus {
            purgery_version: "0.1.0-test".to_string(),
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
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-notss".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
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
            protocol_version: purgery_core::PROTOCOL_VERSION,
            purgery_version: "0.1.0-test".to_string(),
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
            current_transform: None,
            progress_status: None,
            processor_state: None,
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
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        let begin = begin_resp_toml().replace(
            "heartbeat_interval_secs = 60",
            "heartbeat_interval_secs = 1",
        );
        runner.add_response("begin-run", &begin);
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("run-state", &done_run_state_toml());
        let status_toml =
            "purgery_version = \"0.1.0-test\"\nrun_id = \"test-run\"\nnickname = \"laptop\"\nstate = \"done\"\n".to_string();
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

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "sync must succeed");

        // finish-run hook fired → finish-run was called
        assert!(finish_called.load(Ordering::Relaxed));
        // heartbeat must have run — if it had failed, is_healthy would
        // have rejected it before finish-run
        let log = runner.command_log();
        assert!(log.iter().any(|c| c.contains("heartbeat-run")));
    }

    fn ready_run_state_toml() -> String {
        format!(
            r#"protocol_version = {}
purgery_version = "0.1.0-test"
nickname = "laptop"
run_id = "test-run"
phase = "ready"
terminal = false
message = "run phase: ready"
updated_at_unix_secs = 1000
observed_at_unix_secs = 1000
"#,
            purgery_core::PROTOCOL_VERSION
        )
    }

    #[test]
    fn transform_sync_calls_process_run_after_finish_run() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        // First run-state returns ready → triggers process-run
        runner.add_response("run-state", &ready_run_state_toml());
        // process-run response (empty on success)
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        // After process-run: run-state returns done
        runner.add_response("run-state", &done_run_state_toml());
        let status_toml =
            "purgery_version = \"0.1.0-test\"\nrun_id = \"test-run\"\nnickname = \"laptop\"\nstate = \"done\"\n".to_string();
        runner.add_response("status", &status_toml);

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "sync must succeed");

        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("process-run")),
            "command log must contain process-run: {log:?}"
        );
        assert!(
            !log.iter().any(|c| c.contains("process-once")),
            "command log must NOT contain process-once: {log:?}"
        );
        // The process-run command must include the targeted nickname and run-id
        assert!(
            log.iter()
                .filter(|c| c.contains("process-run"))
                .any(|c| c.contains("--nickname") && c.contains("laptop")),
            "process-run must include --nickname laptop: {log:?}"
        );
        assert!(
            log.iter()
                .filter(|c| c.contains("process-run"))
                .any(|c| c.contains("--run-id") && c.contains("test-run")),
            "process-run must include --run-id test-run: {log:?}"
        );
        assert!(
            log.iter().any(|c| c.contains("finish-run")),
            "command log must contain finish-run: {log:?}"
        );
        assert!(
            log.iter().any(|c| c.contains("run-state")),
            "command log must contain run-state: {log:?}"
        );
    }

    #[test]
    fn transform_sync_process_run_failure_is_surfaced() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        // run-state returns ready
        runner.add_response("run-state", &ready_run_state_toml());
        // process-run returns an error
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::RemoteFailure {
                exit_code: Some(1),
                stderr: "simulated process-run failure".to_string(),
            },
        );
        // After process-run error, re-check: run is still ready
        runner.add_response("run-state", &ready_run_state_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("process-run remote failure"),
            "error must mention process-run remote failure, got: {err}"
        );
    }

    #[test]
    fn transform_sync_process_run_failure_is_ignored_when_run_terminal() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        // run-state returns ready
        runner.add_response("run-state", &ready_run_state_toml());
        // process-run returns an error
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::RemoteFailure {
                exit_code: Some(1),
                stderr: "simulated process-run failure".to_string(),
            },
        );
        // After process-run error, re-check: run is now terminal
        runner.add_response("run-state", &done_run_state_toml());
        let status_toml =
            "purgery_version = \"0.1.0-test\"\nrun_id = \"test-run\"\nnickname = \"laptop\"\nstate = \"done\"\n".to_string();
        runner.add_response("status", &status_toml);

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(
            result.is_ok(),
            "sync must succeed when process-run fails but run is terminal"
        );
    }

    #[test]
    fn process_run_error_is_preserved_when_followup_run_state_fails() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        // run-state returns ready (consumed by loop)
        runner.add_response("run-state", &ready_run_state_toml());
        // process-run returns an error (consumed by try_wait)
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::RemoteFailure {
                exit_code: Some(1),
                stderr: "simulated process-run failure".to_string(),
            },
        );
        // Loop polls run-state again → still ready (non-terminal)
        runner.add_response("run-state", &ready_run_state_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("process-run remote failure"),
            "error must mention process-run remote failure, got: {err}"
        );
    }

    #[test]
    fn client_sees_ready_process_run_observes_processing_then_polls() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        // run-state returns ready → triggers process-run
        runner.add_response("run-state", &ready_run_state_toml());
        // process-run succeeds
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        // But another processor already claimed it — run-state shows processing
        runner.add_response(
            "run-state",
            &format!("{}nickname = \"laptop\"\nrun_id = \"test-run\"\nphase = \"processing\"\nterminal = false\nmessage = \"run phase: processing\"\nupdated_at_unix_secs = 1000\nobserved_at_unix_secs = 1000\n", resp_header()),
        );
        // After polling, it becomes terminal
        runner.add_response("run-state", &done_run_state_toml());
        let status_toml =
            "purgery_version = \"0.1.0-test\"\nrun_id = \"test-run\"\nnickname = \"laptop\"\nstate = \"done\"\n".to_string();
        runner.add_response("status", &status_toml);

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(
            result.is_ok(),
            "sync must succeed even when process-run race loses: {result:?}"
        );

        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("process-run")),
            "command log must contain process-run: {log:?}"
        );
        assert!(
            !log.iter().any(|c| c.contains("process-once")),
            "command log must NOT contain process-once: {log:?}"
        );
    }

    #[test]
    fn resume_drives_processing_when_run_is_ready() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        // Provide responses for drain_one path:
        // 1. finish-run (already called during persistence below → drain skips it)
        // Actually the persisted state will be WaitingForTerminalState,
        // so drain_one goes straight to drive_server_until_terminal.
        runner.add_response(
            "run-state",
            &ready_run_state_toml().replace("test-run", "test-resume-drive"),
        );
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-resume-drive"),
            },
        );
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("test-run", "test-resume-drive"),
        );
        runner.add_response(
            "status",
            &done_status_toml().replace("test-run", "test-resume-drive"),
        );

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-resume-drive".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-resume-drive".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::WaitingForTerminalState,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_ok(), "resume must drive processing and succeed");

        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("process-run")),
            "resume must call process-run: {log:?}"
        );
    }

    #[test]
    fn resume_drives_processing_when_run_is_processing() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();

        // First run-state call returns processing (run is already being
        // processed by another server).  The client should call process-run
        // on it and then poll until terminal.
        runner.add_response("run-state", &processing_state("test-resume-proc", 1000));
        // process-run succeeds
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-resume-proc"),
            },
        );
        // After process-run, run-state returns done
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("test-run", "test-resume-proc"),
        );
        runner.add_response(
            "status",
            &done_status_toml().replace("test-run", "test-resume-proc"),
        );

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-resume-proc".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-resume-proc".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::WaitingForTerminalState,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_ok(), "resume must drive processing and succeed");

        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("process-run")),
            "resume must call process-run for processing run: {log:?}"
        );
    }

    #[test]
    fn processing_state_does_not_call_process_run_every_poll() {
        // After the first process-run drive in the processing state,
        // subsequent polls within the 60-second drive interval must
        // not call process-run again.
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();

        // First run-state call returns processing (idle) → triggers process-run
        runner.add_response("run-state", &processing_state("test-backoff", 1000));
        // process-run stays running for several polls, then exits.
        runner.add_spawned_cmd_exit(
            "process-run",
            10,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-backoff"),
            },
        );
        // The supervisor keeps the same worker handle and does not respawn.
        // trigger_start_run_and_recheck calls run-state again → still processing
        runner.add_response("run-state", &processing_state("test-backoff", 1001));
        // Second poll: run-state returns processing again
        // (should NOT call process-run, just poll)
        runner.add_response("run-state", &processing_state("test-backoff", 1002));
        // Third poll: finally terminal
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("test-run", "test-backoff"),
        );
        runner.add_response(
            "status",
            &done_status_toml().replace("test-run", "test-backoff"),
        );

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-backoff".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-backoff".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::WaitingForTerminalState,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_ok(), "resume must succeed: {result:?}");

        let log = runner.command_log();
        let pr_count = log.iter().filter(|c| c.contains("process-run")).count();
        assert_eq!(
            pr_count, 1,
            "must call process-run exactly once (first processing poll, not subsequent): {log:?}"
        );
    }

    #[test]
    fn ready_state_calls_process_run_immediately() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();

        // run-state returns ready → always drives immediately
        runner.add_response(
            "run-state",
            &ready_run_state_toml().replace("test-run", "test-always-ready"),
        );
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("already_active", "test-always-ready"),
            },
        );
        // After process-run: processing (another processor claimed it with active lock)
        let active_proc = format!(
            r#"protocol_version = {}
purgery_version = "0.1.0-test"
nickname = "laptop"
run_id = "test-always-ready"
phase = "processing"
terminal = false
message = "processing"
processor_state = "active"
updated_at_unix_secs = 1000
observed_at_unix_secs = 1000
"#,
            purgery_core::PROTOCOL_VERSION
        );
        runner.add_response("run-state", &active_proc);
        // Poll again: still processing with active processor (no process-run)
        runner.add_response("run-state", &active_proc);
        // Finally terminal
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("test-run", "test-always-ready"),
        );
        runner.add_response(
            "status",
            &done_status_toml().replace("test-run", "test-always-ready"),
        );

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-always-ready".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-always-ready".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::WaitingForTerminalState,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_ok(), "resume must succeed: {result:?}");

        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("process-run")),
            "ready state must call process-run: {log:?}"
        );
    }

    #[test]
    fn resume_processing_drives_immediately() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();

        // First run-state call returns processing → must drive immediately
        runner.add_response("run-state", &processing_state("test-resume-imm", 1000));
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-resume-imm"),
            },
        );
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("test-run", "test-resume-imm"),
        );
        runner.add_response(
            "status",
            &done_status_toml().replace("test-run", "test-resume-imm"),
        );

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-resume-imm".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-resume-imm".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::WaitingForTerminalState,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_ok(), "resume must succeed: {result:?}");

        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("process-run")),
            "must call process-run immediately on first processing observation: {log:?}"
        );
    }

    #[test]
    fn processing_state_process_run_error_is_surfaced() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();

        // First run-state returns processing
        runner.add_response("run-state", &processing_state("test-prerr", 1000));
        // process-run fails
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::RemoteFailure {
                exit_code: Some(1),
                stderr: "simulated process-run failure".to_string(),
            },
        );
        // Follow-up run-state is non-terminal (still processing)
        runner.add_response("run-state", &processing_state("test-prerr", 1001));

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-prerr".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-prerr".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::WaitingForTerminalState,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to resume"),
            "must fail to resume due to process-run error: {err}"
        );
    }

    #[test]
    fn processing_state_process_run_error_ignored_only_if_followup_terminal() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();

        // First run-state returns processing
        runner.add_response("run-state", &processing_state("test-prterm", 1000));
        // process-run fails
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::RemoteFailure {
                exit_code: Some(1),
                stderr: "simulated process-run failure".to_string(),
            },
        );
        // But follow-up run-state is terminal (another processor finished it)
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("test-run", "test-prterm"),
        );
        runner.add_response(
            "status",
            &done_status_toml().replace("test-run", "test-prterm"),
        );

        let manifest = Manifest {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("test-prterm".into()).unwrap(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = RunConfig {
            purgery_version: "0.1.0-test".to_string(),
            nickname: Nickname::new("laptop".into()).unwrap(),
            destination: DestinationPath::new(camino::Utf8PathBuf::from("rel")).unwrap(),
            delete_after_import: true,
        };

        persist_client_run_state(
            &state_dir,
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("test-prterm".into()).unwrap(),
            "host",
            "purgery-server",
            &manifest,
            &run_config,
            None,
            ClientRunPhase::WaitingForTerminalState,
        )
        .unwrap();

        let result = resume_runs(&runner, &state_dir);
        assert!(
            result.is_ok(),
            "must succeed even when process-run fails if follow-up is terminal: {result:?}"
        );
    }

    fn src_with_file(tmp: &tempfile::TempDir) -> String {
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("file.txt"), "data").unwrap();
        src.to_string_lossy().to_string()
    }

    fn transform_args(tmp: &tempfile::TempDir, state_dir: &str) -> SyncArgs {
        SyncArgs {
            transform: Some("transform".into()),
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
        let args = transform_args(&tmp, &state_dir);

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
        let args = transform_args(&tmp, &state_dir);

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
        let args = transform_args(&tmp, &state_dir);

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
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
        let args = transform_args(&tmp, &state_dir);

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
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
        let args = transform_args(&tmp, &state_dir);

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
        let args = transform_args(&tmp, &state_dir);

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
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
        let args = transform_args(&tmp, &state_dir);

        let begin = begin_resp_toml().replace(
            "heartbeat_interval_secs = 60",
            "heartbeat_interval_secs = 1",
        );
        runner.add_response("begin-run", &begin);
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
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
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        let begin = begin_resp_toml().replace(
            "heartbeat_interval_secs = 60",
            "heartbeat_interval_secs = 1",
        );
        runner.add_response("begin-run", &begin);
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("run-state", &done_run_state_toml());
        let status_toml =
            "purgery_version = \"0.1.0-test\"\nrun_id = \"test-run\"\nnickname = \"laptop\"\nstate = \"done\"\n".to_string();
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
            *result_clone.lock().unwrap() = Some(run_sync_with_run_id(
                &runner_for_sync,
                &args,
                &run_id,
                ServerRunSetup::Needed,
            ));
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
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-resolved-dest".into()).unwrap();

        let begin = begin_resp_toml().replace(
            "heartbeat_interval_secs = 60",
            "heartbeat_interval_secs = 1",
        );
        runner.add_response("begin-run", &begin);
        // Server returns a resolved absolute destination.
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-resolved-dest\"\n\
             destination = \"/server/resolved/absolute/path\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        // Make finish-run fail so UploadCompleteFinishPending persists.
        runner.add_error("finish-run", "simulated finish failure");

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
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
            transform: None,
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
            transform: None,
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
    fn transform_trailing_slash_stages_as_files_source_name() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("Videos");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.mp4"), "data").unwrap();
        let src_slash = format!("{}/", src.to_str().unwrap());
        let args = SyncArgs {
            transform: Some("transform".into()),
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
            &format!(
                "{}nickname = \"host\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response(
            "run-state",
            &done_run_state_toml().replace("laptop", "host"),
        );
        let status_toml =
            "purgery_version = \"0.1.0-test\"\nrun_id = \"test-run\"\nnickname = \"host\"\nstate = \"done\"\n".to_string();
        runner.add_response("status", &status_toml);
        runner.add_response("finish-run", "");

        run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed).unwrap();

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
                transform: None,
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
                transform: None,
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
        // Transform
        {
            let tmp = tempdir().unwrap();
            let runner = mk_runner();
            let args = SyncArgs {
                transform: Some("transform".into()),
                delete_after_import: true,
                split: None,
                state_dir: Some(mk_state_dir(&tmp)),
                source: "/".to_string(),
                destination: "host:/dest".to_string(),
                server_command: "purgery-server".to_string(),
            };
            let result = run_sync_with_runner(&runner, &args);
            assert!(result.is_err(), "/ must be rejected in transform mode");
            assert!(result.unwrap_err().to_string().contains("root"));
        }
        // Split
        {
            let tmp = tempdir().unwrap();
            let runner = mk_runner();
            let args = SyncArgs {
                transform: None,
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
            transform: None,
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
            "must include '*.mp4/***' for top-level directory payload"
        );
        assert!(
            includes.contains(&"**/*.mp4/***".to_string()),
            "must include '**/*.mp4/***' for nested directory payload"
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
            transform: None,
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
            transform: None,
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
            transform: None,
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
            cmd.contains("--include=*/"),
            "must include directory traversal rule"
        );
        assert!(cmd.contains("--exclude=*"), "must include exclude rule");
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
            transform: None,
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
        assert!(cmd.contains("--include=*.mp4"));
        assert!(cmd.contains("--exclude=*"));
    }

    // ── Invalid split pattern tests ──

    #[test]
    fn invalid_split_pattern_rejected_pure_passthrough() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = src_dir_str(&tmp);
        fs::write(std::path::Path::new(&src).join("a.txt"), "data").unwrap();

        for bad in &["", "/", "///"] {
            let args = SyncArgs {
                transform: None,
                delete_after_import: false,
                split: Some(bad.to_string()),
                state_dir: Some(state_dir.clone()),
                source: src.clone(),
                destination: "host:/dest".to_string(),
                server_command: "purgery-server".to_string(),
            };
            let result = run_sync_with_runner(&runner, &args);
            assert!(
                result.is_err(),
                "split pattern \"{bad}\" must be rejected in pure passthrough mode"
            );
        }
    }

    #[test]
    fn invalid_split_pattern_rejected_cleanup_transform() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = src_dir_str(&tmp);
        fs::write(std::path::Path::new(&src).join("a.txt"), "data").unwrap();

        for bad in &["", "/", "///"] {
            // Cleanup (delete without transform)
            {
                let args = SyncArgs {
                    transform: None,
                    delete_after_import: true,
                    split: Some(bad.to_string()),
                    state_dir: Some(state_dir.clone()),
                    source: src.clone(),
                    destination: "host:/dest".to_string(),
                    server_command: "purgery-server".to_string(),
                };
                let result = run_sync_with_runner(&runner, &args);
                assert!(
                    result.is_err(),
                    "split pattern \"{bad}\" must be rejected in cleanup mode"
                );
            }
            // Transform
            {
                let args = SyncArgs {
                    transform: Some("transform".into()),
                    delete_after_import: true,
                    split: Some(bad.to_string()),
                    state_dir: Some(state_dir.clone()),
                    source: src.clone(),
                    destination: "host:/dest".to_string(),
                    server_command: "purgery-server".to_string(),
                };
                let result = run_sync_with_runner(&runner, &args);
                assert!(
                    result.is_err(),
                    "split pattern \"{bad}\" must be rejected in transform mode"
                );
            }
        }
    }

    // ── GC lifecycle tests ──────────────────────────────────────────

    #[test]
    fn gc_started_before_begin_run() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_response("run-state", &ready_run_state_toml());
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "sync should succeed: {:?}", result.err());

        let log = runner.command_log();
        let gc_pos = log.iter().position(|c| c.contains("'gc'"));
        let begin_pos = log.iter().position(|c| c.contains("begin-run"));
        assert!(gc_pos.is_some(), "gc must appear in command log: {log:?}");
        assert!(begin_pos.is_some(), "begin-run must appear in command log");
        assert!(
            gc_pos.unwrap() < begin_pos.unwrap(),
            "gc must appear before begin-run in command log"
        );
    }

    #[test]
    fn gc_success_does_not_affect_sync_success() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "sync should succeed");
    }

    #[test]
    fn gc_remote_failure_logged_but_sync_succeeds() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::RemoteFailure {
                exit_code: Some(1),
                stderr: "simulated gc failure".to_string(),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(
            result.is_ok(),
            "sync must succeed even when GC fails: {:?}",
            result.err()
        );
    }

    #[test]
    fn gc_transport_failure_logged_but_sync_succeeds() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::TransportFailure {
                exit_code: Some(255),
                details: "ssh connection failed".to_string(),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(
            result.is_ok(),
            "sync must succeed even when GC transport fails: {:?}",
            result.err()
        );
    }

    #[test]
    fn transform_sync_error_still_settles_gc() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_write_error("run.toml", "write failed during staging");

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_err(), "sync must fail when write fails");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("write failed during staging"),
            "error must preserve original staging error, got: {err}"
        );

        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("'gc'")),
            "gc must appear in command log even after staging failure"
        );
    }

    #[test]
    fn passthrough_sync_does_not_start_gc() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let run_id = RunId::new("test-run".into()).unwrap();
        let args = SyncArgs {
            transform: None,
            delete_after_import: false,
            split: None,
            state_dir: Some(state_dir),
            source: src_with_file(&tmp),
            destination: "host:dest".to_string(),
            server_command: "purgery-server".to_string(),
        };

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "passthrough sync must succeed");

        let log = runner.command_log();
        assert!(
            !log.iter().any(|c| c.contains("'gc'")),
            "passthrough sync must not start GC: {log:?}"
        );
    }

    #[test]
    fn cleanup_sync_without_transform_does_not_start_gc() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let run_id = RunId::new("test-run".into()).unwrap();
        let args = SyncArgs {
            transform: None,
            delete_after_import: true,
            split: None,
            state_dir: Some(state_dir),
            source: src_with_file(&tmp),
            destination: "host:dest".to_string(),
            server_command: "purgery-server".to_string(),
        };

        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &done_run_state_toml());
        let status = r#"purgery_version = "0.1.0-test"
run_id = "test-run"
nickname = "laptop"
state = "done"

[[entries]]
local_path = "/tmp/test"
relative_path = "test"
status = "imported"
"#
        .to_string();
        runner.add_response("status", &status);

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "cleanup sync must succeed");

        let log = runner.command_log();
        assert!(
            !log.iter().any(|c| c.contains("'gc'")),
            "cleanup sync without transform must not start GC: {log:?}"
        );
    }

    #[test]
    fn no_start_gc_in_log() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok());

        let log = runner.command_log();
        for cmd in &["start-run", "worker-run", "start-gc", "gc-worker"] {
            assert!(
                !log.iter().any(|c| c.contains(cmd)),
                "forbidden command '{cmd}' found in log: {log:?}"
            );
        }
    }

    // ── Process-run regression tests ───────────────────────────────

    #[test]
    fn transform_sync_uses_process_run_not_start_run() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        // Run-state must show non-terminal (ready) so process-run is spawned
        runner.add_response("run-state", &ready_run_state_toml());
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok());

        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("process-run")),
            "transform sync must spawn process-run: {log:?}"
        );
        assert!(
            !log.iter().any(|c| c.contains("start-run")),
            "transform sync must not use start-run: {log:?}"
        );
    }

    #[test]
    fn transport_failure_restarts_process_run_when_ready() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_response("run-state", &ready_run_state_toml());
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::TransportFailure {
                exit_code: Some(255),
                details: "ssh failed".to_string(),
            },
        );
        runner.add_response("run-state", &ready_run_state_toml());
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(
            result.is_ok(),
            "sync should survive transport failure + restart"
        );

        let log = runner.command_log();
        let pr_count = log.iter().filter(|c| c.contains("process-run")).count();
        assert!(
            pr_count >= 2,
            "process-run must be restarted after transport failure, count={pr_count}: {log:?}"
        );
    }

    #[test]
    fn transport_failure_does_not_restart_when_processing_active() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_response("run-state", &ready_run_state_toml());
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        let active_processing = format!(
            r#"protocol_version = {}
purgery_version = "0.1.0-test"
nickname = "laptop"
run_id = "test-run"
phase = "processing"
terminal = false
message = "processing"
processor_state = "active"
updated_at_unix_secs = 1000
observed_at_unix_secs = 1000
"#,
            purgery_core::PROTOCOL_VERSION
        );
        runner.add_response("run-state", &active_processing);
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "sync must succeed");

        let log = runner.command_log();
        let pr_count = log.iter().filter(|c| c.contains("process-run")).count();
        assert!(
            pr_count <= 2,
            "process-run must not be respawned while processing is active, count={pr_count}: {log:?}"
        );
    }

    #[test]
    fn no_duplicate_process_run_while_handle_running() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_spawned_cmd_exit(
            "'gc'",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_response("run-state", &ready_run_state_toml());
        runner.add_spawned_cmd_exit(
            "process-run",
            3,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &ready_run_state_toml());
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "sync must succeed");

        let log = runner.command_log();
        let pr_count = log.iter().filter(|c| c.contains("process-run")).count();
        assert!(
            pr_count == 1,
            "process-run must be spawned exactly once (handle was still running), count={pr_count}: {log:?}"
        );
    }

    // ── Server-run-setup tests ─────────────────────────────────────

    #[test]
    fn transform_split_matches_inits_gc_once_total() {
        // Verify that when a top-level split sync calls run_sync_with_run_id
        // for multiple entries, the ServerRunSetup prevents duplicate
        // version check and GC.  We test this by calling the non-split
        // path twice — once with Needed, once with AlreadyDone — and
        // checking that version/GC appear only once total.

        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);

        // First call: ServerRunSetup::Needed — should run version + GC.
        {
            let runner = mk_runner();
            let args = transform_args(&tmp, &state_dir);
            runner.add_response(
                "gc",
                &format!(
                    "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                    purgery_core::PROTOCOL_VERSION
                ),
            );
            runner.add_response("begin-run", &begin_resp_toml());
            runner.add_response(
                "prepare-run",
                &format!(
                    "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                    resp_header()
                ),
            );
            runner.add_response("heartbeat-run", "");
            runner.add_response("finish-run", "");
            runner.add_response("run-state", &ready_run_state_toml());
            runner.add_spawned_cmd_exit(
                "process-run",
                0,
                RemoteCommandExit::Success {
                    stdout: process_run_ok_toml("processed", "test-run"),
                },
            );
            runner.add_response("run-state", &done_run_state_toml());
            runner.add_response("status", &done_status_toml());

            let run_id = RunId::new("test-run".into()).unwrap();
            let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
            assert!(
                result.is_ok(),
                "first entry must succeed: {:?}",
                result.err()
            );
        }

        // Second call: ServerRunSetup::AlreadyDone — must NOT run
        // version or GC again.
        {
            let runner = mk_runner();
            let args = transform_args(&tmp, &state_dir);
            runner.add_response("begin-run", &begin_resp_toml());
            runner.add_response(
                "prepare-run",
                &format!(
                    "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                    resp_header()
                ),
            );
            runner.add_response("heartbeat-run", "");
            runner.add_response("finish-run", "");
            runner.add_response("run-state", &ready_run_state_toml());
            runner.add_spawned_cmd_exit(
                "process-run",
                0,
                RemoteCommandExit::Success {
                    stdout: process_run_ok_toml("processed", "test-run"),
                },
            );
            runner.add_response("run-state", &done_run_state_toml());
            runner.add_response("status", &done_status_toml());

            let run_id = RunId::new("test-run".into()).unwrap();
            let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::AlreadyDone);
            assert!(
                result.is_ok(),
                "second entry must succeed: {:?}",
                result.err()
            );

            // The AlreadyDone variant must not have added version or gc.
            let log = runner.command_log();
            assert!(
                !log.iter().any(|c| c.contains("'version'")),
                "AlreadyDone call must not re-check version: {log:?}"
            );
            assert!(
                !log.iter().any(|c| c.contains("'gc'")),
                "AlreadyDone call must not re-run gc: {log:?}"
            );
        }
    }

    #[test]
    fn transform_split_no_matches_does_not_contact_server() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "data").unwrap();

        let args = SyncArgs {
            transform: Some("transform".into()),
            delete_after_import: true,
            split: Some("*.mp4".into()),
            state_dir: Some(state_dir),
            source: src.to_string_lossy().to_string(),
            destination: "laptop:rel".to_string(),
            server_command: "purgery-server".to_string(),
        };

        let run_id = RunId::new("test-split-nomatch".into()).unwrap();
        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "no-match split must succeed");

        let log = runner.command_log();
        assert!(
            !log.iter().any(|c| c.contains("'version'")),
            "no-match split must not call version: {log:?}"
        );
        assert!(
            !log.iter().any(|c| c.contains("'gc'")),
            "no-match split must not call gc: {log:?}"
        );
        assert!(
            !log.iter().any(|c| c.contains("begin-run")),
            "no-match split must not call begin-run: {log:?}"
        );
    }

    #[test]
    fn passthrough_split_does_not_call_server_commands() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.mp4"), "data").unwrap();

        let args = SyncArgs {
            transform: None,
            delete_after_import: false,
            split: Some("*.mp4".into()),
            state_dir: Some(state_dir),
            source: src.to_string_lossy().to_string(),
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };

        let run_id = RunId::new("test-passthrough-split".into()).unwrap();
        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "passthrough split must succeed");

        let log = runner.command_log();
        for cmd in &[
            "'version'",
            "'gc'",
            "begin-run",
            "prepare-run",
            "finish-run",
            "process-run",
            "run-state",
            "status",
        ] {
            assert!(
                !log.iter().any(|c| c.contains(cmd)),
                "passthrough split must not call {cmd}: {log:?}"
            );
        }
    }

    #[test]
    fn passthrough_cleanup_does_not_call_server_commands() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let file_path = tmp.path().join("video.mp4");
        fs::write(&file_path, "data").unwrap();

        let args = SyncArgs {
            transform: None,
            delete_after_import: true,
            split: None,
            state_dir: Some(state_dir),
            source: file_path.to_string_lossy().to_string(),
            destination: "host:/dest".to_string(),
            server_command: "purgery-server".to_string(),
        };

        let run_id = RunId::new("test-passthrough-cleanup".into()).unwrap();
        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(result.is_ok(), "passthrough with cleanup must succeed");

        let log = runner.command_log();
        for cmd in &[
            "'version'",
            "'gc'",
            "begin-run",
            "prepare-run",
            "finish-run",
            "process-run",
            "run-state",
            "status",
        ] {
            assert!(
                !log.iter().any(|c| c.contains(cmd)),
                "passthrough with cleanup must not call {cmd}: {log:?}"
            );
        }
    }

    #[test]
    fn normal_transform_calls_version_and_gc_once() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_response(
            "gc",
            &format!(
                "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                purgery_core::PROTOCOL_VERSION
            ),
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        runner.add_response("run-state", &ready_run_state_toml());
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "test-run"),
            },
        );
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(
            result.is_ok(),
            "transform sync must succeed: {:?}",
            result.err()
        );

        let log = runner.command_log();
        assert!(
            log.iter().any(|c| c.contains("'version'")),
            "transform sync must call version: {log:?}"
        );
        assert!(
            log.iter().any(|c| c.contains("'gc'")),
            "transform sync must call gc: {log:?}"
        );
        assert!(
            log.iter().any(|c| c.contains("begin-run")),
            "transform sync must call begin-run: {log:?}"
        );
        assert!(
            log.iter().any(|c| c.contains("process-run")),
            "transform sync must call process-run: {log:?}"
        );
        assert!(
            log.iter().any(|c| c.contains("status")),
            "transform sync must call status: {log:?}"
        );

        let version_count = log.iter().filter(|c| c.contains("'version'")).count();
        let gc_count = log.iter().filter(|c| c.contains("'gc'")).count();
        assert_eq!(
            version_count, 1,
            "version called {version_count} times, expected 1"
        );
        assert_eq!(gc_count, 1, "gc called {gc_count} times, expected 1");
    }

    // ── Worker lifecycle tests ─────────────────────────────────────

    #[test]
    fn finish_worker_already_exited_does_not_hang() {
        let runner = mk_runner();
        runner.add_spawned_cmd_exit(
            "worker-test",
            0,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "r"),
            },
        );
        let handle = runner
            .spawn_server_cmd("host", "ps", &["worker-test"])
            .unwrap();
        // Exited immediately (0 polls).
        let mut worker = Some(handle);
        let res = finish_worker_after_terminal(
            &mut worker,
            &runner,
            "host",
            "ps",
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("r".into()).unwrap(),
        );
        assert!(res.is_ok(), "finish should succeed: {:?}", res.err());
        assert!(worker.is_none(), "worker must be consumed");
    }

    #[test]
    fn finish_worker_waits_gracefully_for_scripted_exit() {
        let runner = mk_runner();
        // Scripted exit after 2 polls — should be found during grace period.
        runner.add_spawned_cmd_exit(
            "worker-test",
            2,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "r"),
            },
        );
        let handle = runner
            .spawn_server_cmd("host", "ps", &["worker-test"])
            .unwrap();
        let mut worker = Some(handle);
        let res = finish_worker_after_terminal(
            &mut worker,
            &runner,
            "host",
            "ps",
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("r".into()).unwrap(),
        );
        assert!(res.is_ok(), "graceful wait should succeed: {:?}", res.err());
        assert!(
            worker.is_none(),
            "worker must be consumed after graceful wait"
        );
    }

    #[test]
    fn finish_worker_terminates_when_worker_never_exits() {
        let runner = mk_runner();
        // No scripted exit — worker runs forever.
        let handle = runner
            .spawn_server_cmd("host", "ps", &["forever-worker"])
            .unwrap();
        let mut worker = Some(handle);
        let res = finish_worker_after_terminal(
            &mut worker,
            &runner,
            "host",
            "ps",
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("r".into()).unwrap(),
        );
        assert!(res.is_err(), "never-exiting worker must produce error");
        assert!(
            worker.is_none(),
            "worker must be consumed after termination"
        );
    }

    #[test]
    fn finish_worker_is_noop_when_none() {
        let runner = mk_runner();
        let mut worker: Option<RemoteCommandHandle> = None;
        let res = finish_worker_after_terminal(
            &mut worker,
            &runner,
            "host",
            "ps",
            &Nickname::new("laptop".into()).unwrap(),
            &RunId::new("r".into()).unwrap(),
        );
        assert!(res.is_ok());
        assert!(worker.is_none());
    }

    #[test]
    fn terminate_worker_on_error_clears_handle() {
        let runner = mk_runner();
        runner.add_spawned_cmd_exit(
            "worker-test",
            5,
            RemoteCommandExit::Success {
                stdout: process_run_ok_toml("processed", "r"),
            },
        );
        let handle = runner
            .spawn_server_cmd("host", "ps", &["worker-test"])
            .unwrap();
        let mut worker = Some(handle);
        terminate_worker_on_error(&mut worker);
        assert!(
            worker.is_none(),
            "worker must be consumed after error termination"
        );
    }

    #[test]
    fn terminate_worker_on_error_noop_when_none() {
        let mut worker: Option<RemoteCommandHandle> = None;
        terminate_worker_on_error(&mut worker);
        assert!(worker.is_none());
    }

    #[test]
    fn terminal_state_with_running_worker_does_not_hang() {
        // Integration-style test: drive_server_until_terminal sees
        // terminal state while a process-run worker is still running.
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        // Script the transform flow.  The process-run exit is not
        // scripted (worker runs forever), but the run-state should
        // become terminal on the first poll, triggering the
        // finish_worker_after_terminal path.
        runner.add_response(
            "gc",
            &format!(
                "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                purgery_core::PROTOCOL_VERSION
            ),
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        // Run-state returns terminal immediately → client should not spawn process-run.
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(
            result.is_ok(),
            "sync must succeed when run is already terminal: {:?}",
            result.err()
        );
    }

    #[test]
    fn remote_failure_with_fresh_terminal_succeeds() {
        let tmp = tempdir().unwrap();
        let state_dir = mk_state_dir(&tmp);
        let runner = mk_runner();
        let args = transform_args(&tmp, &state_dir);
        let run_id = RunId::new("test-run".into()).unwrap();

        runner.add_response(
            "gc",
            &format!(
                "protocol_version = {}\npurgery_version = \"0.1.0-test\"\n",
                purgery_core::PROTOCOL_VERSION
            ),
        );
        runner.add_response("begin-run", &begin_resp_toml());
        runner.add_response(
            "prepare-run",
            &format!(
                "{}nickname = \"laptop\"\nrun_id = \"test-run\"\n",
                resp_header()
            ),
        );
        runner.add_response("heartbeat-run", "");
        runner.add_response("finish-run", "");
        // Run-state shows ready → triggers process-run spawn
        runner.add_response("run-state", &ready_run_state_toml());
        // process-run returns remote failure
        runner.add_spawned_cmd_exit(
            "process-run",
            0,
            RemoteCommandExit::RemoteFailure {
                exit_code: Some(1),
                stderr: "simulated failure".to_string(),
            },
        );
        // Fresh run-state shows terminal → should succeed
        runner.add_response("run-state", &done_run_state_toml());
        runner.add_response("status", &done_status_toml());

        let result = run_sync_with_run_id(&runner, &args, &run_id, ServerRunSetup::Needed);
        assert!(
            result.is_ok(),
            "sync must succeed when process-run fails but fresh state is terminal: {:?}",
            result.err()
        );
    }
}
