use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{CleanupEntry, DurableCleanupState, FileStatus, ManifestEntryKind, RunStatus};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;
use tracing::{info, warn};

/// Outcome of processing a single cleanup state file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupOutcome {
    /// Every entry has been cleaned — the state file can be retired.
    Complete,
    /// At least one entry remains unconfirmed or has not been cleaned.
    StillPending,
}

pub(crate) fn state_dir_path(state_dir: &str) -> Result<Utf8PathBuf> {
    let path = Utf8PathBuf::from(state_dir);
    fs::create_dir_all(path.as_std_path())
        .with_context(|| format!("failed to create state dir: {path}"))?;
    Ok(path)
}

pub(crate) fn write_cleanup_state(
    state: &DurableCleanupState,
    state_dir: &str,
) -> Result<Utf8PathBuf> {
    let dir = state_dir_path(state_dir)?;
    let filename = format!("cleanup-{}-{}.toml", state.nickname, state.operation_id);
    let final_path = dir.join(&filename);
    let tmp_path = dir.join(format!("{filename}.tmp"));
    let content = toml::to_string(state)
        .map_err(|e| anyhow::anyhow!("failed to serialize cleanup state: {e}"))?;
    fs::write(&tmp_path, &content)
        .with_context(|| format!("failed to write cleanup state: {tmp_path}"))?;
    fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("failed to atomically publish cleanup state: {final_path}"))?;
    Ok(final_path)
}

pub(crate) fn confirm_all_imports(state_path: &Utf8Path) -> Result<()> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
    // Probe raw TOML for version to distinguish old/incompatible state
    // from malformed current state before full deserialization.
    purgery_core::require_compatible_toml_version(
        &content,
        format_args!("cleanup state {state_path}"),
    )
    .map_err(|e| anyhow::anyhow!("incompatible cleanup state version: {e}"))?;
    let mut state: DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;
    for entry in &mut state.entries {
        if !entry.import_confirmed {
            entry.import_confirmed = true;
        }
    }
    let tmp_path = state_path.with_extension("toml.tmp");
    let new_content = toml::to_string(&state)
        .map_err(|e| anyhow::anyhow!("failed to serialize cleanup state: {e}"))?;
    fs::write(&tmp_path, &new_content)
        .with_context(|| format!("failed to write cleanup state: {tmp_path}"))?;
    fs::rename(&tmp_path, state_path)
        .with_context(|| format!("failed to atomically update cleanup state: {state_path}"))?;
    Ok(())
}

pub(crate) fn confirm_imports_from_status(state_path: &Utf8Path, status: &RunStatus) -> Result<()> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
    // Probe raw TOML for version to distinguish old/incompatible state
    // from malformed current state before full deserialization.
    purgery_core::require_compatible_toml_version(
        &content,
        format_args!("cleanup state {state_path}"),
    )
    .map_err(|e| anyhow::anyhow!("incompatible cleanup state version: {e}"))?;
    let mut state: DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;

    // First pass: match cleanup entries to status entries by exact path/kind.
    for cleanup_entry in &mut state.entries {
        cleanup_entry.import_confirmed = status.entries.iter().any(|status_entry| {
            status_entry.status == FileStatus::Imported
                && status_entry.local_path == cleanup_entry.local_path
                && status_entry.relative_path == cleanup_entry.relative_path
                && status_entry.kind == cleanup_entry.kind
        });
    }

    // Second pass: propagate imported directory confirmation to
    // descendants.  Only propagate from an imported directory when the
    // cleanup state itself has a directory entry that was confirmed in the
    // first pass — this ensures the status entry matches an actual cleanup
    // directory entry by kind, local path, and relative path.
    let confirmed_dirs: Vec<String> = state
        .entries
        .iter()
        .filter(|e| e.import_confirmed && e.kind == ManifestEntryKind::Directory)
        .map(|e| e.relative_path.clone())
        .collect();

    for cleanup_entry in &mut state.entries {
        if cleanup_entry.import_confirmed {
            continue;
        }
        for dir_rel_path in &confirmed_dirs {
            if cleanup_entry
                .relative_path
                .starts_with(&format!("{}/", dir_rel_path))
            {
                cleanup_entry.import_confirmed = true;
                break;
            }
        }
    }

    let tmp_path = state_path.with_extension("toml.tmp");
    let new_content = toml::to_string(&state)
        .map_err(|e| anyhow::anyhow!("failed to serialize cleanup state: {e}"))?;
    fs::write(&tmp_path, &new_content)
        .with_context(|| format!("failed to write cleanup state: {tmp_path}"))?;
    fs::rename(&tmp_path, state_path)
        .with_context(|| format!("failed to atomically update cleanup state: {state_path}"))?;
    Ok(())
}

