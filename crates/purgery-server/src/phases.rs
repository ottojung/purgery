use anyhow::{Context, Result};
use camino::Utf8Path;
use purgery_core::{
    current_purgery_version, Nickname, ProcessingProgress, PurgeryRoot, RunId, RunPhase, RunState,
    RunStatus, ServerConfig, PROTOCOL_VERSION,
};
use std::fs;
use std::os::unix::io::AsRawFd;
use tracing::{info, warn};

/// Outcome of attempting to acquire a processor lock on an existing run
/// directory.
#[derive(Debug)]
pub(crate) enum ProcessorLockAttempt {
    /// Lock acquired. The caller may safely mutate the run.
    Acquired(ProcessingRunLock),
    /// Lock is held by another process. The caller must not mutate the run.
    Busy,
    /// The run directory does not exist. The caller should re-check state
    /// (the run may have been claimed, completed, or cleaned up already).
    Missing,
}

/// An exclusive file lock held on a processing run's `processor.lock` file.
///
/// The lock is automatically released when the `ProcessingRunLock` is dropped
/// (the file descriptor is closed, which releases the `flock`).
///
/// Only the process holding this lock may mutate the run's processing
/// directory.  If the lock cannot be acquired, another processor owns it
/// and the run must not be recovered or replayed.
#[derive(Debug)]
pub(crate) struct ProcessingRunLock {
    _file: std::fs::File,
    _path: camino::Utf8PathBuf,
}

impl ProcessingRunLock {
    /// Try to acquire an exclusive advisory lock on an existing directory's
    /// `processor.lock` file.
    ///
    /// Returns `Missing` if the run directory does not exist — the caller
    /// must re-check state rather than recreating the directory.
    /// Returns `Acquired` if the lock is obtained.
    /// Returns `Busy` if another process holds the lock.
    /// Returns `Err` on IO errors or lock setup failures.
    fn try_lock_existing_dir(run_dir: &Utf8Path) -> Result<ProcessorLockAttempt> {
        if !run_dir.exists() {
            return Ok(ProcessorLockAttempt::Missing);
        }
        let lock_path = run_dir.join("processor.lock");
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path.as_std_path())
        {
            Ok(f) => f,
            Err(e) => {
                anyhow::bail!("failed to open processor lock file {lock_path}: {e}")
            }
        };

        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            Ok(ProcessorLockAttempt::Acquired(ProcessingRunLock {
                _file: file,
                _path: lock_path,
            }))
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                Ok(ProcessorLockAttempt::Busy)
            } else {
                Err(err).with_context(|| format!("failed to lock processor: {lock_path}"))
            }
        }
    }
}

/// Try to acquire the processor lock on an existing run directory.
///
/// The directory must already exist — the caller must have confirmed it
/// exists before calling this function.  If the directory has been removed
/// between the check and the lock attempt, `Missing` is returned and the
/// caller should re-check state.
pub(crate) fn try_lock_existing_run_dir_processor(
    run_dir: &Utf8Path,
) -> Result<ProcessorLockAttempt> {
    ProcessingRunLock::try_lock_existing_dir(run_dir)
}

/// Probe the processor lock state without creating `processor.lock`.
///
/// This is a read-only observation used by `run-state`.  It never creates
/// the lock file.  If the file does not exist, the processor is idle.
/// If the file exists and the lock is busy, the processor is active.
/// If the file exists and the lock is free, the processor is idle.
///
/// Returns `None` if the run directory does not exist.
/// Returns `Some(true)` if the processor appears active.
/// Returns `Some(false)` if the processor appears idle (no file or free lock).
pub(crate) fn probe_processor_lock_readonly(run_dir: &Utf8Path) -> Result<Option<bool>> {
    if !run_dir.exists() {
        return Ok(None);
    }
    let lock_path = run_dir.join("processor.lock");
    if !lock_path.exists() {
        // No lock file — processor is definitely idle.
        return Ok(Some(false));
    }
    // File exists — try nonblocking exclusive lock WITHOUT create.
    let file = match std::fs::OpenOptions::new()
        .create(false)
        .read(true)
        .write(true)
        .open(lock_path.as_std_path())
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Race: file was deleted between exists check and open.
            return Ok(Some(false));
        }
        Err(e) => {
            anyhow::bail!("failed to open processor lock for probe: {lock_path}: {e}")
        }
    };
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        // Got the lock — release immediately (don't hold during probe).
        // Just close the file by letting it drop.
        Ok(Some(false))
    } else {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            Ok(Some(true))
        } else {
            Err(err).with_context(|| format!("failed to probe processor lock: {lock_path}"))
        }
    }
}

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
pub(crate) fn write_progress_best_effort(
    processing_path: &Utf8Path,
    nickname: &Nickname,
    run_id: &RunId,
    state: &str,
    entry_index: usize,
    entry_total: usize,
    current_entry: &str,
    current_transform: &str,
) {
    if let Err(error) = write_progress(
        processing_path,
        nickname,
        run_id,
        state,
        entry_index,
        entry_total,
        current_entry,
        current_transform,
    ) {
        warn!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                state = state,
                entry_index = entry_index,
                entry_total = entry_total,
                current_entry = current_entry,
                current_transform = current_transform,
            %error,
            "failed to write progress"
        );
    }
}

