use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    compute_sha256, ClientLocalPath, Manifest, ManifestEntry, ManifestEntryKind, ManifestEntryMode,
    Nickname, NormalizedRelativePath, RunId,
};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

/// Build a manifest with one logical source entry.
pub(crate) fn build_manifest(
    source: &str,
    run_id: &RunId,
    nickname: &Nickname,
    postprocess_steps: &[String],
) -> Result<Manifest> {
    let source_path = Path::new(source);
    if !source_path.exists() {
        anyhow::bail!("source path does not exist: {source}");
    }

    let metadata = fs::symlink_metadata(source_path)
        .with_context(|| format!("failed to read metadata: {source}"))?;
    let file_type = metadata.file_type();

    let source_name = source_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| source.to_owned());

    let relative_path = NormalizedRelativePath::new(Utf8PathBuf::from(source_name.clone()))
        .with_context(|| format!("invalid source name: {source_name}"))?;

    let staged_path = NormalizedRelativePath::new(Utf8PathBuf::from("files").join(&source_name))
        .with_context(|| "invalid staged path".to_string())?;

    let local_path_str = source_path.to_string_lossy().to_string();
    let local_path = ClientLocalPath::new(local_path_str)
        .with_context(|| format!("invalid local path: {source}"))?;

    let is_regular_file = file_type.is_file();
    let is_dir = file_type.is_dir();
    let is_symlink = file_type.is_symlink();

    let has_postprocess = !postprocess_steps.is_empty();

    let kind = if is_dir {
        ManifestEntryKind::Directory
    } else if is_regular_file {
        ManifestEntryKind::RegularFile
    } else if is_symlink {
        ManifestEntryKind::Symlink
    } else {
        anyhow::bail!("unsupported source kind: {source}");
    };

    let size = if is_regular_file { metadata.len() } else { 0 };

    let (mtime_ns, sha256) = if is_regular_file && has_postprocess {
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let utf8_path = Utf8Path::from_path(source_path)
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path: {source}"))?;
        let sha =
            Some(compute_sha256(utf8_path).with_context(|| format!("SHA-256 failed: {source}"))?);
        (mtime, sha)
    } else {
        (0, None)
    };

    let link_target = if is_symlink {
        let target = fs::read_link(source_path)
            .with_context(|| format!("failed to read symlink: {source}"))?;
        let target = Utf8PathBuf::from_path_buf(target)
            .map_err(|p| anyhow::anyhow!("non-UTF-8 symlink target: {}", p.display()))?;
        Some(target)
    } else {
        None
    };

    let mode = if has_postprocess {
        ManifestEntryMode::Postprocess
    } else {
        ManifestEntryMode::Passthrough
    };

    let entry = ManifestEntry {
        local_path,
        staged_path,
        relative_path,
        kind,
        size,
        mtime_ns,
        sha256,
        link_target,
        mode,
        postprocess_steps: postprocess_steps.to_vec(),
    };

    Ok(Manifest {
        run_id: run_id.clone(),
        nickname: nickname.clone(),
        entries: vec![entry],
    })
}

