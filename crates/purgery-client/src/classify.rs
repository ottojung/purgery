use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    compute_sha256, ClientLocalPath, Manifest, ManifestEntry, ManifestEntryKind, ManifestEntryMode,
    Nickname, NormalizedRelativePath, RunId,
};
use std::fs;
use std::path::Path;
use std::time::SystemTime;
use walkdir::WalkDir;

pub(crate) fn build_manifest(
    source: &str,
    run_id: &RunId,
    nickname: &Nickname,
    postprocess_steps: &[String],
    capture_cleanup_identity: bool,
) -> Result<Manifest> {
    let source_path = Path::new(source);
    if !source_path.exists() {
        anyhow::bail!("source path does not exist: {source}");
    }

    let has_postprocess = !postprocess_steps.is_empty();

    let walk_root = if source_path.is_file() {
        source_path.parent().unwrap_or(source_path).to_path_buf()
    } else {
        source_path.to_path_buf()
    };

    let is_file_source = source_path.is_file();
    let source_abs = source_path;
    let mut entries: Vec<ManifestEntry> = Vec::new();

    for walk_entry in WalkDir::new(&walk_root).follow_links(false).min_depth(1) {
        let walk_entry = walk_entry.with_context(|| format!("error walking {source}"))?;
        let path = walk_entry.path();

        if is_file_source && path != source_abs {
            continue;
        }

        let relative = path
            .strip_prefix(&walk_root)
            .with_context(|| format!("failed to compute relative path: {}", path.display()))?;
        let relative_path = Utf8PathBuf::from_path_buf(relative.to_path_buf())
            .map_err(|p| anyhow::anyhow!("non-UTF-8 relative path: {}", p.display()))?;
        let relative_path_norm = NormalizedRelativePath::new(relative_path.clone())
            .with_context(|| format!("invalid relative path: {}", relative_path.as_str()))?;

        let staged_path = Utf8PathBuf::from("files").join(&relative_path);
        let staged_path_norm = NormalizedRelativePath::new(staged_path)
            .with_context(|| format!("invalid staged path: {}", relative.display()))?;

        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to read metadata: {}", path.display()))?;
        let file_type = metadata.file_type();

        let local_path_str = path.to_string_lossy().to_string();
        let local_path = ClientLocalPath::new(local_path_str)
            .with_context(|| format!("invalid local path: {}", path.display()))?;

        if file_type.is_dir() {
            entries.push(ManifestEntry {
                local_path,
                staged_path: staged_path_norm,
                relative_path: relative_path_norm,
                kind: ManifestEntryKind::Directory,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                mode: ManifestEntryMode::Passthrough,
                postprocess_steps: Vec::new(),
                covered_by: None,
            });
        } else if file_type.is_file() {
            let size = metadata.len();
            let (mtime_ns, sha256) = if has_postprocess || capture_cleanup_identity || size > 0 {
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0);
                let sha = if has_postprocess || capture_cleanup_identity {
                    let utf8_path = Utf8Path::from_path(path)
                        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path: {}", path.display()))?;
                    Some(
                        compute_sha256(utf8_path)
                            .with_context(|| format!("SHA-256 failed: {}", path.display()))?,
                    )
                } else {
                    None
                };
                (mtime, sha)
            } else {
                (0, None)
            };

            let mode = if has_postprocess {
                ManifestEntryMode::Postprocess
            } else {
                ManifestEntryMode::Passthrough
            };

            entries.push(ManifestEntry {
                local_path,
                staged_path: staged_path_norm,
                relative_path: relative_path_norm,
                kind: ManifestEntryKind::RegularFile,
                size,
                mtime_ns,
                sha256: if has_postprocess || capture_cleanup_identity {
                    sha256
                } else {
                    None
                },
                link_target: None,
                mode,
                postprocess_steps: if has_postprocess {
                    postprocess_steps.to_vec()
                } else {
                    Vec::new()
                },
                covered_by: None,
            });
        } else if file_type.is_symlink() {
            let target = fs::read_link(path)
                .with_context(|| format!("failed to read symlink: {}", path.display()))?;
            let target = Utf8PathBuf::from_path_buf(target)
                .map_err(|p| anyhow::anyhow!("non-UTF-8 symlink target: {}", p.display()))?;

            entries.push(ManifestEntry {
                local_path,
                staged_path: staged_path_norm,
                relative_path: relative_path_norm,
                kind: ManifestEntryKind::Symlink,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: Some(target),
                mode: ManifestEntryMode::Passthrough,
                postprocess_steps: Vec::new(),
                covered_by: None,
            });
        }
    }

    let covering_dirs: Vec<String> = entries
        .iter()
        .filter(|e| {
            e.kind == ManifestEntryKind::Directory && e.mode == ManifestEntryMode::Postprocess
        })
        .map(|e| e.relative_path.as_str().to_owned())
        .collect();
    for entry in entries.iter_mut() {
        if entry.mode == ManifestEntryMode::Postprocess {
            continue;
        }
        let rp = entry.relative_path.as_str();
        for dir_path in &covering_dirs {
            if rp == dir_path.as_str() {
                continue;
            }
            if rp.starts_with(dir_path.as_str()) && rp.as_bytes().get(dir_path.len()) == Some(&b'/')
            {
                entry.mode = ManifestEntryMode::Covered;
                entry.covered_by = Some(dir_path.clone());
                entry.postprocess_steps = Vec::new();
                break;
            }
        }
    }

    entries.sort_by(|a, b| {
        let a_depth = a.relative_path.as_path().components().count();
        let b_depth = b.relative_path.as_path().components().count();
        let kind_order = |kind| match kind {
            ManifestEntryKind::Directory => 0,
            ManifestEntryKind::RegularFile | ManifestEntryKind::Symlink => 1,
        };
        a_depth
            .cmp(&b_depth)
            .then_with(|| kind_order(a.kind).cmp(&kind_order(b.kind)))
            .then_with(|| a.relative_path.as_str().cmp(b.relative_path.as_str()))
    });

    if entries.is_empty() {
        anyhow::bail!("no filesystem entries found to sync");
    }

    Ok(Manifest {
        run_id: run_id.clone(),
        nickname: nickname.clone(),
        entries,
    })
}