/// Result of checking an existing progress file before writing.
enum ExistingProgress {
    /// Compatible progress file exists with a valid started_at value.
    CompatibleStartedAt(u64),
    /// No progress file exists.
    Missing,
    /// Progress file exists but has missing/incompatible purgery_version.
    /// Must not be overwritten with current-version progress.
    Incompatible,
    /// Progress file exists but failed to parse (malformed).
    /// May be overwritten (conservatively treating as current-format corruption).
    Malformed,
}

/// Read `started_at_unix_secs` from an existing progress file, if present
/// and the envelope (nickname, run_id) matches the current run.
fn existing_progress_started_at(
    progress_path: &Utf8Path,
    nickname: &Nickname,
    run_id: &RunId,
) -> ExistingProgress {
    let content = match std::fs::read_to_string(progress_path.as_std_path()) {
        Ok(c) => c,
        Err(_) => return ExistingProgress::Missing,
    };
    // Probe raw TOML for version before full parse
    match purgery_core::probe_purgery_version_from_toml(&content) {
        Err(purgery_core::VersionProbeError::MissingVersion) => {
            return ExistingProgress::Incompatible;
        }
        Err(purgery_core::VersionProbeError::InvalidToml(_)) => {
            // Invalid TOML — could be old corruption. Overwrite
            // conservatively (treat as malformed current).
            return ExistingProgress::Malformed;
        }
        Ok(version) => {
            if purgery_core::require_compatible_purgery_version(&version, "progress").is_err() {
                return ExistingProgress::Incompatible;
            }
        }
    }
    let progress: ProcessingProgress = match toml::from_str(&content) {
        Ok(p) => p,
        Err(_) => return ExistingProgress::Malformed,
    };
    if progress.nickname != nickname.as_str() || progress.run_id != run_id.as_str() {
        return ExistingProgress::Malformed;
    }
    ExistingProgress::CompatibleStartedAt(progress.started_at_unix_secs)
}

/// Validate progress state semantics before writing.
/// Returns an error for invalid combinations.
fn validate_progress_update(
    state: &str,
    entry_index: usize,
    entry_total: usize,
    current_entry: &str,
    current_transform: &str,
) -> Result<()> {
    match state {
        "processing_started" | "publishing_status" => {
            if !current_entry.is_empty() {
                anyhow::bail!("run-level progress state {state} must not have current_entry");
            }
            if !current_transform.is_empty() {
                anyhow::bail!("run-level progress state {state} must not have current_transform");
            }
            Ok(())
        }
        "processing_entry" => {
            if entry_total == 0 {
                anyhow::bail!("per-entry progress state {state} must have entry_total > 0");
            }
            if entry_index >= entry_total {
                anyhow::bail!("entry_index must be less than entry_total");
            }
            if current_entry.is_empty() {
                anyhow::bail!("per-entry progress state {state} must have current_entry");
            }
            if !current_transform.is_empty() {
                anyhow::bail!("processing_entry must not have current_transform");
            }
            Ok(())
        }
        "transform_started" | "transform_running" | "transform_finished" => {
            if entry_total == 0 {
                anyhow::bail!("per-entry progress state {state} must have entry_total > 0");
            }
            if entry_index >= entry_total {
                anyhow::bail!("entry_index must be less than entry_total");
            }
            if current_entry.is_empty() {
                anyhow::bail!("per-entry progress state {state} must have current_entry");
            }
            if current_transform.is_empty() {
                anyhow::bail!("transform progress state {state} must have current_transform");
            }
            Ok(())
        }
        _ => anyhow::bail!("unknown progress state: {state}"),
    }
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
    current_transform: &str,
) -> Result<()> {
    validate_progress_update(
        state,
        entry_index,
        entry_total,
        current_entry,
        current_transform,
    )?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let final_path = processing_path.join("progress.toml");
    let started_at = match existing_progress_started_at(&final_path, nickname, run_id) {
        ExistingProgress::CompatibleStartedAt(v) => v,
        ExistingProgress::Incompatible => {
            warn!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                path = %final_path.as_str(),
                "incompatible existing progress; not overwriting",
            );
            return Ok(());
        }
        ExistingProgress::Missing | ExistingProgress::Malformed => now,
    };
    let progress = ProcessingProgress {
        protocol_version: purgery_core::PROGRESS_FILE_VERSION,
        purgery_version: current_purgery_version().to_string(),
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        phase: "processing".to_string(),
        state: state.to_owned(),
        entry_index,
        entry_total,
        current_entry: current_entry.to_owned(),
        current_transform: current_transform.to_owned(),
        started_at_unix_secs: started_at,
        updated_at_unix_secs: now,
    };
    let content = toml::to_string(&progress)
        .map_err(|e| anyhow::anyhow!("failed to serialize progress: {e}"))?;
    let tmp = processing_path.join("progress.toml.tmp");
    fs::write(&tmp, &content)
        .with_context(|| format!("failed to write progress: {}", tmp.as_str()))?;
    fs::rename(&tmp, &final_path)
        .with_context(|| format!("failed to publish progress: {}", final_path.as_str()))?;
    Ok(())
}

