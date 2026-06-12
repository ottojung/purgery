use anyhow::{Context, Result};
use purgery_core::{
    ClientConfig, ClientLocalPath, ManifestEntry, ManifestEntryKind, ManifestEntryMode,
    NormalizedRelativePath, PostprocessRule, RunId,
};
use std::fs;
use std::path::Path;
use std::time::SystemTime;
use tracing::warn;
use walkdir::WalkDir;

use crate::cleanup::compute_sha256;

/// Walk one resolved sync source and return its selected manifest entries.
/// Selection uses the sync match pattern; transformation uses its ordered step names.
pub(crate) fn walk_and_classify_sync(
    _config: &ClientConfig,
    sync: &purgery_core::SyncMapping,
    _run_id: &RunId,
    _applicable_rules: &[&PostprocessRule],
) -> Result<(Vec<ManifestEntry>, bool)> {
    let from_path = sync.from_path.as_str();
    let to_path = sync.to_path.qualified_path();
    let from = Path::new(from_path);

    if !from.exists() {
        warn!(path = from_path, "sync path does not exist, skipping");
        return Ok((Vec::new(), false));
    }

    let mut entries = Vec::new();
    let mut has_postprocess = false;
    let mut walked = Vec::new();

    for walk_entry in WalkDir::new(from).follow_links(false).min_depth(1) {
        let walk_entry = walk_entry.with_context(|| format!("error walking {from_path}"))?;
        let path = walk_entry.path();
        let relative = path
            .strip_prefix(from)
            .with_context(|| format!("failed to compute relative path for: {}", path.display()))?;
        let relative_path =
            camino::Utf8PathBuf::from_path_buf(relative.to_path_buf()).map_err(|path| {
                anyhow::anyhow!("non-UTF-8 relative path is unsupported: {}", path.display())
            })?;
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to read metadata: {}", path.display()))?;
        walked.push((path.to_path_buf(), relative_path, metadata));
    }

    let selected_leaf_paths: Vec<camino::Utf8PathBuf> = walked
        .iter()
        .filter(|(_, _, metadata)| !metadata.file_type().is_dir())
        .filter(|(_, relative, _)| {
            sync.match_pattern
                .as_ref()
                .is_none_or(|pattern| purgery_core::rsync_pattern_match(pattern, relative.as_str()))
        })
        .map(|(_, relative, _)| relative.clone())
        .collect();

    for (path, relative_path, metadata) in walked {
        let file_type = metadata.file_type();
        let selected = if file_type.is_dir() {
            sync.match_pattern.is_none()
                || selected_leaf_paths.iter().any(|selected| {
                    selected.starts_with(&relative_path) && selected != &relative_path
                })
        } else {
            selected_leaf_paths.contains(&relative_path)
        };
        if !selected {
            continue;
        }

        let staged_path = camino::Utf8Path::new("files")
            .join(&to_path)
            .join(&relative_path);
        let postprocess = !sync.postprocess_steps.is_empty()
            && (!file_type.is_dir() || sync.match_pattern.is_none());
        let mode = if postprocess {
            has_postprocess = true;
            ManifestEntryMode::Postprocess
        } else {
            ManifestEntryMode::Passthrough
        };
        let postprocess_steps = if postprocess {
            sync.postprocess_steps.clone()
        } else {
            Vec::new()
        };

        let (kind, size, mtime_ns, sha256, link_target) = if file_type.is_dir() {
            (ManifestEntryKind::Directory, 0, 0, None, None)
        } else if file_type.is_file() {
            let (mtime_ns, sha256) = if postprocess {
                // Postprocess entries require SHA-256 for server-side identity verification
                let mtime_ns = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos() as i64)
                    .unwrap_or(0);
                let sha256 = compute_sha256(&path).with_context(|| {
                    format!(
                        "failed to compute SHA-256 for postprocess entry: {}",
                        path.display()
                    )
                })?;
                (mtime_ns, Some(sha256))
            } else if sync.delete_after_import {
                // Passthrough entries with delete-after-import: SHA needed for cleanup identity
                let mtime_ns = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos() as i64)
                    .unwrap_or(0);
                let sha256 = compute_sha256(&path).with_context(|| {
                    format!(
                        "failed to compute SHA-256 for delete-after-import entry: {}",
                        path.display()
                    )
                })?;
                (mtime_ns, Some(sha256))
            } else {
                (0, None)
            };
            (
                ManifestEntryKind::RegularFile,
                metadata.len(),
                mtime_ns,
                sha256,
                None,
            )
        } else if file_type.is_symlink() {
            let target = fs::read_link(&path)
                .with_context(|| format!("failed to read symlink: {}", path.display()))?;
            let target = camino::Utf8PathBuf::from_path_buf(target).map_err(|path| {
                anyhow::anyhow!(
                    "non-UTF-8 symlink target is unsupported: {}",
                    path.display()
                )
            })?;
            (ManifestEntryKind::Symlink, 0, 0, None, Some(target))
        } else {
            anyhow::bail!("unsupported filesystem object: {}", path.display());
        };

        entries.push(ManifestEntry {
            sync_name: sync.name.clone(),
            local_path: ClientLocalPath::new(path.to_string_lossy().to_string())
                .with_context(|| format!("invalid local path for: {}", path.display()))?,
            staged_path: NormalizedRelativePath::new(staged_path)
                .with_context(|| format!("invalid staged path for: {}", path.display()))?,
            relative_path: NormalizedRelativePath::new(relative_path)
                .with_context(|| format!("invalid relative path for: {}", path.display()))?,
            kind,
            size,
            mtime_ns,
            sha256,
            link_target,
            mode,
            postprocess_steps,
            covered_by: None,
        });
    }

    // Entries beneath a selected postprocess directory are represented by the
    // directory transform and remain available for cleanup identity checks.
    let covering_dirs: Vec<String> = entries
        .iter()
        .filter(|entry| {
            entry.kind == ManifestEntryKind::Directory
                && entry.mode == ManifestEntryMode::Postprocess
        })
        .map(|entry| entry.relative_path.as_str().to_owned())
        .collect();
    for entry in &mut entries {
        for directory in &covering_dirs {
            let relative = entry.relative_path.as_str();
            if relative != directory
                && relative.starts_with(directory)
                && relative.as_bytes().get(directory.len()) == Some(&b'/')
            {
                entry.mode = ManifestEntryMode::Covered;
                entry.covered_by = Some(directory.clone());
                entry.postprocess_steps.clear();
                break;
            }
        }
    }

    // Sort: directories first, then by depth, then by name
    entries.sort_by(|left, right| {
        let left_depth = left.relative_path.as_path().components().count();
        let right_depth = right.relative_path.as_path().components().count();
        let kind_order = |kind| match kind {
            ManifestEntryKind::Directory => 0,
            ManifestEntryKind::RegularFile | ManifestEntryKind::Symlink => 1,
        };
        left_depth
            .cmp(&right_depth)
            .then_with(|| kind_order(left.kind).cmp(&kind_order(right.kind)))
            .then_with(|| left.sync_name.as_str().cmp(right.sync_name.as_str()))
            .then_with(|| {
                left.relative_path
                    .as_str()
                    .cmp(right.relative_path.as_str())
            })
    });

    Ok((entries, has_postprocess))
}

