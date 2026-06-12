use anyhow::{Context, Result};
use purgery_core::{
    BeginRunResponse, ClientRunPhase, ClientRunState, DurableCleanupState, Manifest, Nickname,
    PrepareRunResponse, RunConfig, RunId, RunStateResponse, RunStatus,
};
use std::fs;
use std::time::Duration;
use tracing::info;

use crate::classify;
use crate::cleanup;
use crate::ssh;
use crate::transfer;
use crate::SyncArgs;

fn parse_destination(destination: &str) -> Result<(&str, &str)> {
    let colon_pos = destination.rfind(':').ok_or_else(|| {
        anyhow::anyhow!("destination must be in format USER@HOST:PATH or HOST:PATH")
    })?;
    let host = &destination[..colon_pos];
    let path = &destination[colon_pos + 1..];
    if host.is_empty() || path.is_empty() {
        anyhow::bail!("destination host and path must not be empty");
    }
    Ok((host, path))
}

fn derive_nickname(destination: &str) -> Result<Nickname> {
    let (host_part, _) = parse_destination(destination)?;
    let sanitized: String = host_part
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

fn persist_client_run_state(
    state_dir: &str,
    nickname: &Nickname,
    run_id: &RunId,
    manifest: &Manifest,
    run_config: &RunConfig,
    phase: ClientRunPhase,
) -> Result<()> {
    let run_state = ClientRunState {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
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

pub(crate) fn run_sync(args: &SyncArgs) -> Result<()> {
    let has_postprocess = !args.postprocess.is_empty();

    if has_postprocess && !args.delete_after_import {
        anyhow::bail!("--delete-after-import is required when --postprocess is used");
    }

    let (host, dest_path) = parse_destination(&args.destination)?;
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
    let server_cmd = &args.server_command;

    info!(
        nickname = %nickname.as_str(),
        run_id = %run_id.as_str(),
        source = %args.source,
        destination = %args.destination,
        "starting sync"
    );

    let run_config = RunConfig {
        nickname: nickname.clone(),
        to: dest_path.to_owned(),
        delete_after_import: args.delete_after_import,
    };

    let manifest = classify::build_manifest(&args.source, &run_id, &nickname, &args.postprocess)?;

    let cleanup_state_path = if args.delete_after_import {
        let entries = cleanup::build_cleanup_entries(&args.source, &manifest)?;
        if !entries.is_empty() {
            let state = DurableCleanupState {
                nickname: nickname.as_str().to_owned(),
                operation_id: run_id.as_str().to_owned(),
                entries,
            };
            Some(cleanup::write_cleanup_state(&state, &state_dir)?)
        } else {
            None
        }
    } else {
        None
    };

    info!("starting server run");
    let begin_resp = begin_run(host, server_cmd, &nickname, &run_id)?;

    info!("transferring files");
    transfer::run_rsync(&args.source, host, &begin_resp.files_dir)?;

    if let Some(ref state_path) = cleanup_state_path {
        cleanup::mark_rsync_succeeded(state_path)?;
    }

    ssh::write_remote_file(host, &begin_resp.run_config_path, &run_config.to_toml()?)?;
    ssh::write_remote_file(host, &begin_resp.manifest_path, &manifest.to_toml()?)?;

    info!("preparing run");
    prepare_run(host, server_cmd, &nickname, &run_id)?;

    persist_client_run_state(
        &state_dir,
        &nickname,
        &run_id,
        &manifest,
        &run_config,
        ClientRunPhase::UploadCompleteFinishPending,
    )?;

    finish_run(host, server_cmd, &nickname, &run_id)?;

    persist_client_run_state(
        &state_dir,
        &nickname,
        &run_id,
        &manifest,
        &run_config,
        ClientRunPhase::WaitingForTerminalState,
    )?;

    if has_postprocess {
        info!("waiting for server processing");
        wait_for_terminal(host, server_cmd, &nickname, &run_id)?;
    }

    info!("reading run status");
    let status = read_status(host, server_cmd, &nickname, &run_id)?;

    if let Some(ref state_path) = cleanup_state_path {
        cleanup::process_cleanup_state_file(state_path)?;
    }

    persist_client_run_state(
        &state_dir,
        &nickname,
        &run_id,
        &manifest,
        &run_config,
        ClientRunPhase::CleanupComplete,
    )?;
    remove_client_run_state(&state_dir, &nickname, &run_id);

    info!(state = %status.state.as_str(), "sync complete");
    Ok(())
}