pub(crate) fn resume_pending_cleanups(state_dir: &str) -> Result<()> {
    let dir = match state_dir_path(state_dir) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    if !dir.exists() {
        return Ok(());
    }
    let mut deleted_total = 0usize;
    for entry in fs::read_dir(dir.as_std_path())
        .with_context(|| format!("failed to read state dir: {dir}"))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("cleanup-"))
        {
            continue;
        }
        let state_path = match Utf8PathBuf::from_path_buf(path) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let content = match fs::read_to_string(state_path.as_std_path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Probe purgery_version from raw TOML before full deserialization
        // so we can distinguish old/incompatible from malformed current state.
        let version = match purgery_core::probe_purgery_version_from_toml(&content) {
            Err(purgery_core::VersionProbeError::MissingVersion) => {
                warn!(path = %state_path, "cleanup state missing purgery_version (too old); skipping");
                continue;
            }
            Err(purgery_core::VersionProbeError::InvalidToml(e)) => {
                warn!(path = %state_path, error = %e, "cleanup state has malformed TOML; skipping");
                continue;
            }
            Ok(v) => v,
        };
        if let Err(e) = purgery_core::require_compatible_purgery_version(&version, "cleanup state")
        {
            warn!(path = %state_path, error = %e, "cleanup state has incompatible purgery_version; skipping");
            continue;
        }
        let state = match toml::from_str::<DurableCleanupState>(&content) {
            Ok(s) => s,
            Err(e) => {
                warn!(path = %state_path, error = %e, "cleanup state has malformed current-version content; skipping");
                continue;
            }
        };
        let before = state.entries.iter().filter(|e| e.cleaned).count();
        let total = state.entries.len();
        match process_cleanup_state_file(&state_path) {
            Err(e) => warn!(path = %state_path, error = %e, "failed to process cleanup state"),
            Ok(CleanupOutcome::Complete) => {
                // Do not retire a completed cleanup file while a client run
                // state still exists for it — the run machine's inline path
                // (process_cleanup_from_status) needs the file to advance
                // TerminalStatusSeen → CleanupComplete.  Only retire
                // orphaned files whose run state is already gone.
                // Run state is persisted at:
                //   state_dir/runs/{nickname}-{run_id}/state.toml
                let run_state_path = dir
                    .join("runs")
                    .join(format!("{}-{}", state.nickname, state.operation_id))
                    .join("state.toml");
                if !run_state_path.exists() {
                    if let Err(e) = retire_cleanup_state_file(&state_path) {
                        warn!(path = %state_path, error = %e, "failed to retire completed cleanup state file");
                    }
                }
                deleted_total += total - before;
            }
            Ok(CleanupOutcome::StillPending) => {
                if let Ok(new_content) = fs::read_to_string(state_path.as_std_path()) {
                    if let Ok(new_state) = toml::from_str::<DurableCleanupState>(&new_content) {
                        if purgery_core::require_compatible_purgery_version(
                            &new_state.purgery_version,
                            "cleanup state",
                        )
                        .is_err()
                        {
                            continue;
                        }
                        let after = new_state.entries.iter().filter(|e| e.cleaned).count();
                        deleted_total += after - before;
                    }
                }
            }
        }
    }
    if deleted_total > 0 {
        info!(deleted = deleted_total, "resumed pending cleanups");
    }
    Ok(())
}

pub(crate) fn compute_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open file for SHA-256: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read file for SHA-256: {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn process_cleanup_state_file(state_path: &Utf8Path) -> Result<CleanupOutcome> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
    // Probe raw TOML for version to distinguish old/incompatible state
    // from malformed current state before full deserialization.
    purgery_core::require_compatible_toml_version(
        &content,
        format_args!("cleanup state {state_path}"),
    )
    .map_err(|e| anyhow::anyhow!("incompatible cleanup state version: {e}"))?;
    let mut state: DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;

    // Phase 1: clean non-directory entries (files and symlinks).
    // Keep the in-memory state mutable so directory checks in phase 2
    // see the updated cleaned flags.
    for i in 0..state.entries.len() {
        if !state.entries[i].import_confirmed || state.entries[i].cleaned {
            continue;
        }
        if state.entries[i].kind == ManifestEntryKind::Directory {
            continue;
        }
        let cleaned = try_clean_entry(&state.entries, i);
        if cleaned {
            state.entries[i].cleaned = true;
            write_cleanup_state_atomic(state_path, &state)?;
        }
    }

    // Phase 2: clean directories bottom-up. Entries are already sorted
    // deepest-first, so iterating forward processes leaf directories
    // before their parents.
    for i in 0..state.entries.len() {
        if !state.entries[i].import_confirmed || state.entries[i].cleaned {
            continue;
        }
        if state.entries[i].kind != ManifestEntryKind::Directory {
            continue;
        }
        if !can_remove_directory(&state.entries, i) {
            continue;
        }
        let local_path = Path::new(&state.entries[i].local_path);
        match fs::remove_dir(local_path) {
            Ok(()) => {
                info!(path = %state.entries[i].local_path, "removed empty directory");
                state.entries[i].cleaned = true;
                write_cleanup_state_atomic(state_path, &state)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                state.entries[i].cleaned = true;
                write_cleanup_state_atomic(state_path, &state)?;
            }
            Err(e) => {
                warn!(path = %state.entries[i].local_path, error = %e, "failed to remove directory");
            }
        }
    }

    let all_cleaned = state.entries.iter().all(|e| e.cleaned);
    if all_cleaned {
        Ok(CleanupOutcome::Complete)
    } else {
        Ok(CleanupOutcome::StillPending)
    }
}

