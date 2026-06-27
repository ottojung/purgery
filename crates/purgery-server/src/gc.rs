use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{Nickname, RunId, RunPhase, RunState, RunStatus, ServerConfig};
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

use crate::phases::probe_processor_lock_readonly;

pub fn run_gc(config: &ServerConfig) -> Result<()> {
    let root = config.work_dir.as_path();
    if !root.exists() {
        return Ok(());
    }
    let now = unix_now();
    for entry in fs::read_dir(root.as_std_path())
        .with_context(|| format!("failed to read work directory: {}", root.as_str()))?
    {
        let entry = entry?;
        let nickname_path = entry.path();
        if !nickname_path.is_dir() {
            continue;
        }
        let Some(name) = nickname_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(nickname) = Nickname::new(name.to_owned()) else {
            collect_invalid_nickname_dir(config, name, &nickname_path, now)?;
            continue;
        };
        for phase in [
            RunPhase::Incoming,
            RunPhase::Ready,
            RunPhase::Processing,
            RunPhase::Done,
            RunPhase::Failed,
        ] {
            collect_phase(
                config,
                &nickname,
                &Utf8PathBuf::from_path_buf(nickname_path.clone()).unwrap(),
                phase,
                now,
            )?;
        }
    }
    Ok(())
}

fn collect_invalid_nickname_dir(
    config: &ServerConfig,
    name: &str,
    path: &std::path::Path,
    now: u64,
) -> Result<()> {
    let utf8_path = Utf8PathBuf::from_path_buf(path.to_path_buf()).unwrap();
    let expiry = mtime_secs(&utf8_path)?.saturating_add(config.gc.orphan_retention_secs);
    if now >= expiry {
        warn!(dirname=%name, reason="invalid nickname", seconds_past_expiry=now.saturating_sub(expiry), action="delete", "gc: removing invalid top-level work_dir directory");
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove invalid top-level directory: {}", name))?;
    } else {
        warn!(dirname=%name, reason="invalid nickname", seconds_until_deletion=expiry.saturating_sub(now), action="retain", "gc: retaining invalid top-level work_dir directory under orphan policy");
    }
    Ok(())
}

fn collect_phase(
    config: &ServerConfig,
    nickname: &Nickname,
    nickname_path: &Utf8Path,
    phase: RunPhase,
    now: u64,
) -> Result<()> {
    let phase_dir = nickname_path.join(phase.as_str());
    if !phase_dir.exists() {
        return Ok(());
    }
    for run_entry in fs::read_dir(phase_dir.as_std_path())
        .with_context(|| format!("failed to read {} dir: {}", phase.as_str(), phase_dir))?
    {
        let run_entry = run_entry?;
        let run_path = Utf8PathBuf::from_path_buf(run_entry.path()).unwrap();
        if !run_path.is_dir() {
            continue;
        }
        let Some(id) = run_path.file_name() else {
            continue;
        };
        let Ok(run_id) = RunId::new(id.to_owned()) else {
            collect_unknown(
                config,
                nickname,
                phase,
                &run_path,
                id,
                now,
                "invalid run id",
            )?;
            continue;
        };
        collect_run(config, nickname, &run_id, phase, &run_path, now)?;
    }
    Ok(())
}

fn collect_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    phase: RunPhase,
    run_path: &Utf8Path,
    now: u64,
) -> Result<()> {
    match phase {
        RunPhase::Incoming => collect_incoming(config, nickname, run_id, run_path, now),
        RunPhase::Ready => expire_by_mtime(
            config,
            nickname,
            run_id,
            phase,
            run_path,
            now,
            config.gc.ready_retention_secs,
        ),
        RunPhase::Processing => collect_processing(config, nickname, run_id, run_path, now),
        RunPhase::Done => collect_done(config, nickname, run_id, run_path, now),
        RunPhase::Failed => expire_by_mtime(
            config,
            nickname,
            run_id,
            phase,
            run_path,
            now,
            config.gc.failed_retention_secs,
        ),
    }
}

