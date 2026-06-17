use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use purgery_core::{Nickname, RunId, RunPhase, RunState, RunStatus, ServerConfig};
use std::fs;
use std::os::unix::io::AsRawFd;
use tracing::{info, warn};

use crate::phases::publish_status_atomic;

pub fn run_gc(config: &ServerConfig) -> Result<()> {
    let gc_config = &config.gc;
    let purgery_path = config.work_dir.as_path();

    if !purgery_path.exists() {
        return Ok(());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for entry in fs::read_dir(purgery_path.as_std_path())
        .with_context(|| format!("failed to read work directory: {}", purgery_path.as_str()))?
    {
        let entry = entry?;
        let nickname_path = entry.path();
        if !nickname_path.is_dir() {
            continue;
        }
        let nickname_str = match nickname_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let Ok(nickname) = Nickname::new(nickname_str) else {
            continue;
        };

        let incoming_dir = nickname_path.join("incoming");
        if !incoming_dir.exists() {
            continue;
        }

        for run_entry in fs::read_dir(&incoming_dir)
            .with_context(|| format!("failed to read incoming dir: {}", incoming_dir.display()))?
        {
            let run_entry = run_entry?;
            let run_path = run_entry.path();
            if !run_path.is_dir() {
                continue;
            }
            let run_id_str = match run_path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_owned(),
                None => continue,
            };
            let Ok(run_id) = RunId::new(run_id_str) else {
                continue;
            };

            let lease_path = Utf8PathBuf::from_path_buf(run_path.join("lease.toml"))
                .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref()));

            let expired = if lease_path.exists() {
                match fs::read_to_string(lease_path.as_std_path()) {
                    Ok(content) => {
                        // Probe purgery_version from raw TOML before full
                        // deserialization so we can distinguish old/incompatible
                        // leases from malformed current leases.
                        // Old/incompatible leases must be skipped entirely —
                        // they must NOT be collected, quarantined, or moved
                        // to failed.
                        match purgery_core::probe_purgery_version_from_toml(&content) {
                            Err(purgery_core::VersionProbeError::MissingVersion) => {
                                warn!(
                                    nickname = %nickname.as_str(),
                                    run_id = %run_id.as_str(),
                                    lease_path = %lease_path.as_str(),
                                    "gc: lease missing purgery_version (too old); \
                                     skipping — not collecting",
                                );
                                continue;
                            }
                            Err(purgery_core::VersionProbeError::InvalidToml(e)) => {
                                warn!(
                                    nickname = %nickname.as_str(),
                                    run_id = %run_id.as_str(),
                                    lease_path = %lease_path.as_str(),
                                    error = %e,
                                    "gc: lease has invalid TOML (cannot determine version); \
                                     skipping — not collecting",
                                );
                                continue;
                            }
                            Ok(version) => {
                                let version_ok = purgery_core::require_compatible_purgery_version(
                                    &version, "lease",
                                )
                                .is_ok();
                                if !version_ok {
                                    warn!(
                                        nickname = %nickname.as_str(),
                                        run_id = %run_id.as_str(),
                                        lease_path = %lease_path.as_str(),
                                        lease_version = %version,
                                        current_version =
                                            %purgery_core::current_purgery_version(),
                                        "gc: lease has incompatible purgery_version; \
                                         skipping — not collecting",
                                    );
                                    continue;
                                }
                                match toml::from_str::<purgery_core::LeaseFile>(&content) {
                                    Ok(lease) => {
                                        if lease.protocol_version
                                            != purgery_core::LEASE_FILE_VERSION
                                            || lease.nickname != nickname.as_str()
                                            || lease.run_id != run_id.as_str()
                                        {
                                            warn!(
                                                nickname = %nickname.as_str(),
                                                run_id = %run_id.as_str(),
                                                lease_path = %lease_path.as_str(),
                                                lease_protocol = lease.protocol_version,
                                                lease_nickname = %lease.nickname,
                                                lease_run_id = %lease.run_id,
                                                "gc: lease envelope mismatch",
                                            );
                                            true
                                        } else {
                                            now >= lease.expires_at_unix_secs
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            nickname = %nickname.as_str(),
                                            run_id = %run_id.as_str(),
                                            lease_path = %lease_path.as_str(),
                                            error = %e,
                                            "gc: failed to parse lease; treating as expired",
                                        );
                                        true
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => true,
                }
            } else {
                let metadata = match fs::metadata(&run_path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .unwrap_or_default()
                    .as_secs();
                now.saturating_sub(mtime) > gc_config.incoming_lease_secs * 2
            };

            if !expired {
                continue;
            }

            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                "gc: collecting expired incoming run"
            );

            let failed_path = config
                .work_dir
                .run_dir(&nickname, &run_id, RunPhase::Failed);
            if failed_path.exists() {
                let quarantine_name = format!("gc-quarantine-{}-{}", run_id.as_str(), now);
                let quarantine_path = config.work_dir.run_dir(
                    &nickname,
                    &RunId::new(quarantine_name).unwrap(),
                    RunPhase::Failed,
                );
                if let Some(parent) = quarantine_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if fs::rename(&run_path, quarantine_path.as_std_path()).is_ok() {
                    let status = RunStatus {
                        purgery_version: purgery_core::current_purgery_version().to_string(),
                        run_id: run_id.clone(),
                        nickname: nickname.clone(),
                        state: RunState::Failed,
                        entries: vec![],
                        error: Some("abandoned upload expired (quarantined)".into()),
                    };
                    if let Err(error) = publish_status_atomic(&quarantine_path, &status) {
                        warn!(nickname = %nickname.as_str(), run_id = %run_id.as_str(), error = %error, "gc: failed to publish quarantine status");
                    }
                    let files_dir = quarantine_path.join("files");
                    if files_dir.exists() {
                        if let Err(error) = fs::remove_dir_all(files_dir.as_std_path()) {
                            warn!(nickname = %nickname.as_str(), run_id = %run_id.as_str(), error = %error, "gc: failed to remove quarantined files");
                        }
                    }
                }
                continue;
            }

            if let Some(parent) = failed_path.parent() {
                let _ = fs::create_dir_all(parent.as_std_path());
            }

            if let Err(e) = fs::rename(&run_path, failed_path.as_std_path()) {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    error = %e,
                    "gc: failed to claim abandoned run"
                );
                continue;
            }

            let status = RunStatus {
                purgery_version: purgery_core::current_purgery_version().to_string(),
                run_id: run_id.clone(),
                nickname: nickname.clone(),
                state: RunState::Failed,
                entries: vec![],
                error: Some("abandoned upload expired".into()),
            };
            if let Err(error) = publish_status_atomic(&failed_path, &status) {
                warn!(nickname = %nickname.as_str(), run_id = %run_id.as_str(), error = %error, "gc: failed to publish failed status");
            }

            let files_dir = failed_path.join("files");
            if files_dir.exists() {
                if let Err(error) = fs::remove_dir_all(files_dir.as_std_path()) {
                    warn!(nickname = %nickname.as_str(), run_id = %run_id.as_str(), error = %error, "gc: failed to remove collected files");
                }
            }
        }
    }

    Ok(())
}

/// Return the path to the global GC lock file.
fn gc_lock_path(work_dir: &camino::Utf8Path) -> camino::Utf8PathBuf {
    work_dir.join(".gc.lock")
}

/// Try to acquire the global GC lock (nonblocking).
/// Returns `Ok(true)` if the lock was acquired, `Ok(false)` if busy.
fn try_lock_gc(work_dir: &camino::Utf8Path) -> Result<bool> {
    let lock_path = gc_lock_path(work_dir);
    // Ensure parent exists
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent.as_std_path());
    }
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path.as_std_path())
    {
        Ok(f) => f,
        Err(e) => anyhow::bail!("failed to open GC lock {lock_path}: {e}"),
    };
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        Ok(true)
    } else {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            Ok(false)
        } else {
            Err(err).with_context(|| format!("failed to lock GC: {lock_path}"))
        }
    }
}