/// Capture cleanup identity for a source entry.
///
/// For a directory source, recursively captures descendant identities
/// so the client can safely delete the entire imported tree after
/// server-confirmed import. These identities are used only for
/// deletion safety — the manifest/status still describe one logical entry.
pub(crate) fn capture_cleanup_identity(source: &str) -> Result<Vec<purgery_core::CleanupEntry>> {
    use purgery_core::CleanupEntry;
    use std::path::Path;
    use walkdir::WalkDir;

    let source_path = Path::new(source);
    if !source_path.exists() {
        return Ok(Vec::new());
    }

    let metadata = fs::symlink_metadata(source_path)
        .with_context(|| format!("failed to read metadata: {source}"))?;
    let file_type = metadata.file_type();

    if file_type.is_file() {
        let size = metadata.len();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let utf8_path = Utf8Path::from_path(source_path)
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path: {source}"))?;
        let sha =
            Some(compute_sha256(utf8_path).with_context(|| format!("SHA-256 failed: {source}"))?);
        let name = source_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| source.to_owned());
        return Ok(vec![CleanupEntry {
            relative_path: name,
            local_path: source.to_owned(),
            kind: ManifestEntryKind::RegularFile,
            size,
            mtime_ns: mtime,
            sha256: sha,
            link_target: None,
            import_confirmed: false,
            cleaned: false,
        }]);
    }

    if file_type.is_symlink() {
        let target = fs::read_link(source_path)
            .ok()
            .map(|t| t.to_string_lossy().to_string());
        let name = source_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| source.to_owned());
        return Ok(vec![CleanupEntry {
            relative_path: name,
            local_path: source.to_owned(),
            kind: ManifestEntryKind::Symlink,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: target,
            import_confirmed: false,
            cleaned: false,
        }]);
    }

    // Directory source: walk all descendants to capture identities.
    let mut entries: Vec<CleanupEntry> = Vec::new();
    let mut dirs: Vec<(String, String)> = Vec::new();

    for walk_entry in WalkDir::new(source_path).follow_links(false).min_depth(1) {
        let walk_entry = match walk_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = walk_entry.path();
        let ft = match fs::symlink_metadata(path) {
            Ok(m) => m.file_type(),
            Err(_) => continue,
        };

        let relative = match path.strip_prefix(source_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let relative_str = relative.to_string_lossy().to_string();

        let local_str = path.to_string_lossy().to_string();

        if ft.is_dir() {
            dirs.push((relative_str, local_str));
        } else if ft.is_file() {
            let size = match fs::metadata(path) {
                Ok(m) => m.len(),
                Err(_) => 0,
            };
            let mtime = fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            let sha = Utf8Path::from_path(path).and_then(|p| compute_sha256(p).ok());
            entries.push(CleanupEntry {
                relative_path: relative_str,
                local_path: local_str,
                kind: ManifestEntryKind::RegularFile,
                size,
                mtime_ns: mtime,
                sha256: sha,
                link_target: None,
                import_confirmed: false,
                cleaned: false,
            });
        } else if ft.is_symlink() {
            let target = fs::read_link(path)
                .ok()
                .map(|t| t.to_string_lossy().to_string());
            entries.push(CleanupEntry {
                relative_path: relative_str,
                local_path: local_str,
                kind: ManifestEntryKind::Symlink,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: target,
                import_confirmed: false,
                cleaned: false,
            });
        }
    }

    // Add directories bottom-up (deepest first).
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

#[cfg(test)]
mod tests {
    use super::*;
    use purgery_core::{ManifestEntryKind, ManifestEntryMode};
    use std::fs;
    use std::os::unix;
    use tempfile::tempdir;

    #[test]
    fn normal_sync_creates_one_source_entry() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        fs::write(&file, "content").unwrap();
        let manifest = build_manifest(
            file.to_str().unwrap(),
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            &[],
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].kind, ManifestEntryKind::RegularFile);
        assert_eq!(manifest.entries[0].mode, ManifestEntryMode::Passthrough);
    }

    #[test]
    fn source_may_be_a_regular_file() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("video.mp4");
        fs::write(&file, "data").unwrap();
        let manifest = build_manifest(
            file.to_str().unwrap(),
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            &["compress".into()],
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].kind, ManifestEntryKind::RegularFile);
        assert_eq!(manifest.entries[0].mode, ManifestEntryMode::Postprocess);
        assert_eq!(manifest.entries[0].postprocess_steps, vec!["compress"]);
    }

    #[test]
    fn source_may_be_a_directory() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        let manifest = build_manifest(
            dir.to_str().unwrap(),
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            &["compress".into()],
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].kind, ManifestEntryKind::Directory);
        assert_eq!(manifest.entries[0].mode, ManifestEntryMode::Postprocess);
    }

    #[test]
    fn source_may_be_a_symlink() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("target.txt");
        fs::write(&target, "data").unwrap();
        let link = tmp.path().join("link");
        unix::fs::symlink(&target, &link).unwrap();
        let manifest = build_manifest(
            link.to_str().unwrap(),
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            &["compress".into()],
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].kind, ManifestEntryKind::Symlink);
        assert_eq!(manifest.entries[0].mode, ManifestEntryMode::Postprocess);
        assert!(manifest.entries[0].link_target.is_some());
    }

    #[test]
    fn postprocess_single_entry_no_recursive_entries() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.mp4"), "data").unwrap();
        fs::write(dir.join("b.mp4"), "data").unwrap();
        let manifest = build_manifest(
            dir.to_str().unwrap(),
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            &["compress".into()],
        )
        .unwrap();
        // One logical entry for the source directory, not three.
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].kind, ManifestEntryKind::Directory);
    }

    #[test]
    fn capture_cleanup_identity_directory_descendants() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.mp4"), "data").unwrap();
        fs::write(dir.join("sub/b.mp4"), "data").unwrap();
        let entries = capture_cleanup_identity(dir.to_str().unwrap()).unwrap();
        // Should include both files and the subdirectory (not the top-level dir)
        assert!(!entries.is_empty());
        assert!(entries.iter().any(|e| e.relative_path == "a.mp4"));
        assert!(entries.iter().any(|e| e.relative_path == "sub/b.mp4"));
        assert!(entries.iter().any(|e| e.relative_path == "sub"));
    }
}
