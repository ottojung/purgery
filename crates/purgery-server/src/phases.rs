use anyhow::{Context, Result};
use camino::Utf8Path;
use purgery_core::{
    Nickname, ProcessingProgress, PurgeryRoot, RunId, RunPhase, RunState, RunStatus, ServerConfig,
};
use std::fs;
use tracing::{info, warn};

pub(crate) fn publish_status_atomic(directory: &Utf8Path, status: &RunStatus) -> Result<()> {
    let content = status.to_toml().context("failed to serialize status")?;
    let temporary = directory.join("status.toml.tmp");
    let final_path = directory.join("status.toml");
    fs::write(&temporary, content)
        .with_context(|| format!("failed to write temporary status: {}", temporary))?;
    fs::rename(&temporary, &final_path)
        .with_context(|| format!("failed to publish status: {}", final_path))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_progress(
    processing_path: &Utf8Path,
    nickname: &Nickname,
    run_id: &RunId,
    state: &str,
    entry_index: usize,
    entry_total: usize,
    current_entry: &str,
    current_step: &str,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let progress = ProcessingProgress {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        phase: "processing".to_string(),
        state: state.to_owned(),
        entry_index,
        entry_total,
        current_entry: current_entry.to_owned(),
        current_step: current_step.to_owned(),
        started_at_unix_secs: now,
        updated_at_unix_secs: now,
    };
    let content = toml::to_string(&progress)
        .map_err(|e| anyhow::anyhow!("failed to serialize progress: {e}"))?;
    let tmp = processing_path.join("progress.toml.tmp");
    let final_path = processing_path.join("progress.toml");
    fs::write(&tmp, &content)
        .with_context(|| format!("failed to write progress: {}", tmp.as_str()))?;
    fs::rename(&tmp, &final_path)
        .with_context(|| format!("failed to publish progress: {}", final_path.as_str()))?;
    Ok(())
}

pub(crate) fn write_run_failure(
    purgery_root: &PurgeryRoot,
    nickname: &Nickname,
    run_id: &RunId,
    error_msg: &str,
) -> Result<()> {
    let processing_path = purgery_root.run_dir(nickname, run_id, RunPhase::Processing);
    let status = RunStatus {
        run_id: run_id.clone(),
        nickname: nickname.clone(),
        state: RunState::Failed,
        entries: vec![],
        error: Some(error_msg.to_owned()),
    };
    let status_toml = status
        .to_toml()
        .with_context(|| "failed to serialize run failure status")?;
    let status_path = processing_path.join("status.toml");
    let tmp_path = processing_path.join("status.toml.tmp");

    fs::write(&tmp_path, &status_toml).with_context(|| {
        format!(
            "failed to write temporary run failure status: {}",
            tmp_path.as_str()
        )
    })?;
    fs::rename(&tmp_path, &status_path).with_context(|| {
        format!(
            "failed to finalize run failure status: {}",
            status_path.as_str()
        )
    })?;

    let failed_path = purgery_root.run_dir(nickname, run_id, RunPhase::Failed);
    if let Some(parent) = failed_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create failed parent: {}", parent.as_str()))?;
    }
    fs::rename(&processing_path, &failed_path).with_context(|| {
        format!(
            "failed to move run-level failure to failed: {} -> {}",
            processing_path.as_str(),
            failed_path.as_str()
        )
    })?;

    Ok(())
}

pub(crate) fn find_runs_in_phase(
    purgery_root: &PurgeryRoot,
    phase: RunPhase,
) -> Result<Vec<(Nickname, RunId)>> {
    let mut runs = Vec::new();
    let purgery_path = purgery_root.as_path();

    if !purgery_path.exists() {
        return Ok(runs);
    }

    for entry in fs::read_dir(purgery_path)
        .with_context(|| format!("failed to read purgery root: {}", purgery_path.as_str()))?
    {
        let entry = entry?;
        let nickname_path = entry.path();
        if !nickname_path.is_dir() {
            continue;
        }
        let nickname_str = nickname_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let Ok(nickname) = Nickname::new(nickname_str.to_owned()) else {
            continue;
        };

        let phase_path = nickname_path.join(phase.as_str());
        if !phase_path.exists() {
            continue;
        }

        for run_entry in fs::read_dir(&phase_path).with_context(|| {
            format!(
                "failed to read {} dir: {}",
                phase.as_str(),
                phase_path.display()
            )
        })? {
            let run_entry = run_entry?;
            let run_path = run_entry.path();
            if !run_path.is_dir() {
                continue;
            }
            let run_id_str = run_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let Ok(run_id) = RunId::new(run_id_str.to_owned()) else {
                continue;
            };
            runs.push((nickname.clone(), run_id));
        }
    }

    Ok(runs)
}

pub fn find_ready_runs(purgery_root: &PurgeryRoot) -> Result<Vec<(Nickname, RunId)>> {
    find_runs_in_phase(purgery_root, RunPhase::Ready)
}

pub fn find_processing_runs(purgery_root: &PurgeryRoot) -> Result<Vec<(Nickname, RunId)>> {
    find_runs_in_phase(purgery_root, RunPhase::Processing)
}

pub(crate) fn finalize_processing_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    state: &RunState,
) -> Result<()> {
    let processing_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Processing);
    let dest_phase = match state {
        RunState::Done | RunState::Partial => RunPhase::Done,
        RunState::Failed => RunPhase::Failed,
    };
    let dest_path = config.purgery_root.run_dir(nickname, run_id, dest_phase);
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create {} parent: {}",
                dest_phase.as_str(),
                parent.as_str()
            )
        })?;
    }
    fs::rename(&processing_path, &dest_path).with_context(|| {
        format!(
            "failed to move run to {}: {} -> {}",
            dest_phase.as_str(),
            processing_path.as_str(),
            dest_path.as_str()
        )
    })?;
    Ok(())
}

