use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    compute_sha256, ClientLocalPath, Manifest, ManifestEntry, ManifestEntryKind, ManifestEntryMode,
    Nickname, NormalizedRelativePath, RunId,
};
use std::collections::HashSet;
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
    let walk_root = source_path.to_path_buf();
    let mut entries: Vec<ManifestEntry> = Vec::new();

    // Track which directories are postprocess roots so their descendants
    // can be marked as covered.
    let mut covering_dirs: HashSet<String> = HashSet::new();

    for walk_entry in WalkDir::new(&walk_root).follow_links(false).min_depth(1) {
        let walk_entry = walk_entry.with_context(|| format!("error walking {source}"))?;
        let path = walk_entry.path();

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

        // Determine if this entry is covered by a postprocessed parent directory.
        let parent_dir = relative_path.parent().map(|p| p.as_str().to_owned());
        let is_covered = parent_dir
            .as_ref()
            .is_some_and(|p| covering_dirs.contains(p));

        // Determine mode and steps for this entry.
        let is_top_level = relative_path.parent().is_none_or(|p| p.as_str().is_empty());

        let (mode, steps) = if has_postprocess && is_top_level {
            // Top-level entries in postprocess runs get postprocess mode.
            (ManifestEntryMode::Postprocess, postprocess_steps.to_vec())
        } else if has_postprocess && is_covered {
            // Descendants of postprocess directory roots are covered.
            (ManifestEntryMode::Covered, Vec::new())
        } else {
            (ManifestEntryMode::Passthrough, Vec::new())
        };

        // Track postprocess directory roots so their descendants are covered.
        if file_type.is_dir() && mode == ManifestEntryMode::Postprocess {
            covering_dirs.insert(relative_path.as_str().to_owned());
        }

        // Compute identity fields.
        let is_regular_file = file_type.is_file();
        let is_symlink = file_type.is_symlink();
        let is_dir = file_type.is_dir();

        let size = if is_regular_file { metadata.len() } else { 0 };
        let (mtime_ns, sha256) =
            if is_regular_file && (has_postprocess || capture_cleanup_identity || size > 0) {
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

        let kind = if is_dir {
            ManifestEntryKind::Directory
        } else if is_regular_file {
            ManifestEntryKind::RegularFile
        } else if is_symlink {
            ManifestEntryKind::Symlink
        } else {
            anyhow::bail!("unsupported filesystem entry: {}", path.display());
        };

        let link_target = if is_symlink {
            let target = fs::read_link(path)
                .with_context(|| format!("failed to read symlink: {}", path.display()))?;
            let target = Utf8PathBuf::from_path_buf(target)
                .map_err(|p| anyhow::anyhow!("non-UTF-8 symlink target: {}", p.display()))?;
            Some(target)
        } else {
            None
        };

        let covered_by = if is_covered { parent_dir } else { None };

        entries.push(ManifestEntry {
            local_path,
            staged_path: staged_path_norm,
            relative_path: relative_path_norm,
            kind,
            size,
            mtime_ns,
            sha256,
            link_target,
            mode,
            postprocess_steps: steps,
            covered_by,
        });
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