fn collect_incoming(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    run_path: &Utf8Path,
    now: u64,
) -> Result<()> {
    let lease_path = run_path.join("lease.toml");
    let (expired, reason, expiry) = match fs::read_to_string(lease_path.as_std_path()) {
        Ok(content) => match toml::from_str::<purgery_core::LeaseFile>(&content) {
            Ok(lease) if lease.nickname == nickname.as_str() && lease.run_id == run_id.as_str() => {
                (
                    now >= lease.expires_at_unix_secs,
                    "lease",
                    lease.expires_at_unix_secs,
                )
            }
            Ok(_) => orphan_expiry(config, run_path, now, "lease envelope mismatch")?,
            Err(_) => orphan_expiry(config, run_path, now, "malformed lease")?,
        },
        Err(_) => orphan_expiry(config, run_path, now, "missing lease")?,
    };
    log_decision(
        nickname,
        run_id,
        RunPhase::Incoming,
        expired,
        now,
        expiry,
        reason,
    );
    if expired {
        remove_run(run_path, nickname, run_id, RunPhase::Incoming, reason)?;
    }
    Ok(())
}

fn collect_processing(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    run_path: &Utf8Path,
    now: u64,
) -> Result<()> {
    let expiry = mtime_secs(run_path)?.saturating_add(config.gc.processing_retention_secs);
    let expired = now >= expiry;
    match probe_processor_lock_readonly(run_path)? {
        Some(true) => {
            if expired {
                warn!(nickname=%nickname.as_str(), run_id=%run_id.as_str(), phase="processing", seconds_past_expiry=now.saturating_sub(expiry), "gc: locked processing request is expired; keeping active state until lock is released");
            } else {
                debug!(nickname=%nickname.as_str(), run_id=%run_id.as_str(), phase="processing", seconds_until_deletion=expiry.saturating_sub(now), "gc: locked processing request retained");
            }
        }
        _ => {
            log_decision(
                nickname,
                run_id,
                RunPhase::Processing,
                expired,
                now,
                expiry,
                "processing retention",
            );
            if expired {
                remove_run(
                    run_path,
                    nickname,
                    run_id,
                    RunPhase::Processing,
                    "processing retention expired",
                )?;
            }
        }
    }
    Ok(())
}

fn collect_done(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    run_path: &Utf8Path,
    now: u64,
) -> Result<()> {
    let original_mtime = mtime_secs(run_path)?;
    let expiry = original_mtime.saturating_add(config.gc.done_retention_secs);
    let expired = now >= expiry;

    if expired {
        log_decision(
            nickname,
            run_id,
            RunPhase::Done,
            true,
            now,
            expiry,
            "retention",
        );
        return remove_run(
            run_path,
            nickname,
            run_id,
            RunPhase::Done,
            "retention expired",
        );
    }

    if terminal_state(run_path).is_ok_and(|state| state == RunState::Done) {
        prune_success_terminal(run_path)
            .with_context(|| format!("failed to prune successful terminal state: {run_path}"))?;
    }

    log_decision(
        nickname,
        run_id,
        RunPhase::Done,
        false,
        now,
        expiry,
        "retention",
    );
    Ok(())
}

fn terminal_state(run_path: &Utf8Path) -> Result<RunState> {
    let status_path = run_path.join("status.toml");
    let status = RunStatus::from_toml(&fs::read_to_string(status_path.as_std_path())?)
        .with_context(|| format!("failed to read terminal status: {status_path}"))?;
    Ok(status.state)
}

pub(crate) fn prune_success_terminal(run_path: &Utf8Path) -> Result<()> {
    for name in ["files", "work"] {
        let p = run_path.join(name);
        if p.exists() {
            fs::remove_dir_all(p.as_std_path()).with_context(|| format!("failed to remove {p}"))?;
        }
    }
    for name in ["lease.toml", "progress.toml", "processor.lock"] {
        let p = run_path.join(name);
        if p.exists() {
            fs::remove_file(p.as_std_path()).with_context(|| format!("failed to remove {p}"))?;
        }
    }
    for e in fs::read_dir(run_path.as_std_path())? {
        let p = Utf8PathBuf::from_path_buf(e?.path()).unwrap();
        if p.file_name().is_some_and(|n| n.ends_with(".tmp")) {
            fs::remove_file(p.as_std_path()).with_context(|| format!("failed to remove {p}"))?;
        }
    }
    Ok(())
}

