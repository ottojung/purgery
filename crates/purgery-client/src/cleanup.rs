use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    CleanupEntry, DurableCleanupState, Manifest, ManifestEntryKind, ManifestEntryMode,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;
use tracing::{info, warn};
use walkdir::WalkDir;

pub(crate) fn cleanup_state_dir(state_dir_override: Option<&str>) -> Result<Utf8PathBuf> {
    let path = match state_dir_override {
        Some(d) => Utf8PathBuf::from(d),
        None => {
            if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
                if !dir.is_empty() {
                    Utf8PathBuf::from(dir).join("purgery")
                } else {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
                    Utf8PathBuf::from(format!("{home}/.local/state/purgery"))
                }
            } else {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
                Utf8PathBuf::from(format!("{home}/.local/state/purgery"))
            }
        }
    };
    fs::create_dir_all(path.as_std_path())
        .with_context(|| format!("failed to create state dir: {path}"))?;
    Ok(path)
}

pub(crate) fn write_cleanup_state(
    state: &DurableCleanupState,
    state_dir_override: Option<&str>,
) -> Result<Utf8PathBuf> {
    let dir = cleanup_state_dir(state_dir_override)?;
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

pub(crate) fn mark_cleaned(state_path: &Utf8Path, sync_name: &str, local_path: &str) -> Result<()> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
    let mut state: DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;
    for entry in &mut state.entries {
        if entry.sync_name == sync_name && entry.local_path == local_path {
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

pub(crate) fn mark_rsync_succeeded(state_path: &Utf8Path, sync_name: &str) -> Result<()> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
    let mut state: DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;
    for entry in &mut state.entries {
        if entry.sync_name == sync_name && !entry.rsync_succeeded {
            entry.rsync_succeeded = true;
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

#[allow(dead_code)]
pub(crate) fn resume_pending_cleanups(config: &purgery_core::ClientConfig) -> Result<()> {
    let dir = match cleanup_state_dir(config.state_dir.as_deref()) {
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
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = toml::from_str::<DurableCleanupState>(&content) else {
            continue;
        };
        let state_path = match camino::Utf8PathBuf::from_path_buf(path) {
            Ok(p) => p,
            Err(_) => continue,
        };
        for entry in &state.entries {
            if !entry.rsync_succeeded || entry.cleaned {
                continue;
            }
            let local_path = Path::new(&entry.local_path);
            let symmeta = match fs::symlink_metadata(local_path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = mark_cleaned(&state_path, &entry.sync_name, &entry.local_path);
                    deleted_total += 1;
                    continue;
                }
                Err(_) => continue,
            };
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
            if let Some(ref expected_sha) = entry.sha256 {
                if let Ok(actual_sha) = compute_sha256(local_path) {
                    if &actual_sha != expected_sha {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            if let Err(e) = fs::remove_file(local_path) {
                warn!(path = %entry.local_path, error = %e, "failed to delete");
            } else {
                let _ = mark_cleaned(&state_path, &entry.sync_name, &entry.local_path);
                deleted_total += 1;
            }
        }
    }
    if deleted_total > 0 {
        info!(deleted = deleted_total, "resumed pending cleanups");
    }
    Ok(())
}

pub(crate) fn build_cleanup_entries_from_manifest(
    _config: &purgery_core::ClientConfig,
    sync_name: &str,
    manifest: &Manifest,
) -> Result<Vec<CleanupEntry>> {
    let entries: Vec<CleanupEntry> = manifest
        .entries
        .iter()
        .filter(|e| e.sync_name.as_str() == sync_name && e.mode == ManifestEntryMode::Passthrough)
        .map(|e| CleanupEntry {
            sync_name: sync_name.to_owned(),
            relative_path: e.relative_path.as_str().to_owned(),
            local_path: e.local_path.as_str().to_owned(),
            kind: e.kind,
            size: e.size,
            mtime_ns: e.mtime_ns,
            sha256: e.sha256.clone(),
            link_target: e.link_target.as_ref().map(|p| p.as_str().to_owned()),
            rsync_succeeded: false,
            cleaned: false,
        })
        .collect();
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

pub(crate) fn build_pre_rsync_cleanup_entries(
    _config: &purgery_core::ClientConfig,
    sync: &purgery_core::SyncMapping,
) -> Result<Vec<CleanupEntry>> {
    let from_path = sync.from_path.as_str();
    let from = Path::new(from_path);
    if !from.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    // Collect directories for cleanup order (bottom-up)
    let mut dirs: Vec<(String, String)> = Vec::new();
    for walk_entry in WalkDir::new(from).sort_by_file_name() {
        let walk_entry = match walk_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let local_path = walk_entry.path();
        let metadata = match fs::symlink_metadata(local_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let relative = match local_path.strip_prefix(from) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let relative_str = relative.to_string_lossy().replace('\\', "/");
        let local_path_str = local_path.to_string_lossy().into_owned();

        let file_type = metadata.file_type();
        if file_type.is_dir() {
            dirs.push((relative_str, local_path_str));
        } else if file_type.is_symlink() {
            // Capture symlink identity (literal target, never follow)
            let link_target = fs::read_link(local_path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            entries.push(CleanupEntry {
                sync_name: sync.name.as_str().to_owned(),
                relative_path: relative_str,
                local_path: local_path_str,
                kind: ManifestEntryKind::Symlink,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target,
                rsync_succeeded: false,
                cleaned: false,
            });
        } else if file_type.is_file() {
            let file_meta = match fs::metadata(local_path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size = file_meta.len();
            let mtime_ns = file_meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            let sha256 = compute_sha256(local_path).ok();

            entries.push(CleanupEntry {
                sync_name: sync.name.as_str().to_owned(),
                relative_path: relative_str,
                local_path: local_path_str,
                kind: ManifestEntryKind::RegularFile,
                size,
                mtime_ns,
                sha256,
                link_target: None,
                rsync_succeeded: false,
                cleaned: false,
            });
        }
    }
    // Add directories bottom-up (reverse walk order so children precede parents)
    for (relative_str, local_path_str) in dirs.into_iter().rev() {
        entries.push(CleanupEntry {
            sync_name: sync.name.as_str().to_owned(),
            relative_path: relative_str,
            local_path: local_path_str,
            kind: ManifestEntryKind::Directory,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            rsync_succeeded: false,
            cleaned: false,
        });
    }
    Ok(entries)
}

pub(crate) fn process_cleanup_state_file(state_path: &Utf8Path) -> Result<()> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
    let state: DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;

    // Process entries bottom-up (directories after their children)
    // The entries are already ordered with children before parents from build_pre_rsync_cleanup_entries.
    for entry in &state.entries {
        if !entry.rsync_succeeded || entry.cleaned {
            continue;
        }
        let local_path = Path::new(&entry.local_path);
        let symmeta = match fs::symlink_metadata(local_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = mark_cleaned(state_path, &entry.sync_name, &entry.local_path);
                continue;
            }
            Err(_) => continue,
        };

        match entry.kind {
            ManifestEntryKind::Directory => {
                // Verify the current path is still a directory
                if !symmeta.file_type().is_dir() {
                    continue;
                }
                // Verify no new or unexpected entries exist inside
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
                            // Check if any child path is known in the cleanup state
                            // (has been or will be processed)
                            if child_path == local_path {
                                continue;
                            }
                            // Accept if this child is already cleaned or pending cleanup
                            if state.entries.iter().any(|e| {
                                let e_path = Path::new(&e.local_path);
                                e_path == child_path && (e.cleaned || !e.rsync_succeeded)
                            }) {
                                continue;
                            }
                            // An entry exists that we don't know about
                            unexpected = true;
                        }
                        unexpected
                    }
                    Err(_) => true,
                };
                if has_unexpected {
                    continue;
                }
                // Directory is safe to remove
                if let Err(e) = fs::remove_dir(local_path) {
                    warn!(path = %entry.local_path, error = %e, "failed to remove directory");
                } else {
                    let _ = mark_cleaned(state_path, &entry.sync_name, &entry.local_path);
                }
            }
            ManifestEntryKind::Symlink => {
                // Verify it's still a symlink
                if !symmeta.file_type().is_symlink() {
                    continue;
                }
                // Verify the link target matches
                if let Some(ref expected_target) = entry.link_target {
                    if let Ok(current_target) = fs::read_link(local_path) {
                        let current = current_target.to_string_lossy().into_owned();
                        if current != *expected_target {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                // Unlink the symlink (never follow the target)
                if let Err(e) = fs::remove_file(local_path) {
                    warn!(path = %entry.local_path, error = %e, "failed to unlink symlink");
                } else {
                    let _ = mark_cleaned(state_path, &entry.sync_name, &entry.local_path);
                }
            }
            ManifestEntryKind::RegularFile => {
                // Verify it's still a regular file (not replaced by symlink)
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
                if let Some(ref expected_sha) = entry.sha256 {
                    if let Ok(actual_sha) = compute_sha256(local_path) {
                        if &actual_sha != expected_sha {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                if let Err(e) = fs::remove_file(local_path) {
                    warn!(path = %entry.local_path, error = %e, "failed to delete");
                } else {
                    let _ = mark_cleaned(state_path, &entry.sync_name, &entry.local_path);
                }
            }
        }
    }
    Ok(())
}
