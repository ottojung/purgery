use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use purgery_core::{
    Nickname, RunId, RunPhase, RunState, RunStatus, ServerConfig,
};
use std::fs;
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
                                        if lease.protocol_version != purgery_core::LEASE_FILE_VERSION
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