fn expire_by_mtime(
    _config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    phase: RunPhase,
    run_path: &Utf8Path,
    now: u64,
    retention: u64,
) -> Result<()> {
    let expiry = mtime_secs(run_path)?.saturating_add(retention);
    let expired = now >= expiry;
    log_decision(nickname, run_id, phase, expired, now, expiry, "retention");
    if expired {
        remove_run(run_path, nickname, run_id, phase, "retention expired")?;
    }
    Ok(())
}

fn collect_unknown(
    config: &ServerConfig,
    nickname: &Nickname,
    phase: RunPhase,
    run_path: &Utf8Path,
    run_id: &str,
    now: u64,
    reason: &str,
) -> Result<()> {
    let expiry = mtime_secs(run_path)?.saturating_add(config.gc.orphan_retention_secs);
    if now >= expiry {
        warn!(nickname=%nickname.as_str(), run_id, phase=%phase.as_str(), reason, seconds_past_expiry=now.saturating_sub(expiry), "gc: removing unknown request state");
        fs::remove_dir_all(run_path.as_std_path())
            .with_context(|| format!("failed to remove orphan request: {run_path}"))?;
    } else {
        warn!(nickname=%nickname.as_str(), run_id, phase=%phase.as_str(), reason, seconds_until_deletion=expiry.saturating_sub(now), "gc: retaining unknown request state under orphan policy");
    }
    Ok(())
}

fn orphan_expiry(
    config: &ServerConfig,
    run_path: &Utf8Path,
    now: u64,
    reason: &'static str,
) -> Result<(bool, &'static str, u64)> {
    let expiry = mtime_secs(run_path)?.saturating_add(config.gc.orphan_retention_secs);
    Ok((now >= expiry, reason, expiry))
}

fn remove_run(
    run_path: &Utf8Path,
    nickname: &Nickname,
    run_id: &RunId,
    phase: RunPhase,
    reason: &str,
) -> Result<()> {
    info!(nickname=%nickname.as_str(), run_id=%run_id.as_str(), phase=%phase.as_str(), reason, action="delete", "gc: deleting expired server work state");
    fs::remove_dir_all(run_path.as_std_path()).with_context(|| {
        format!(
            "failed to remove expired {} run {}: {}",
            phase.as_str(),
            run_id.as_str(),
            run_path
        )
    })
}

fn log_decision(
    nickname: &Nickname,
    run_id: &RunId,
    phase: RunPhase,
    expired: bool,
    now: u64,
    expiry: u64,
    reason: &str,
) {
    if expired {
        info!(nickname=%nickname.as_str(), run_id=%run_id.as_str(), phase=%phase.as_str(), reason, seconds_past_expiry=now.saturating_sub(expiry), expired=true, "gc: request expired");
    } else {
        debug!(nickname=%nickname.as_str(), run_id=%run_id.as_str(), phase=%phase.as_str(), reason, seconds_until_deletion=expiry.saturating_sub(now), expired=false, "gc: request retained");
    }
}