pub fn move_to_failed(
    purgery_root: &PurgeryRoot,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<()> {
    let processing_path = purgery_root.run_dir(nickname, run_id, RunPhase::Processing);
    let failed_path = purgery_root.run_dir(nickname, run_id, RunPhase::Failed);

    if processing_path.exists() {
        if let Some(parent) = failed_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create failed parent: {}", parent.as_str()))?;
        }
        fs::rename(&processing_path, &failed_path).with_context(|| {
            format!(
                "failed to move run to failed: {} -> {}",
                processing_path.as_str(),
                failed_path.as_str()
            )
        })?;
    }

    Ok(())
}

pub fn begin_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<String> {
    // Run GC opportunistically before creating the run
    if let Err(e) = crate::gc::run_gc(config) {
        warn!(error = %e, "opportunistic GC failed");
    }

    let phases = [
        RunPhase::Incoming,
        RunPhase::Ready,
        RunPhase::Processing,
        RunPhase::Done,
        RunPhase::Failed,
    ];
    for phase in &phases {
        let phase_path = config.purgery_root.run_dir(nickname, run_id, *phase);
        if phase_path.exists() {
            anyhow::bail!(
                "run {}/{} already exists in '{}' phase at '{}'",
                nickname.as_str(),
                run_id.as_str(),
                phase.as_str(),
                phase_path.as_str()
            );
        }
    }

    let incoming_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Incoming);
    let files_dir = incoming_path.join("files");
    let run_config_path = incoming_path.join("run.toml");
    let manifest_path = incoming_path.join("manifest.toml");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Some(parent) = incoming_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create incoming parent: {}", parent.as_str()))?;
    }
    fs::create_dir(incoming_path.as_std_path()).with_context(|| {
        format!(
            "failed to create incoming dir '{}' (race: run may have been created concurrently)",
            incoming_path.as_str()
        )
    })?;
    if let Err(error) = fs::create_dir(files_dir.as_std_path()) {
        let _ = fs::remove_dir_all(&incoming_path);
        return Err(error).with_context(|| format!("failed to create files dir: {}", files_dir));
    }

    let lease = purgery_core::LeaseFile {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        created_at_unix_secs: now,
        last_heartbeat_unix_secs: now,
        expires_at_unix_secs: now + config.gc.incoming_lease_secs,
    };
    let lease_content =
        toml::to_string(&lease).map_err(|e| anyhow::anyhow!("failed to serialize lease: {e}"))?;
    let lease_tmp = incoming_path.join("lease.toml.tmp");
    let lease_write_result = (|| -> Result<()> {
        fs::write(lease_tmp.as_std_path(), &lease_content)?;
        fs::rename(
            lease_tmp.as_std_path(),
            incoming_path.join("lease.toml").as_std_path(),
        )?;
        Ok(())
    })();

    if let Err(e) = lease_write_result {
        let _ = fs::remove_dir_all(&incoming_path);
        return Err(e.context("failed to write lease file"));
    }

    let response = purgery_core::BeginRunResponse {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        incoming_dir: incoming_path.as_str().to_owned(),
        files_dir: files_dir.as_str().to_owned(),
        run_config_path: run_config_path.as_str().to_owned(),
        manifest_path: manifest_path.as_str().to_owned(),
        heartbeat_interval_secs: config.gc.heartbeat_interval_secs,
    };

    let response_str = toml::to_string(&response).map_err(|e| {
        let _ = fs::remove_dir_all(&incoming_path);
        anyhow::anyhow!("failed to serialize begin-run response: {e}")
    })?;
    Ok(response_str)
}

pub fn finish_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    let incoming_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Incoming);

    // If run is already past incoming, treat finish as already accepted.
    let later_phases = [
        RunPhase::Ready,
        RunPhase::Processing,
        RunPhase::Done,
        RunPhase::Failed,
    ];
    for phase in &later_phases {
        let dir = config.purgery_root.run_dir(nickname, run_id, *phase);
        if dir.exists() {
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = %phase.as_str(),
                "finish-run: run already in later phase"
            );
            return Ok(());
        }
    }

    if !incoming_path.exists() {
        anyhow::bail!(
            "incoming directory does not exist for run {}/{} at '{}'",
            nickname.as_str(),
            run_id.as_str(),
            incoming_path.as_str()
        );
    }

    let lease_path = incoming_path.join("lease.toml");
    if lease_path.exists() {
        let lease_content =
            fs::read_to_string(&lease_path).with_context(|| "failed to read lease file")?;
        let lease: purgery_core::LeaseFile =
            toml::from_str(&lease_content).with_context(|| "failed to parse lease file")?;
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
        if now >= lease.expires_at_unix_secs {
            anyhow::bail!(
                "cannot finish run: incoming lease expired at {}",
                lease.expires_at_unix_secs
            );
        }
    } else {
        anyhow::bail!("cannot finish run: no lease file found, run may be incomplete");
    }

    let ready_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Ready);
    if ready_path.exists() {
        anyhow::bail!(
            "ready directory already exists for run {}/{} at '{}'",
            nickname.as_str(),
            run_id.as_str(),
            ready_path.as_str()
        );
    }

    if let Some(parent) = ready_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create ready parent: {}", parent.as_str()))?;
    }

    fs::rename(&incoming_path, &ready_path).with_context(|| {
        format!(
            "failed to move incoming to ready: {} -> {}",
            incoming_path.as_str(),
            ready_path.as_str()
        )
    })?;

    Ok(())
}