/// Remove a cleanup state file that is no longer needed.
///
/// Used by the transform/server-run flow **after** `CleanupComplete` has been
/// persisted, so a crash before the removal leaves the completed file in
/// place where `resume_pending_cleanups` or the next inline pass can
/// safely retire it.
pub(crate) fn retire_cleanup_state_file(state_path: &Utf8Path) -> Result<()> {
    fs::remove_file(state_path.as_std_path())
        .with_context(|| format!("failed to remove cleanup state file: {state_path}"))?;
    info!(path = %state_path, "retired completed cleanup state file");
    Ok(())
}

/// Process a cleanup state file and retire it when all entries are cleaned.
///
/// Suitable for the direct passthrough path, which has no client run state
/// to protect.  For the transform/server-run path use
/// [`process_cleanup_state_file`] directly and call
/// [`retire_cleanup_state_file`] **after** persisting `CleanupComplete`, so
/// that a crash between the two steps does not orphan a completed cleanup
/// with `TerminalStatusSeen` still on disk.
pub(crate) fn process_cleanup_state_file_and_retire(
    state_path: &Utf8Path,
) -> Result<CleanupOutcome> {
    let outcome = process_cleanup_state_file(state_path)?;
    if matches!(outcome, CleanupOutcome::Complete) {
        retire_cleanup_state_file(state_path)?;
    }
    Ok(outcome)
}

/// Returns true if the entry at index i was successfully cleaned.
fn try_clean_entry(entries: &[CleanupEntry], i: usize) -> bool {
    let entry = &entries[i];
    let local_path = Path::new(&entry.local_path);
    let symmeta = match fs::symlink_metadata(local_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };

    match entry.kind {
        ManifestEntryKind::Directory => false,
        ManifestEntryKind::Symlink => {
            if !symmeta.file_type().is_symlink() {
                return false;
            }
            let Some(ref expected_target) = entry.link_target else {
                return false;
            };
            let Ok(current_target) = fs::read_link(local_path) else {
                return false;
            };
            if current_target.to_string_lossy().as_ref() != expected_target.as_str() {
                return false;
            }
            if let Err(e) = fs::remove_file(local_path) {
                warn!(path = %entry.local_path, error = %e, "failed to unlink symlink");
                return false;
            }
            true
        }
        ManifestEntryKind::RegularFile => {
            if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
                return false;
            }
            let Ok(meta) = fs::metadata(local_path) else {
                return false;
            };
            if meta.len() != entry.size {
                return false;
            }
            let current_mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            if current_mtime != entry.mtime_ns {
                return false;
            }
            let Some(ref expected_sha) = entry.sha256 else {
                return false;
            };
            let Ok(actual_sha) = compute_sha256(local_path) else {
                return false;
            };
            if actual_sha != *expected_sha {
                return false;
            }
            if let Err(e) = fs::remove_file(local_path) {
                warn!(path = %entry.local_path, error = %e, "failed to delete");
                return false;
            }
            true
        }
    }
}

/// Check whether a directory can be safely removed: all children in the
/// cleanup state must be cleaned or unconfirmed, and there must be no
/// unexpected files on disk.
fn can_remove_directory(entries: &[CleanupEntry], dir_idx: usize) -> bool {
    let dir_entry = &entries[dir_idx];
    let local_path = Path::new(&dir_entry.local_path);

    let reader = match fs::read_dir(local_path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };

    for child in reader {
        let child = match child {
            Ok(c) => c,
            Err(_) => return false,
        };
        let child_path = child.path();
        // Is this child tracked in the cleanup state?
        let tracked = entries.iter().any(|e| {
            let e_path = Path::new(&e.local_path);
            e_path == child_path
        });
        if !tracked {
            // Untracked entry on disk — directory is not empty of
            // unknown content; refuse to remove.
            return false;
        }
        // If tracked, it must already be cleaned (removed) or
        // unconfirmed (never imported).
        let child_clear = entries.iter().any(|e| {
            let e_path = Path::new(&e.local_path);
            e_path == child_path && (e.cleaned || !e.import_confirmed)
        });
        if !child_clear {
            return false;
        }
    }

    true
}