pub(crate) fn write_run_failure(
    work_dir: &PurgeryRoot,
    nickname: &Nickname,
    run_id: &RunId,
    error_msg: &str,
) -> Result<()> {
    let processing_path = work_dir.run_dir(nickname, run_id, RunPhase::Processing);
    let status = RunStatus {
        purgery_version: current_purgery_version().to_string(),
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

    let failed_path = work_dir.run_dir(nickname, run_id, RunPhase::Failed);
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

    // Clean up the processor lock file from the terminal directory.
    let lock_path = failed_path.join("processor.lock");
    let _ = fs::remove_file(lock_path.as_std_path());

    Ok(())
}

pub(crate) fn find_runs_in_phase(
    work_dir: &PurgeryRoot,
    phase: RunPhase,
) -> Result<Vec<(Nickname, RunId)>> {
    let mut runs = Vec::new();
    let purgery_path = work_dir.as_path();

    if !purgery_path.exists() {
        return Ok(runs);
    }

    for entry in fs::read_dir(purgery_path)
        .with_context(|| format!("failed to read work directory: {}", purgery_path.as_str()))?
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

pub fn find_ready_runs(work_dir: &PurgeryRoot) -> Result<Vec<(Nickname, RunId)>> {
    find_runs_in_phase(work_dir, RunPhase::Ready)
}

pub fn find_processing_runs(work_dir: &PurgeryRoot) -> Result<Vec<(Nickname, RunId)>> {
    find_runs_in_phase(work_dir, RunPhase::Processing)
}

pub(crate) fn finalize_processing_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    state: &RunState,
) -> Result<()> {
    let processing_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Processing);
    let dest_phase = match state {
        RunState::Done | RunState::Partial => RunPhase::Done,
        RunState::Failed => RunPhase::Failed,
    };
    let dest_path = config.work_dir.run_dir(nickname, run_id, dest_phase);
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

    // Clean up the processor lock file in the destination.
    let lock_path = dest_path.join("processor.lock");
    let _ = fs::remove_file(lock_path.as_std_path());
    Ok(())
}

pub fn move_to_failed(work_dir: &PurgeryRoot, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    let processing_path = work_dir.run_dir(nickname, run_id, RunPhase::Processing);
    let failed_path = work_dir.run_dir(nickname, run_id, RunPhase::Failed);

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

        // Clean up the processor lock file from the terminal directory.
        let lock_path = failed_path.join("processor.lock");
        let _ = fs::remove_file(lock_path.as_std_path());
    }

    Ok(())
}

pub fn begin_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<String> {
    let phases = [
        RunPhase::Incoming,
        RunPhase::Ready,
        RunPhase::Processing,
        RunPhase::Done,
        RunPhase::Failed,
    ];
    for phase in &phases {
        let phase_path = config.work_dir.run_dir(nickname, run_id, *phase);
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
        .work_dir
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
        protocol_version: purgery_core::LEASE_FILE_VERSION,
        purgery_version: current_purgery_version().to_string(),
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
        protocol_version: PROTOCOL_VERSION,
        purgery_version: current_purgery_version().to_string(),
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
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Incoming);

    // If run is already past incoming, treat finish as already accepted.
    let later_phases = [
        RunPhase::Ready,
        RunPhase::Processing,
        RunPhase::Done,
        RunPhase::Failed,
    ];
    for phase in &later_phases {
        let dir = config.work_dir.run_dir(nickname, run_id, *phase);
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
        // Probe raw TOML for version before full deserialization
        if let Err(e) = purgery_core::probe_purgery_version_from_toml(&lease_content) {
            anyhow::bail!(
                "cannot finish run: lease is missing purgery_version or has invalid TOML \
                 (producer version cannot be established) at '{}': {e}",
                lease_path.as_str(),
            );
        }
        let lease: purgery_core::LeaseFile =
            toml::from_str(&lease_content).with_context(|| "failed to parse lease file")?;
        purgery_core::require_compatible_purgery_version(&lease.purgery_version, "lease")
            .with_context(|| "incompatible lease version")?;
        if lease.protocol_version != purgery_core::LEASE_FILE_VERSION {
            anyhow::bail!(
                "lease protocol version {} does not match expected {}",
                lease.protocol_version,
                purgery_core::LEASE_FILE_VERSION
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

    let ready_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Ready);
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