/// Short-running: start a detached GC worker and return.
pub fn start_gc(config: &ServerConfig, config_path: &str) -> Result<StartGcResult> {
    let purgery_path = config.work_dir.as_path();
    match try_lock_gc(purgery_path) {
        Ok(true) => {
            // Lock acquired — spawn detached gc-worker.
            let exe =
                std::env::current_exe().with_context(|| "cannot determine current executable")?;
            match std::process::Command::new(&exe)
                .arg("--config")
                .arg(config_path)
                .arg("gc-worker")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    info!(pid = child.id(), "spawned gc-worker");
                    Ok(StartGcResult::Spawned)
                }
                Err(e) => Ok(StartGcResult::SpawnFailed {
                    message: format!("{e}"),
                }),
            }
        }
        Ok(false) => Ok(StartGcResult::AlreadyActive),
        Err(e) => Ok(StartGcResult::SpawnFailed {
            message: format!("{e}"),
        }),
    }
}

/// Long-running: acquire GC lock and run GC.
pub fn gc_worker(config: &ServerConfig) -> Result<GcWorkerResult> {
    let purgery_path = config.work_dir.as_path();
    match try_lock_gc(purgery_path) {
        Ok(true) => {
            info!("gc-worker acquired lock; running garbage collection");
            if let Err(e) = run_gc(config) {
                warn!(error = %e, "GC failed");
            }
            Ok(GcWorkerResult::Completed)
        }
        Ok(false) => {
            info!("gc-worker: another GC worker is active; skipping");
            Ok(GcWorkerResult::SkippedLockBusy)
        }
        Err(e) => Err(e),
    }
}

/// Outcome of `start_gc`.
#[derive(Debug)]
pub enum StartGcResult {
    Spawned,
    AlreadyActive,
    SpawnFailed { message: String },
}

impl StartGcResult {
    /// Serialize as simple TOML for the CLI response.
    pub fn to_toml(&self) -> String {
        let (action, message) = match self {
            StartGcResult::Spawned => ("spawned_gc", "background GC started"),
            StartGcResult::AlreadyActive => ("already_active", "another GC worker is active"),
            StartGcResult::SpawnFailed { message } => ("spawn_failed", message.as_str()),
        };
        format!(
            r#"protocol_version = {}
purgery_version = "{}"

action = {action:?}
message = {message:?}
"#,
            purgery_core::PROTOCOL_VERSION,
            purgery_core::current_purgery_version(),
        )
    }
}

/// Outcome of `gc_worker`.
#[derive(Debug)]
pub enum GcWorkerResult {
    Completed,
    SkippedLockBusy,
}