fn write_cleanup_state_atomic(state_path: &Utf8Path, state: &DurableCleanupState) -> Result<()> {
    let tmp_path = state_path.with_extension("toml.tmp");
    let new_content = toml::to_string(state)
        .map_err(|e| anyhow::anyhow!("failed to serialize cleanup state: {e}"))?;
    fs::write(&tmp_path, &new_content)
        .with_context(|| format!("failed to write cleanup state: {tmp_path}"))?;
    fs::rename(&tmp_path, state_path)
        .with_context(|| format!("failed to atomically update cleanup state: {state_path}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use purgery_core::{EntryStatusEntry, Nickname, RunId, RunState};

    fn cleanup_entry(path: &Path) -> CleanupEntry {
        let metadata = fs::metadata(path).unwrap();
        let mtime_ns = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        CleanupEntry {
            relative_path: "a.txt".to_owned(),
            local_path: path.to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::RegularFile,
            size: metadata.len(),
            mtime_ns,
            sha256: Some(compute_sha256(path).unwrap()),
            link_target: None,
            import_confirmed: false,
            cleaned: false,
        }
    }

    fn status_for(path: &Path, file_status: FileStatus) -> RunStatus {
        RunStatus {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("run-1".to_owned()).unwrap(),
            nickname: Nickname::new("host".to_owned()).unwrap(),
            state: RunState::Done,
            entries: vec![EntryStatusEntry {
                kind: ManifestEntryKind::RegularFile,
                local_path: path.to_str().unwrap().to_owned(),
                relative_path: "a.txt".to_owned(),
                status: file_status,
                final_paths: vec!["/destination/a.txt".to_owned()],
                transform: Some("transform".to_owned()),
                error: None,
            }],
            error: None,
        }
    }

    #[test]
    fn transform_cleanup_waits_for_imported_server_status() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        fs::write(&file, "original").unwrap();
        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-1".to_owned(),
            entries: vec![cleanup_entry(&file)],
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        process_cleanup_state_file(&state_path).unwrap();
        assert!(
            file.exists(),
            "staging transfer alone must not authorize cleanup"
        );

        confirm_imports_from_status(&state_path, &status_for(&file, FileStatus::Imported)).unwrap();
        process_cleanup_state_file(&state_path).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn cleanup_removes_file_then_parent_directory_in_one_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("subdir");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "hello").unwrap();

        let file_sha = compute_sha256(&file).unwrap();
        let mut entries = vec![dir_entry(&dir), file_entry(&file, &file_sha)];
        for e in &mut entries {
            e.import_confirmed = true;
        }

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-dir".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        process_cleanup_state_file(&state_path).unwrap();

        assert!(!file.exists(), "file should be removed");
        assert!(!dir.exists(), "empty directory should be removed");

        // Re-running should be idempotent
        process_cleanup_state_file(&state_path).unwrap();
    }

    #[test]
    fn cleanup_preserves_directory_with_untracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("subdir");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "hello").unwrap();
        let extra = dir.join("extra.txt");
        fs::write(&extra, "untracked").unwrap();

        let file_sha = compute_sha256(&file).unwrap();
        let mut entries = vec![dir_entry(&dir), file_entry(&file, &file_sha)];
        for e in &mut entries {
            e.import_confirmed = true;
        }

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-extra".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        process_cleanup_state_file(&state_path).unwrap();

        assert!(!file.exists(), "tracked file should be removed");
        assert!(dir.exists(), "directory with untracked file must remain");
        assert!(extra.exists(), "untracked file must remain");
    }

    #[test]
    fn cleanup_preserves_directory_with_changed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("subdir");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "hello").unwrap();

        let file_sha = compute_sha256(&file).unwrap();
        let mut entries = vec![dir_entry(&dir), file_entry(&file, &file_sha)];
        for e in &mut entries {
            e.import_confirmed = true;
        }
        // Change the file after capturing identity
        fs::write(&file, "modified").unwrap();

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-changed".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        process_cleanup_state_file(&state_path).unwrap();

        assert!(file.exists(), "changed file must remain");
        assert!(dir.exists(), "directory with changed file must remain");
    }

    fn dir_entry(path: &Path) -> CleanupEntry {
        CleanupEntry {
            relative_path: path.file_name().unwrap().to_str().unwrap().to_owned(),
            local_path: path.to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::Directory,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            import_confirmed: false,
            cleaned: false,
        }
    }

    fn file_entry(path: &Path, sha: &str) -> CleanupEntry {
        let metadata = fs::metadata(path).unwrap();
        let mtime_ns = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        CleanupEntry {
            relative_path: path.file_name().unwrap().to_str().unwrap().to_owned(),
            local_path: path.to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::RegularFile,
            size: metadata.len(),
            mtime_ns,
            sha256: Some(sha.to_owned()),
            link_target: None,
            import_confirmed: false,
            cleaned: false,
        }
    }

    #[test]
    fn cleanup_nested_directories_removed_in_one_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let a_dir = tmp.path().join("a");
        let b_dir = a_dir.join("b");
        fs::create_dir_all(&b_dir).unwrap();
        let file = b_dir.join("file.txt");
        fs::write(&file, "content").unwrap();

        let file_sha = compute_sha256(&file).unwrap();
        let mut entries: Vec<CleanupEntry> = Vec::new();
        entries.push(file_entry(&file, &file_sha));
        entries.push(CleanupEntry {
            relative_path: "a/b".to_owned(),
            local_path: b_dir.to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::Directory,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            import_confirmed: false,
            cleaned: false,
        });
        entries.push(CleanupEntry {
            relative_path: "a".to_owned(),
            local_path: a_dir.to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::Directory,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            import_confirmed: false,
            cleaned: false,
        });
        for e in &mut entries {
            e.import_confirmed = true;
        }

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-nested".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        process_cleanup_state_file(&state_path).unwrap();

        assert!(!file.exists(), "file should be removed");
        assert!(!b_dir.exists(), "b dir should be removed");
        assert!(!a_dir.exists(), "a dir should be removed");

        // Idempotent
        process_cleanup_state_file(&state_path).unwrap();
    }

    #[test]
    fn transform_cleanup_preserves_failed_and_skipped_entries() {
        for status in [FileStatus::Failed, FileStatus::Skipped] {
            let tmp = tempfile::tempdir().unwrap();
            let file = tmp.path().join("a.txt");
            fs::write(&file, "original").unwrap();
            let state = DurableCleanupState {
                purgery_version: "0.1.0-test".to_string(),
                nickname: "host".to_owned(),
                operation_id: format!("run-{}", status.as_str()),
                entries: vec![cleanup_entry(&file)],
            };
            let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

            confirm_imports_from_status(&state_path, &status_for(&file, status)).unwrap();
            process_cleanup_state_file(&state_path).unwrap();
            assert!(file.exists());
        }
    }

    #[test]
    fn cleanup_confirms_directory_descendants_when_imported() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("photos");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("photo1.jpg");
        fs::write(&file, "image data").unwrap();

        let file_sha = compute_sha256(&file).unwrap();
        let file_meta = fs::metadata(&file).unwrap();
        let file_mtime = file_meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;

        let entries = vec![
            CleanupEntry {
                relative_path: "photos/photo1.jpg".to_owned(),
                local_path: file.to_str().unwrap().to_owned(),
                kind: ManifestEntryKind::RegularFile,
                size: file_meta.len(),
                mtime_ns: file_mtime,
                sha256: Some(file_sha),
                link_target: None,
                import_confirmed: false,
                cleaned: false,
            },
            CleanupEntry {
                relative_path: "photos".to_owned(),
                local_path: dir.to_str().unwrap().to_owned(),
                kind: ManifestEntryKind::Directory,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                import_confirmed: false,
                cleaned: false,
            },
        ];

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-dir".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        // Status only mentions the directory as imported — no individual descendant entry.
        let status = RunStatus {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("run-dir".to_owned()).unwrap(),
            nickname: Nickname::new("host".to_owned()).unwrap(),
            state: RunState::Done,
            entries: vec![EntryStatusEntry {
                kind: ManifestEntryKind::Directory,
                local_path: dir.to_str().unwrap().to_owned(),
                relative_path: "photos".to_owned(),
                status: FileStatus::Imported,
                final_paths: vec!["/dest/photos".to_owned()],
                transform: Some("transform".to_owned()),
                error: None,
            }],
            error: None,
        };

        confirm_imports_from_status(&state_path, &status).unwrap();

        let content = fs::read_to_string(state_path.as_std_path()).unwrap();
        let state: DurableCleanupState = toml::from_str(&content).unwrap();

        let dir_confirmed = state
            .entries
            .iter()
            .any(|e| e.kind == ManifestEntryKind::Directory && e.import_confirmed);
        let file_confirmed = state
            .entries
            .iter()
            .any(|e| e.kind == ManifestEntryKind::RegularFile && e.import_confirmed);

        assert!(dir_confirmed, "parent directory must be confirmed");
        assert!(
            file_confirmed,
            "directory descendant must be confirmed via parent directory"
        );
    }

    #[test]
    fn cleanup_does_not_confirm_directory_descendants_when_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("photos");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("photo1.jpg");
        fs::write(&file, "image data").unwrap();

        let entries = vec![
            CleanupEntry {
                relative_path: "photos/photo1.jpg".to_owned(),
                local_path: file.to_str().unwrap().to_owned(),
                kind: ManifestEntryKind::RegularFile,
                size: fs::metadata(&file).unwrap().len(),
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                import_confirmed: false,
                cleaned: false,
            },
            CleanupEntry {
                relative_path: "photos".to_owned(),
                local_path: dir.to_str().unwrap().to_owned(),
                kind: ManifestEntryKind::Directory,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                import_confirmed: false,
                cleaned: false,
            },
        ];

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-failed".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        let status = RunStatus {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("run-failed".to_owned()).unwrap(),
            nickname: Nickname::new("host".to_owned()).unwrap(),
            state: RunState::Done,
            entries: vec![EntryStatusEntry {
                kind: ManifestEntryKind::Directory,
                local_path: dir.to_str().unwrap().to_owned(),
                relative_path: "photos".to_owned(),
                status: FileStatus::Failed,
                final_paths: vec![],
                transform: None,
                error: Some("something went wrong".to_owned()),
            }],
            error: None,
        };

        confirm_imports_from_status(&state_path, &status).unwrap();

        let content = fs::read_to_string(state_path.as_std_path()).unwrap();
        let state: DurableCleanupState = toml::from_str(&content).unwrap();

        assert!(
            state.entries.iter().all(|e| !e.import_confirmed),
            "no entries should be confirmed when the parent directory failed"
        );
    }

    #[test]
    fn cleanup_does_not_confirm_directory_descendants_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("photos");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("photo1.jpg");
        fs::write(&file, "image data").unwrap();

        let entries = vec![
            CleanupEntry {
                relative_path: "photos/photo1.jpg".to_owned(),
                local_path: file.to_str().unwrap().to_owned(),
                kind: ManifestEntryKind::RegularFile,
                size: fs::metadata(&file).unwrap().len(),
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                import_confirmed: false,
                cleaned: false,
            },
            CleanupEntry {
                relative_path: "photos".to_owned(),
                local_path: dir.to_str().unwrap().to_owned(),
                kind: ManifestEntryKind::Directory,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                import_confirmed: false,
                cleaned: false,
            },
        ];

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-missing".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        // Status has no entry for the parent directory.
        let status = RunStatus {
            purgery_version: "0.1.0-test".to_string(),
            run_id: RunId::new("run-missing".to_owned()).unwrap(),
            nickname: Nickname::new("host".to_owned()).unwrap(),
            state: RunState::Done,
            entries: vec![EntryStatusEntry {
                kind: ManifestEntryKind::RegularFile,
                local_path: "/unrelated".to_owned(),
                relative_path: "unrelated.txt".to_owned(),
                status: FileStatus::Imported,
                final_paths: vec!["/dest/unrelated.txt".to_owned()],
                transform: None,
                error: None,
            }],
            error: None,
        };

        confirm_imports_from_status(&state_path, &status).unwrap();

        let content = fs::read_to_string(state_path.as_std_path()).unwrap();
        let state: DurableCleanupState = toml::from_str(&content).unwrap();

        assert!(
            state.entries.iter().all(|e| !e.import_confirmed),
            "no entries should be confirmed when the parent directory is absent from status"
        );
    }

    #[test]
    fn directory_cleanup_deletes_unchanged_descendants_then_source_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let source_name = "Videos";
        let dir = tmp.path().join(source_name);
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let file1 = dir.join("a.mp4");
        fs::write(&file1, "data").unwrap();
        let file2 = sub.join("b.mp4");
        fs::write(&file2, "data2").unwrap();

        let file1_sha = compute_sha256(&file1).unwrap();
        let file2_sha = compute_sha256(&file2).unwrap();

        let mut entries: Vec<CleanupEntry> = Vec::new();

        // File descendant: Videos/a.mp4
        let m1 = fs::metadata(&file1).unwrap();
        entries.push(CleanupEntry {
            relative_path: format!("{source_name}/a.mp4"),
            local_path: file1.to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::RegularFile,
            size: m1.len(),
            mtime_ns: m1
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64,
            sha256: Some(file1_sha),
            link_target: None,
            import_confirmed: true,
            cleaned: false,
        });

        // File descendant: Videos/sub/b.mp4
        let m2 = fs::metadata(&file2).unwrap();
        entries.push(CleanupEntry {
            relative_path: format!("{source_name}/sub/b.mp4"),
            local_path: file2.to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::RegularFile,
            size: m2.len(),
            mtime_ns: m2
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64,
            sha256: Some(file2_sha),
            link_target: None,
            import_confirmed: true,
            cleaned: false,
        });

        // Directory descendant: Videos/sub
        entries.push(CleanupEntry {
            relative_path: format!("{source_name}/sub"),
            local_path: sub.to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::Directory,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            import_confirmed: true,
            cleaned: false,
        });

        // Top-level source directory: Videos
        entries.push(CleanupEntry {
            relative_path: source_name.to_owned(),
            local_path: dir.to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::Directory,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            import_confirmed: true,
            cleaned: false,
        });

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-dir-cleanup".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        process_cleanup_state_file(&state_path).unwrap();

        assert!(
            !file1.exists(),
            "unchanged descendant file should be deleted"
        );
        assert!(
            !file2.exists(),
            "unchanged nested descendant file should be deleted"
        );
        assert!(
            !sub.exists(),
            "empty descendant directory should be removed"
        );
        assert!(
            !dir.exists(),
            "empty top-level source directory should be removed"
        );
    }

    #[test]
    fn directory_cleanup_preserves_changed_descendant() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("a.mp4");
        fs::write(&file, "original").unwrap();

        let file_sha = compute_sha256(&file).unwrap();
        let meta = fs::metadata(&file).unwrap();

        let entries = vec![
            CleanupEntry {
                relative_path: "Videos/a.mp4".to_owned(),
                local_path: file.to_str().unwrap().to_owned(),
                kind: ManifestEntryKind::RegularFile,
                size: meta.len(),
                mtime_ns: meta
                    .modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as i64,
                sha256: Some(file_sha),
                link_target: None,
                import_confirmed: true,
                cleaned: false,
            },
            CleanupEntry {
                relative_path: "Videos".to_owned(),
                local_path: dir.to_str().unwrap().to_owned(),
                kind: ManifestEntryKind::Directory,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                import_confirmed: true,
                cleaned: false,
            },
        ];

        // Change the file after capturing identity
        fs::write(&file, "modified").unwrap();

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-changed-dir".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        process_cleanup_state_file(&state_path).unwrap();

        assert!(file.exists(), "changed descendant file must remain");
        assert!(dir.exists(), "directory with changed content must remain");
    }

    #[test]
    fn cleanup_outcome_complete_retires_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        fs::write(&file, "hello").unwrap();

        let mut entry = cleanup_entry(&file);
        entry.import_confirmed = true;
        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-complete".to_owned(),
            entries: vec![entry],
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        let outcome = process_cleanup_state_file_and_retire(&state_path).unwrap();
        assert_eq!(outcome, CleanupOutcome::Complete);
        assert!(!state_path.exists(), "completed state file must be retired");
    }

    #[test]
    fn cleanup_outcome_stale_complete_retires_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        fs::write(&file, "hello").unwrap();

        let mut entry = cleanup_entry(&file);
        entry.import_confirmed = true;
        entry.cleaned = true;
        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-stale".to_owned(),
            entries: vec![entry],
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        let outcome = process_cleanup_state_file_and_retire(&state_path).unwrap();
        assert_eq!(outcome, CleanupOutcome::Complete);
        assert!(
            !state_path.exists(),
            "stale completed state file must be retired"
        );
    }

    #[test]
    fn cleanup_outcome_partial_pending_keeps_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file_a = tmp.path().join("a.txt");
        fs::write(&file_a, "hello").unwrap();
        let file_b = tmp.path().join("b.txt");
        fs::write(&file_b, "world").unwrap();

        let mut entry_a = cleanup_entry(&file_a);
        entry_a.import_confirmed = true;
        let mut entry_b = cleanup_entry(&file_b);
        entry_b.import_confirmed = true;

        // Change file_b after capture so it won't be cleaned.
        fs::write(&file_b, "modified").unwrap();

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-partial".to_owned(),
            entries: vec![entry_a, entry_b],
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        let outcome = process_cleanup_state_file_and_retire(&state_path).unwrap();
        assert_eq!(outcome, CleanupOutcome::StillPending);
        assert!(state_path.exists(), "partial state file must remain");

        // file_a should have been cleaned, file_b preserved.
        assert!(!file_a.exists(), "unchanged file must be deleted");
        assert!(file_b.exists(), "changed file must be preserved");
    }

    #[test]
    fn cleanup_outcome_unconfirmed_no_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        fs::write(&file, "secret").unwrap();

        let mut entry = cleanup_entry(&file);
        entry.import_confirmed = false;
        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-unconfirmed".to_owned(),
            entries: vec![entry],
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        let outcome = process_cleanup_state_file_and_retire(&state_path).unwrap();
        assert_eq!(outcome, CleanupOutcome::StillPending);
        assert!(state_path.exists(), "unconfirmed state file must remain");
        assert!(file.exists(), "unconfirmed file must not be deleted");
    }

    #[test]
    fn cleanup_outcome_missing_confirmed_dir_cleaned() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ghost");
        let dir_str = dir.to_str().unwrap().to_owned();

        let entry = CleanupEntry {
            relative_path: "ghost".to_owned(),
            local_path: dir_str,
            kind: ManifestEntryKind::Directory,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            import_confirmed: true,
            cleaned: false,
        };
        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-ghost".to_owned(),
            entries: vec![entry],
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        let outcome = process_cleanup_state_file(&state_path).unwrap();
        assert_eq!(
            outcome,
            CleanupOutcome::Complete,
            "missing confirmed dir should be treated as cleaned"
        );

        let content = fs::read_to_string(state_path.as_std_path()).unwrap();
        let saved: DurableCleanupState = toml::from_str(&content).unwrap();
        assert!(saved.entries[0].cleaned, "entry must be marked cleaned");
    }

    #[test]
    fn cleanup_outcome_directory_with_untracked_content_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("subdir");
        fs::create_dir(&dir).unwrap();
        let tracked = dir.join("a.txt");
        fs::write(&tracked, "tracked").unwrap();
        let untracked = dir.join("untracked.txt");
        fs::write(&untracked, "untracked").unwrap();

        let file_sha = compute_sha256(&tracked).unwrap();
        let mut entries = vec![dir_entry(&dir), file_entry(&tracked, &file_sha)];
        for e in &mut entries {
            e.import_confirmed = true;
        }

        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-untracked".to_owned(),
            entries,
        };
        let state_path = write_cleanup_state(&state, tmp.path().to_str().unwrap()).unwrap();

        let outcome = process_cleanup_state_file_and_retire(&state_path).unwrap();
        assert_eq!(outcome, CleanupOutcome::StillPending);
        assert!(
            state_path.exists(),
            "state file with untracked content must remain"
        );

        assert!(!tracked.exists(), "tracked file should be deleted");
        assert!(untracked.exists(), "untracked file must remain");
        assert!(dir.exists(), "directory with untracked content must remain");
    }

    #[test]
    fn resume_pending_cleanups_removes_stale_completed_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_str().unwrap();

        let entry = CleanupEntry {
            relative_path: "stale.txt".to_owned(),
            local_path: tmp.path().join("stale.txt").to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::RegularFile,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            import_confirmed: true,
            cleaned: true,
        };
        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "host".to_owned(),
            operation_id: "run-resume-stale".to_owned(),
            entries: vec![entry],
        };
        let state_path = write_cleanup_state(&state, state_dir).unwrap();
        assert!(
            state_path.exists(),
            "stale state file must exist before resume"
        );

        resume_pending_cleanups(state_dir).unwrap();

        assert!(
            !state_path.exists(),
            "stale completed state file must be removed by resume"
        );
    }

    #[test]
    fn resume_pending_cleanups_preserves_file_when_run_state_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_str().unwrap();

        let entry = CleanupEntry {
            relative_path: "done.txt".to_owned(),
            local_path: tmp.path().join("done.txt").to_str().unwrap().to_owned(),
            kind: ManifestEntryKind::RegularFile,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            import_confirmed: true,
            cleaned: true,
        };
        let state = DurableCleanupState {
            purgery_version: "0.1.0-test".to_string(),
            nickname: "myhost".to_owned(),
            operation_id: "run-active".to_owned(),
            entries: vec![entry],
        };
        let state_path = write_cleanup_state(&state, state_dir).unwrap();

        // Create a corresponding client run state file so resume
        // knows this cleanup is not orphaned.
        // Run state lives at: state_dir/runs/{nickname}-{run_id}/state.toml
        let run_dir = tmp.path().join("runs").join("myhost-run-active");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("state.toml"),
            "phase = \"TerminalStatusSeen\"\n",
        )
        .unwrap();

        resume_pending_cleanups(state_dir).unwrap();

        assert!(
            state_path.exists(),
            "completed cleanup file must survive resume when run state is present"
        );
    }
}