/// Build a manifest from selected entries in every configured sync group.
/// Test-only; production code classifies and transfers groups by execution mode.
#[cfg(test)]
pub(crate) fn build_manifest(
    config: &ClientConfig,
    run_id: &RunId,
) -> Result<purgery_core::Manifest> {
    let mut entries = Vec::new();
    for sync in &config.sync {
        let sync_name = sync.name.as_str();
        let applicable = purgery_core::applicable_rules(&config.postprocess.rules, sync_name);
        let (sync_entries, _) = walk_and_classify_sync(config, sync, run_id, &applicable)?;
        entries.extend(sync_entries);
    }

    // Sort: directories first, then by depth, then by name
    entries.sort_by(|left, right| {
        let left_depth = left.relative_path.as_path().components().count();
        let right_depth = right.relative_path.as_path().components().count();
        let kind_order = |kind| match kind {
            ManifestEntryKind::Directory => 0,
            ManifestEntryKind::RegularFile | ManifestEntryKind::Symlink => 1,
        };
        left_depth
            .cmp(&right_depth)
            .then_with(|| kind_order(left.kind).cmp(&kind_order(right.kind)))
            .then_with(|| left.sync_name.as_str().cmp(right.sync_name.as_str()))
            .then_with(|| {
                left.relative_path
                    .as_str()
                    .cmp(right.relative_path.as_str())
            })
    });

    if entries.is_empty() {
        anyhow::bail!("no filesystem entries found to sync");
    }

    Ok(purgery_core::Manifest {
        run_id: run_id.clone(),
        nickname: config.nickname.clone(),
        entries,
    })
}