fn mtime_secs(path: &Utf8Path) -> Result<u64> {
    Ok(fs::metadata(path.as_std_path())?
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs())
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use purgery_core::{GCConfig, RunState, RunStatus, ServerWorkDir};
    use std::collections::BTreeMap;
    use std::os::unix::io::AsRawFd;

    fn config(work_dir: Utf8PathBuf) -> ServerConfig {
        ServerConfig {
            work_dir: ServerWorkDir::new(work_dir).unwrap(),
            transforms: BTreeMap::new(),
            gc: GCConfig {
                incoming_lease_secs: 1,
                heartbeat_interval_secs: 1,
                ready_retention_secs: 0,
                processing_retention_secs: 0,
                done_retention_secs: 0,
                failed_retention_secs: 0,
                orphan_retention_secs: 86400,
            },
            logging: Default::default(),
        }
    }

    fn status(nickname: &Nickname, run_id: &RunId, state: RunState) -> RunStatus {
        RunStatus {
            purgery_version: purgery_core::current_purgery_version().to_string(),
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            state,
            entries: vec![],
            error: None,
        }
    }

    fn set_dir_mtime(path: &Utf8Path, secs_since_epoch: i64) {
        filetime::set_file_mtime(
            path.as_std_path(),
            filetime::FileTime::from_unix_time(secs_since_epoch, 0),
        )
        .unwrap();
    }

    #[test]
    fn gc_prunes_successful_done_payload_before_observation_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        cfg.gc.done_retention_secs = 86400;
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("done-prune".into()).unwrap();
        let done = cfg.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        fs::create_dir_all(done.join("files/a")).unwrap();
        fs::create_dir_all(done.join("work")).unwrap();
        fs::write(done.join("files/a/input.txt"), "payload").unwrap();
        fs::write(done.join("lease.toml"), "lease").unwrap();
        fs::write(done.join("progress.toml"), "progress").unwrap();
        fs::write(done.join("processor.lock"), "lock").unwrap();
        fs::write(done.join("status.toml.tmp"), "tmp").unwrap();
        fs::write(done.join("run.toml"), "destination = \"x\"\n").unwrap();
        fs::write(done.join("manifest.toml"), "entries = []\n").unwrap();
        fs::write(
            done.join("status.toml"),
            status(&nickname, &run_id, RunState::Done)
                .to_toml()
                .unwrap(),
        )
        .unwrap();

        run_gc(&cfg).unwrap();

        assert!(done.exists());
        for path in [
            "files",
            "work",
            "lease.toml",
            "progress.toml",
            "processor.lock",
            "status.toml.tmp",
        ] {
            assert!(!done.join(path).exists(), "{path} must be pruned");
        }
        for path in ["status.toml", "run.toml", "manifest.toml"] {
            assert!(done.join(path).exists(), "{path} must remain observable");
        }
    }

    #[test]
    fn gc_expires_terminal_and_work_phases() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        let nickname = Nickname::new("laptop".into()).unwrap();
        for (id, phase, state) in [
            ("old-done", RunPhase::Done, Some(RunState::Done)),
            ("old-failed", RunPhase::Failed, Some(RunState::Failed)),
            ("old-ready", RunPhase::Ready, None),
            ("old-processing", RunPhase::Processing, None),
        ] {
            let run_id = RunId::new(id.into()).unwrap();
            let dir = cfg.work_dir.run_dir(&nickname, &run_id, phase);
            fs::create_dir_all(dir.join("files")).unwrap();
            if let Some(state) = state {
                fs::write(
                    dir.join("status.toml"),
                    status(&nickname, &run_id, state).to_toml().unwrap(),
                )
                .unwrap();
            }
        }

        run_gc(&cfg).unwrap();

        for (id, phase) in [
            ("old-done", RunPhase::Done),
            ("old-failed", RunPhase::Failed),
            ("old-ready", RunPhase::Ready),
            ("old-processing", RunPhase::Processing),
        ] {
            let run_id = RunId::new(id.into()).unwrap();
            assert!(!cfg.work_dir.run_dir(&nickname, &run_id, phase).exists());
        }
    }

    #[test]
    fn gc_retains_fresh_malformed_incoming_under_orphan_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("fresh-bad".into()).unwrap();
        let incoming = cfg.work_dir.run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(incoming.join("files")).unwrap();
        fs::write(incoming.join("lease.toml"), "not toml").unwrap();

        run_gc(&cfg).unwrap();

        assert!(incoming.exists());
        assert!(incoming.join("files").exists());
    }

    #[test]
    fn gc_removes_old_malformed_incoming_after_orphan_window() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        cfg.gc.orphan_retention_secs = 3600;
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("old-bad".into()).unwrap();
        let incoming = cfg.work_dir.run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(incoming.join("files")).unwrap();
        fs::write(incoming.join("lease.toml"), "not toml").unwrap();

        let now = unix_now();
        set_dir_mtime(&incoming, now as i64 - 7200);

        run_gc(&cfg).unwrap();

        assert!(!incoming.exists(), "old orphan incoming must be removed");
    }

    #[test]
    fn gc_expired_done_dir_deleted_without_pruning_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        cfg.gc.done_retention_secs = 3600;
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("expired-done".into()).unwrap();
        let done = cfg.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        fs::create_dir_all(done.join("files/a")).unwrap();
        fs::create_dir_all(done.join("work")).unwrap();
        fs::write(done.join("files/a/input.txt"), "payload").unwrap();
        fs::write(done.join("lease.toml"), "lease").unwrap();
        fs::write(done.join("progress.toml"), "progress").unwrap();
        fs::write(done.join("processor.lock"), "lock").unwrap();
        fs::write(done.join("run.toml"), "destination = \"x\"\n").unwrap();
        fs::write(done.join("manifest.toml"), "entries = []\n").unwrap();
        fs::write(
            done.join("status.toml"),
            status(&nickname, &run_id, RunState::Done)
                .to_toml()
                .unwrap(),
        )
        .unwrap();

        let now = unix_now();
        set_dir_mtime(&done, now as i64 - 7200);

        run_gc(&cfg).unwrap();

        assert!(
            !done.exists(),
            "expired done directory with stale payload must be fully deleted, not merely pruned"
        );
    }

    #[test]
    fn gc_removes_old_invalid_top_level_nickname_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        cfg.gc.orphan_retention_secs = 3600;
        let invalid_dir = cfg.work_dir.as_path().join("bad!nick name");
        fs::create_dir_all(invalid_dir.join("incoming/some-run")).unwrap();

        let now = unix_now();
        set_dir_mtime(&invalid_dir, now as i64 - 7200);

        run_gc(&cfg).unwrap();

        assert!(
            !invalid_dir.exists(),
            "old invalid top-level directory must be removed"
        );
    }

    #[test]
    fn gc_retains_fresh_invalid_top_level_nickname_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        cfg.gc.orphan_retention_secs = 86400;
        let invalid_dir = cfg.work_dir.as_path().join("bad!nick name");
        fs::create_dir_all(invalid_dir.join("incoming/some-run")).unwrap();

        run_gc(&cfg).unwrap();

        assert!(
            invalid_dir.exists(),
            "fresh invalid top-level directory must be retained under orphan policy"
        );
    }

    #[test]
    fn gc_does_not_delete_locked_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        cfg.gc.processing_retention_secs = 0;
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("locked-processing".into()).unwrap();
        let processing = cfg
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.join("files")).unwrap();

        let lock_path = processing.join("processor.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path.as_std_path())
            .unwrap();
        let fd = lock_file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(ret, 0, "must acquire lock for test");

        run_gc(&cfg).unwrap();

        assert!(
            processing.exists(),
            "locked processing must not be deleted even if expired"
        );

        drop(lock_file);
    }

    #[test]
    fn gc_removes_expired_unlocked_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        cfg.gc.processing_retention_secs = 0;
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("old-processing".into()).unwrap();
        let processing = cfg
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.join("files")).unwrap();

        run_gc(&cfg).unwrap();

        assert!(
            !processing.exists(),
            "expired unlocked processing must be removed"
        );
    }

    #[test]
    fn fresh_done_with_stale_payload_is_pruned_not_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        cfg.gc.done_retention_secs = 86400;
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("fresh-done".into()).unwrap();
        let done = cfg.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        fs::create_dir_all(done.join("files/a")).unwrap();
        fs::create_dir_all(done.join("work")).unwrap();
        fs::write(done.join("files/a/input.txt"), "payload").unwrap();
        fs::write(done.join("lease.toml"), "lease").unwrap();
        fs::write(done.join("progress.toml"), "progress").unwrap();
        fs::write(done.join("processor.lock"), "lock").unwrap();
        fs::write(done.join("run.toml"), "destination = \"x\"\n").unwrap();
        fs::write(done.join("manifest.toml"), "entries = []\n").unwrap();
        fs::write(
            done.join("status.toml"),
            status(&nickname, &run_id, RunState::Done)
                .to_toml()
                .unwrap(),
        )
        .unwrap();

        run_gc(&cfg).unwrap();

        assert!(done.exists(), "fresh done directory must remain");
        assert!(
            done.join("status.toml").exists(),
            "status.toml must remain observable"
        );
        assert!(
            done.join("run.toml").exists(),
            "run.toml must remain observable"
        );
        assert!(
            done.join("manifest.toml").exists(),
            "manifest.toml must remain observable"
        );
        assert!(
            !done.join("files").exists(),
            "staged payload must be pruned"
        );
        assert!(!done.join("work").exists(), "work area must be pruned");
        assert!(!done.join("lease.toml").exists(), "lease must be pruned");
        assert!(
            !done.join("progress.toml").exists(),
            "progress must be pruned"
        );
        assert!(
            !done.join("processor.lock").exists(),
            "processor.lock must be pruned"
        );
    }

    #[test]
    fn done_not_in_done_state_is_still_expired_by_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        cfg.gc.done_retention_secs = 0;
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("done-failed".into()).unwrap();
        let done = cfg.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        fs::create_dir_all(done.join("files")).unwrap();
        fs::write(
            done.join("status.toml"),
            status(&nickname, &run_id, RunState::Failed)
                .to_toml()
                .unwrap(),
        )
        .unwrap();

        run_gc(&cfg).unwrap();

        assert!(
            !done.exists(),
            "done dir with Failed state must still expire by mtime"
        );
    }

    #[test]
    #[cfg(unix)]
    fn finalization_succeeds_even_when_pruning_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(Utf8PathBuf::from_path_buf(tmp.path().join("work")).unwrap());
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("prune-fail".into()).unwrap();
        let processing = cfg
            .work_dir
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.join("files/sub")).unwrap();
        fs::create_dir_all(processing.join("work")).unwrap();
        fs::write(processing.join("files/sub/x.txt"), "payload").unwrap();
        fs::write(
            processing.join("status.toml"),
            status(&nickname, &run_id, RunState::Done)
                .to_toml()
                .unwrap(),
        )
        .unwrap();
        fs::write(
            processing.join("run.toml"),
            r#"purgery_version = "0.1.0-test"
nickname = "laptop"
destination = "/tmp/dest"
delete_after_import = true
"#,
        )
        .unwrap();
        fs::write(processing.join("manifest.toml"), "entries = []\n").unwrap();
        fs::write(processing.join("lease.toml"), "lease").unwrap();
        fs::write(processing.join("progress.toml"), "progress").unwrap();
        fs::write(processing.join("processor.lock"), "lock").unwrap();

        std::fs::set_permissions(
            processing.join("files").as_std_path(),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let result =
            crate::phases::finalize_processing_run(&cfg, &nickname, &run_id, &RunState::Done);
        assert!(
            result.is_ok(),
            "finalize_processing_run must return Ok even when pruning fails: {result:?}"
        );

        let done = cfg.work_dir.run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done.exists(), "done dir must exist after finalization");
        assert!(
            done.join("status.toml").exists(),
            "status.toml must be present in done dir"
        );
        let status_content = fs::read_to_string(done.join("status.toml")).unwrap();
        let s = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(s.state, RunState::Done, "run must remain terminal Done");

        // Restore permissions so tempdir cleanup can proceed
        let _ = std::fs::set_permissions(
            done.join("files").as_std_path(),
            std::fs::Permissions::from_mode(0o755),
        );
    }
}
