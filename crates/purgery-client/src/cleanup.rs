use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    CleanupEntry, DurableCleanupState, FileStatus, Manifest, ManifestEntryKind, ManifestEntryMode,
    RunStatus,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;
use tracing::{info, warn};

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

pub(crate) fn mark_cleaned(state_path: &Utf8Path, local_path: &str) -> Result<()> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
    let mut state: DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;
    for entry in &mut state.entries {
        if entry.local_path == local_path {
            entry.cleaned = true;
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

pub(crate) fn confirm_all_imports(state_path: &Utf8Path) -> Result<()> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
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
    let mut state: DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;

    for cleanup_entry in &mut state.entries {
        cleanup_entry.import_confirmed = status.entries.iter().any(|status_entry| {
            status_entry.status == FileStatus::Imported
                && status_entry.local_path == cleanup_entry.local_path
                && status_entry.relative_path == cleanup_entry.relative_path
                && status_entry.kind == cleanup_entry.kind
        });
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
        if let Ok(content) = fs::read_to_string(state_path.as_std_path()) {
            if let Ok(state) = toml::from_str::<DurableCleanupState>(&content) {
                let before = state.entries.iter().filter(|e| e.cleaned).count();
                if let Err(e) = process_cleanup_state_file(&state_path) {
                    warn!(path = %state_path, error = %e, "failed to process cleanup state");
                } else if let Ok(new_content) = fs::read_to_string(state_path.as_std_path()) {
                    if let Ok(new_state) = toml::from_str::<DurableCleanupState>(&new_content) {
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

pub(crate) fn build_cleanup_entries(
    source: &str,
    manifest: &Manifest,
) -> Result<Vec<CleanupEntry>> {
    let source_path = Path::new(source);
    let is_file_source = source_path.is_file();
    let walk_root = if is_file_source {
        source_path.parent().unwrap_or(source_path)
    } else {
        source_path
    };

    let mut entries: Vec<CleanupEntry> = Vec::new();
    let mut dirs: Vec<(String, String)> = Vec::new();

    for entry in manifest.entries.iter() {
        let local_path = entry.local_path.as_str();
        let path = Path::new(local_path);

        if entry.mode == ManifestEntryMode::Covered {
            continue;
        }

        let relative_from_root = path
            .strip_prefix(walk_root)
            .ok()
            .and_then(|r| r.to_str())
            .unwrap_or(entry.relative_path.as_str())
            .to_owned();

        match entry.kind {
            ManifestEntryKind::Directory => {
                dirs.push((relative_from_root, local_path.to_owned()));
            }
            ManifestEntryKind::Symlink => {
                let link_target = entry.link_target.as_ref().map(|t| t.as_str().to_owned());
                entries.push(CleanupEntry {
                    relative_path: relative_from_root,
                    local_path: local_path.to_owned(),
                    kind: ManifestEntryKind::Symlink,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target,
                    import_confirmed: false,
                    cleaned: false,
                });
            }
            ManifestEntryKind::RegularFile => {
                let ident = entry.identity();
                entries.push(CleanupEntry {
                    relative_path: relative_from_root,
                    local_path: local_path.to_owned(),
                    kind: ManifestEntryKind::RegularFile,
                    size: ident.size,
                    mtime_ns: ident.mtime_ns,
                    sha256: ident.sha256,
                    link_target: None,
                    import_confirmed: false,
                    cleaned: false,
                });
            }
        }
    }

    for (relative_str, local_path_str) in dirs.into_iter().rev() {
        entries.push(CleanupEntry {
            relative_path: relative_str,
            local_path: local_path_str,
            kind: ManifestEntryKind::Directory,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            import_confirmed: false,
            cleaned: false,
        });
    }

    entries.sort_by(|a, b| {
        let a_depth = a.local_path.matches('/').count();
        let b_depth = b.local_path.matches('/').count();
        b_depth.cmp(&a_depth)
    });

    Ok(entries)
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

pub(crate) fn process_cleanup_state_file(state_path: &Utf8Path) -> Result<()> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
    let state: DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;

    for entry in &state.entries {
        if !entry.import_confirmed || entry.cleaned {
            continue;
        }
        let local_path = Path::new(&entry.local_path);
        let symmeta = match fs::symlink_metadata(local_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = mark_cleaned(state_path, &entry.local_path);
                continue;
            }
            Err(_) => continue,
        };

        match entry.kind {
            ManifestEntryKind::Directory => {
                if !symmeta.file_type().is_dir() {
                    continue;
                }
                let has_unexpected = match fs::read_dir(local_path) {
                    Ok(reader) => {
                        let mut unexpected = false;
                        for child in reader {
                            let child = match child {
                                Ok(c) => c,
                                Err(_) => {
                                    unexpected = true;
                                    break;
                                }
                            };
                            let child_path = child.path();
                            if child_path == local_path {
                                continue;
                            }
                            if state.entries.iter().any(|e| {
                                let e_path = Path::new(&e.local_path);
                                e_path == child_path && (e.cleaned || !e.import_confirmed)
                            }) {
                                continue;
                            }
                            unexpected = true;
                        }
                        unexpected
                    }
                    Err(_) => true,
                };
                if has_unexpected {
                    continue;
                }
                if let Err(e) = fs::remove_dir(local_path) {
                    warn!(path = %entry.local_path, error = %e, "failed to remove directory");
                } else {
                    let _ = mark_cleaned(state_path, &entry.local_path);
                }
            }
            ManifestEntryKind::Symlink => {
                if !symmeta.file_type().is_symlink() {
                    continue;
                }
                let Some(ref expected_target) = entry.link_target else {
                    continue;
                };
                let Ok(current_target) = fs::read_link(local_path) else {
                    continue;
                };
                if current_target.to_string_lossy().as_ref() != expected_target.as_str() {
                    continue;
                }
                if let Err(e) = fs::remove_file(local_path) {
                    warn!(path = %entry.local_path, error = %e, "failed to unlink symlink");
                } else {
                    let _ = mark_cleaned(state_path, &entry.local_path);
                }
            }
            ManifestEntryKind::RegularFile => {
                if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
                    continue;
                }
                let Ok(meta) = fs::metadata(local_path) else {
                    continue;
                };
                if meta.len() != entry.size {
                    continue;
                }
                let current_mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0);
                if current_mtime != entry.mtime_ns {
                    continue;
                }
                let Some(ref expected_sha) = entry.sha256 else {
                    continue;
                };
                let Ok(actual_sha) = compute_sha256(local_path) else {
                    continue;
                };
                if actual_sha != *expected_sha {
                    continue;
                }
                if let Err(e) = fs::remove_file(local_path) {
                    warn!(path = %entry.local_path, error = %e, "failed to delete");
                } else {
                    let _ = mark_cleaned(state_path, &entry.local_path);
                }
            }
        }
    }
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
            run_id: RunId::new("run-1".to_owned()).unwrap(),
            nickname: Nickname::new("host".to_owned()).unwrap(),
            state: RunState::Done,
            entries: vec![EntryStatusEntry {
                kind: ManifestEntryKind::RegularFile,
                local_path: path.to_str().unwrap().to_owned(),
                relative_path: "a.txt".to_owned(),
                status: file_status,
                final_paths: vec!["/destination/a.txt".to_owned()],
                postprocess: Some(vec!["transform".to_owned()]),
                error: None,
            }],
            error: None,
        }
    }

    #[test]
    fn postprocess_cleanup_waits_for_imported_server_status() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        fs::write(&file, "original").unwrap();
        let state = DurableCleanupState {
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
    fn postprocess_cleanup_preserves_failed_and_skipped_entries() {
        for status in [FileStatus::Failed, FileStatus::Skipped] {
            let tmp = tempfile::tempdir().unwrap();
            let file = tmp.path().join("a.txt");
            fs::write(&file, "original").unwrap();
            let state = DurableCleanupState {
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
}
