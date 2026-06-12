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
fn start_heartbeat(
    host: String,
    server_cmd: String,
    nickname: Nickname,
    run_id: RunId,
    interval_secs: u64,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Result<()>> {
    let half_interval = Duration::from_secs(interval_secs.max(1) / 2);
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
            // Sleep in small increments so we can react to stop quickly
            let sleep_remaining = half_interval;
            let start = std::time::Instant::now();
            while start.elapsed() < sleep_remaining && !stop.load(Ordering::Relaxed) {
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
                warn!("failed to parse client run state {:?}: {e}", state_path);
                continue;
            }
        };
        let run_dir = entry.path();
        match resume_one(state_dir, &run_state) {
            Ok(true) => {
                let _ = fs::remove_dir_all(&run_dir);
            }
            Ok(false) => {
                info!(
                    nickname = %run_state.nickname,
                    run_id = %run_state.run_id,
                    phase = ?run_state.phase,
                    "resumed run state, waiting for next iteration"
                );
            }
            Err(e) => {
                warn!(
                    "failed to resume run {}/{}: {e}",
                    run_state.nickname, run_state.run_id
                );
            }
        }
    }
    Ok(())
}

/// Returns Ok(true) if the run was fully completed (cleanup done, state removed).
/// Returns Ok(false) if the run has been progressed but not finished.
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
                ClientRunPhase::WaitingForTerminalState,
            )?;
            Ok(false)
        }
        ClientRunPhase::WaitingForTerminalState => {
            debug!("waiting for terminal state");
            let terminal = wait_for_terminal(host, server_cmd, &nickname, &run_id)?;
            if terminal.phase == "not_found" {
                warn!("run not found on server, abandoning");
                return Ok(true);
            }
            let status = read_status(host, server_cmd, &nickname, &run_id)?;
            if status.nickname != nickname || status.run_id != run_id {
                anyhow::bail!("server status envelope does not match persisted run");
            }
            persist_client_run_state(
                state_dir,
                &nickname,
                &run_id,
                host,
                server_cmd,
                &manifest,
                &run_config,
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
            let status = match read_status(host, server_cmd, &nickname, &run_id) {
                Ok(s) => s,
                Err(e) => {
                    warn!("could not read status for resume: {e}");
                    return Ok(true);
                }
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
        ClientRunPhase::Abandoned | ClientRunPhase::Corrupt => {
            warn!("abandoned or corrupt run state, removing");
            Ok(true)
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
        ClientRunPhase::CleanupComplete,
    )?;
    remove_client_run_state(state_dir, nickname, run_id);
    Ok(())
}

// ── Main sync entry point ──────────────────────────────────────────────

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

pub(crate) fn run_sync(args: &SyncArgs) -> Result<()> {
    validate_source_is_directory(&args.source)?;

    let has_postprocess = !args.postprocess.is_empty();
    if has_postprocess && !args.delete_after_import {
        anyhow::bail!("--delete-after-import is required when --postprocess is used");
    }

    let remote = parse_destination(&args.destination)?;
    let nickname = derive_nickname(&args.destination)?;
    let run_id = RunId::generate();
    let state_dir = args.state_dir.clone().unwrap_or_else(|| {
        if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
            format!("{dir}/purgery")
        } else if let Ok(home) = std::env::var("HOME") {
            format!("{home}/.local/state/purgery")
        } else {
            "/tmp/purgery-client".to_string()
        }
    });

    cleanup::resume_pending_cleanups(&state_dir)?;
    resume_runs(&state_dir)?;

    info!(
        nickname = %nickname.as_str(),
        operation_id = %run_id.as_str(),
        source = %args.source,
        host = %remote.host,
        destination = %remote.path.as_str(),
        "starting sync"
    );

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

    let server_cmd = &args.server_command;
    let run_config = RunConfig {
        nickname: nickname.clone(),
        destination: remote.path.clone(),
        delete_after_import: true,
    };

    info!("starting server run");
    let begin_resp = begin_run(&remote.host, server_cmd, &nickname, &run_id)?;

    let stop_heartbeat = Arc::new(AtomicBool::new(false));
    let heartbeat_handle = start_heartbeat(
        remote.host.clone(),
        server_cmd.to_string(),
        nickname.clone(),
        run_id.clone(),
        begin_resp.heartbeat_interval_secs,
        Arc::clone(&stop_heartbeat),
    );

    let result = (|| -> Result<()> {
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

    stop_heartbeat.store(true, Ordering::Relaxed);
    let _ = heartbeat_handle.join();
    result?;

    info!("finishing server run");
    finish_run(&remote.host, server_cmd, &nickname, &run_id)?;

    persist_client_run_state(
        &state_dir,
        &nickname,
        &run_id,
        &remote.host,
        server_cmd,
        &manifest,
        &run_config,
        ClientRunPhase::WaitingForTerminalState,
    )?;

    info!("waiting for server processing");
    let terminal = wait_for_terminal(&remote.host, server_cmd, &nickname, &run_id)?;
    if terminal.phase == "not_found" {
        anyhow::bail!(
            "run {} not found on server after finish-run",
            run_id.as_str()
        );
    }

    info!("reading run status");
    let status = read_status(&remote.host, server_cmd, &nickname, &run_id)?;
    if status.nickname != nickname || status.run_id != run_id {
        anyhow::bail!("server status envelope does not match requested run");
    }

    persist_client_run_state(
        &state_dir,
        &nickname,
        &run_id,
        &remote.host,
        server_cmd,
        &manifest,
        &run_config,
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

    fn run_sync_validate(args: &SyncArgs) -> Result<()> {
        let has_postprocess = !args.postprocess.is_empty();
        if has_postprocess && !args.delete_after_import {
            anyhow::bail!("--delete-after-import is required when --postprocess is used");
        }
        Ok(())
    }
}
