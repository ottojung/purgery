use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    compute_sha256, CleanupEntry, ClientLocalPath, Manifest, ManifestEntry, ManifestEntryKind,
    Nickname, NormalizedRelativePath, RunId,
};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

/// The kind of a source filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    RegularFile,
    Directory,
    Symlink,
}

/// Normalized source specification used by all client paths.
///
/// Established by `normalize_source()` before dispatch to passthrough,
/// split, cleanup, or transform. All modes use `operation_path` for
/// rsync and `source_entry_name` for manifest, staging, and cleanup.
#[derive(Debug, Clone)]
pub(crate) struct SourceSpec {
    /// The exact CLI argument, for diagnostics only.
    #[allow(dead_code)]
    pub raw_input: String,
    /// The local path operand for rsync, normalized to preserve
    /// source-entry semantics (trailing slash stripped, `.`/`..`
    /// resolved to a concrete named path).
    pub operation_path: String,
    /// The name used for manifest `relative_path`, staged path, cleanup
    /// root `relative_path`, and the final source-entry name.
    pub source_entry_name: String,
    /// The filesystem kind, determined without following symlink sources.
    pub kind: SourceKind,
}

/// Normalize a source path before dispatch.
///
/// # Rules
///
/// - `/` is rejected in every mode.
/// - Trailing slashes are ignored for source-entry semantics.
/// - `.` is resolved to the current directory's concrete path.
/// - `..` is resolved to the parent directory's concrete path.
/// - Ordinary paths are used as-is; canonicalization is not performed.
/// - Symlink sources remain symlink sources.
pub(crate) fn normalize_source(source: &str) -> Result<SourceSpec> {
    let raw_input = source.to_owned();

    if source == "/" {
        anyhow::bail!("cannot use root path as source entry: /");
    }

    // Normalize the operand first — strip trailing slashes and resolve
    // `.`/`..` — so the normalized path is used for filesystem-kind
    // inspection. This ensures symlink sources remain symlink sources
    // even when the original CLI operand included a trailing slash.
    let (operation_path, source_entry_name) = normalize_operand(source)?;

    let metadata = fs::symlink_metadata(&operation_path)
        .with_context(|| format!("source path does not exist: {operation_path}"))?;
    let file_type = metadata.file_type();

    let kind = if file_type.is_dir() && !file_type.is_symlink() {
        SourceKind::Directory
    } else if file_type.is_file() {
        SourceKind::RegularFile
    } else if file_type.is_symlink() {
        SourceKind::Symlink
    } else {
        anyhow::bail!("unsupported source kind: {source}");
    };

    Ok(SourceSpec {
        raw_input,
        operation_path,
        source_entry_name,
        kind,
    })
}

/// Strip trailing slashes and resolve `.`/`..` without accessing the
/// filesystem for metadata. Returns the normalized path and the source
/// entry name.
fn normalize_operand(source: &str) -> Result<(String, String)> {
    if source == "." || source == ".." {
        let cwd = std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("failed to resolve current directory: {e}"))?;
        let target = if source == ".." {
            cwd.parent().map(|p| p.to_owned()).unwrap_or(cwd)
        } else {
            cwd
        };
        let name = target
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("cannot determine source entry name for '{source}'"))?
            .to_string_lossy()
            .to_string();
        if name.is_empty() {
            anyhow::bail!(
                "cannot determine source entry name for '{source}': resolved path has no name"
            );
        }
        Ok((target.to_string_lossy().to_string(), name))
    } else {
        let path = Path::new(source);
        let path_str = path.to_string_lossy().to_string();
        let stripped = path_str.trim_end_matches('/');
        let name = Path::new(stripped)
            .file_name()
            .ok_or_else(|| {
                anyhow::anyhow!("cannot determine source entry name from path: {source}")
            })?
            .to_string_lossy()
            .to_string();
        if name.is_empty() {
            anyhow::bail!("cannot determine source entry name from path: {source}");
        }
        Ok((stripped.to_owned(), name))
    }
}

/// Build a manifest with one logical source entry.
pub(crate) fn build_manifest(
    spec: &SourceSpec,
    run_id: &RunId,
    nickname: &Nickname,
    transform: Option<&str>,
) -> Result<Manifest> {
    let source_path = Path::new(&spec.operation_path);

    let metadata = fs::symlink_metadata(source_path)
        .with_context(|| format!("failed to read metadata: {}", spec.operation_path))?;
    let file_type = metadata.file_type();

    let relative_path =
        NormalizedRelativePath::new(Utf8PathBuf::from(spec.source_entry_name.clone()))
            .with_context(|| format!("invalid source name: {}", spec.source_entry_name))?;

    let staged_path =
        NormalizedRelativePath::new(Utf8PathBuf::from("files").join(&spec.source_entry_name))
            .with_context(|| "invalid staged path".to_string())?;

    let local_path = ClientLocalPath::new(spec.operation_path.clone())
        .with_context(|| format!("invalid local path: {}", spec.operation_path))?;

    let has_transform = transform.is_some();

    let kind = match spec.kind {
        SourceKind::Directory => ManifestEntryKind::Directory,
        SourceKind::RegularFile => ManifestEntryKind::RegularFile,
        SourceKind::Symlink => ManifestEntryKind::Symlink,
    };

    let size = if file_type.is_file() {
        metadata.len()
    } else {
        0
    };

    let (mtime_ns, sha256) = if file_type.is_file() && has_transform {
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let utf8_path = Utf8Path::from_path(source_path)
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path: {}", spec.operation_path))?;
        let sha = Some(
            compute_sha256(utf8_path)
                .with_context(|| format!("SHA-256 failed: {}", spec.operation_path))?,
        );
        (mtime, sha)
    } else {
        (0, None)
    };

    let link_target = if file_type.is_symlink() {
        let target = fs::read_link(source_path)
            .with_context(|| format!("failed to read symlink: {}", spec.operation_path))?;
        let target = Utf8PathBuf::from_path_buf(target)
            .map_err(|p| anyhow::anyhow!("non-UTF-8 symlink target: {}", p.display()))?;
        Some(target)
    } else {
        None
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
        transform: transform.map(|s| s.to_owned()),
    };

    Ok(Manifest {
        purgery_version: Some(purgery_core::current_purgery_version().to_string()),
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
pub(crate) fn capture_cleanup_identity(spec: &SourceSpec) -> Result<Vec<CleanupEntry>> {
    use walkdir::WalkDir;

    let source_path = Path::new(&spec.operation_path);

    let metadata = fs::symlink_metadata(source_path)
        .with_context(|| format!("failed to read metadata: {}", spec.operation_path))?;
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
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path: {}", spec.operation_path))?;
        let sha = Some(
            compute_sha256(utf8_path)
                .with_context(|| format!("SHA-256 failed: {}", spec.operation_path))?,
        );
        return Ok(vec![CleanupEntry {
            relative_path: spec.source_entry_name.clone(),
            local_path: spec.operation_path.clone(),
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
            .map(|t| t.to_string_lossy().to_string())
            .with_context(|| format!("failed to read symlink target: {}", spec.operation_path))?;
        return Ok(vec![CleanupEntry {
            relative_path: spec.source_entry_name.clone(),
            local_path: spec.operation_path.clone(),
            kind: ManifestEntryKind::Symlink,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: Some(target),
            import_confirmed: false,
            cleaned: false,
        }]);
    }

    // Directory source: include the top-level directory itself plus all
    // descendants. Relative paths are rooted at the source entry name.
    let mut entries: Vec<CleanupEntry> = Vec::new();
    let mut dirs: Vec<(String, String)> = Vec::new();

    // Top-level directory entry.
    dirs.push((spec.source_entry_name.clone(), spec.operation_path.clone()));

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
        // Prefix with source name so relative paths are rooted at the
        // source entry (e.g. "Videos/a.mp4").
        let relative_str = format!("{}/{}", spec.source_entry_name, relative.to_string_lossy());
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
            let utf8_path = Utf8Path::from_path(path).ok_or_else(|| {
                anyhow::anyhow!("non-UTF-8 path for cleanup identity: {}", path.display())
            })?;
            let sha = Some(compute_sha256(utf8_path).with_context(|| {
                format!("SHA-256 failed for cleanup identity: {}", path.display())
            })?);
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
                .map(|t| t.to_string_lossy().to_string())
                .with_context(|| format!("failed to read symlink target: {}", path.display()))?;
            entries.push(CleanupEntry {
                relative_path: relative_str,
                local_path: local_str,
                kind: ManifestEntryKind::Symlink,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: Some(target),
                import_confirmed: false,
                cleaned: false,
            });
        }
    }

    // Add directories bottom-up (deepest first) so children are deleted before parents.
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

    // Sort deepest-first for bottom-up deletion.
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
    use purgery_core::ManifestEntryKind;
    use std::fs;
    use std::os::unix;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn spec(path: &str) -> SourceSpec {
        normalize_source(path).unwrap()
    }

    #[test]
    fn normal_sync_creates_one_source_entry() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        fs::write(&file, "content").unwrap();
        let s = spec(file.to_str().unwrap());
        let manifest = build_manifest(
            &s,
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].kind, ManifestEntryKind::RegularFile);
    }

    #[test]
    fn source_may_be_a_regular_file() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("video.mp4");
        fs::write(&file, "data").unwrap();
        let s = spec(file.to_str().unwrap());
        let manifest = build_manifest(
            &s,
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            Some("compress"),
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].kind, ManifestEntryKind::RegularFile);
        assert_eq!(manifest.entries[0].transform, Some("compress".into()));
    }

    #[test]
    fn source_may_be_a_directory() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        let s = spec(dir.to_str().unwrap());
        let manifest = build_manifest(
            &s,
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            Some("compress"),
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].kind, ManifestEntryKind::Directory);
    }

    #[test]
    fn source_may_be_a_symlink() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("target.txt");
        fs::write(&target, "data").unwrap();
        let link = tmp.path().join("link");
        unix::fs::symlink(&target, &link).unwrap();
        let s = spec(link.to_str().unwrap());
        let manifest = build_manifest(
            &s,
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            Some("compress"),
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].kind, ManifestEntryKind::Symlink);
        assert!(manifest.entries[0].link_target.is_some());
    }

    #[test]
    fn transform_single_entry_no_recursive_entries() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.mp4"), "data").unwrap();
        fs::write(dir.join("b.mp4"), "data").unwrap();
        let s = spec(dir.to_str().unwrap());
        let manifest = build_manifest(
            &s,
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            Some("compress"),
        )
        .unwrap();
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
        let s = spec(dir.to_str().unwrap());
        let entries = capture_cleanup_identity(&s).unwrap();
        assert!(entries.iter().any(|e| e.relative_path == "Videos"));
        assert!(entries.iter().any(|e| e.relative_path == "Videos/a.mp4"));
        assert!(entries
            .iter()
            .any(|e| e.relative_path == "Videos/sub/b.mp4"));
        assert!(entries.iter().any(|e| e.relative_path == "Videos/sub"));
        let dir_idx = entries
            .iter()
            .position(|e| e.relative_path == "Videos")
            .unwrap();
        let file_idx = entries
            .iter()
            .position(|e| e.relative_path == "Videos/a.mp4")
            .unwrap();
        assert!(file_idx < dir_idx, "files should appear before parent dir");
    }

    #[test]
    fn cleanup_sha_failure_fatal_for_directory_descendant() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(dir.join("sub")).unwrap_or_else(|_| fs::create_dir_all(&dir).unwrap());
        fs::write(dir.join("ok.txt"), "data").unwrap();
        let bad_file = dir.join("secret.txt");
        fs::write(&bad_file, "hidden").unwrap();
        let mut perms = fs::metadata(&bad_file).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&bad_file, perms).unwrap();
        let s = spec(dir.to_str().unwrap());
        let result = capture_cleanup_identity(&s);
        assert!(
            result.is_err(),
            "SHA failure must be fatal for directory descendant"
        );
        let mut perms = fs::metadata(&bad_file).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&bad_file, perms).unwrap();
    }

    #[test]
    fn cleanup_sha_failure_fatal_for_file_source() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("secret.txt");
        fs::write(&file, "hidden").unwrap();
        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&file, perms).unwrap();
        let s = spec(file.to_str().unwrap());
        let result = capture_cleanup_identity(&s);
        assert!(result.is_err(), "SHA failure must be fatal for file source");
        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&file, perms).unwrap();
    }

    #[test]
    fn normalize_source_for_regular_file() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("video.mp4");
        fs::write(&file, "data").unwrap();
        let s = normalize_source(file.to_str().unwrap()).unwrap();
        assert_eq!(s.source_entry_name, "video.mp4");
        assert_eq!(s.kind, SourceKind::RegularFile);
        assert!(!s.operation_path.ends_with('/'));
    }

    #[test]
    fn normalize_source_for_directory() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        let s = normalize_source(dir.to_str().unwrap()).unwrap();
        assert_eq!(s.source_entry_name, "Videos");
        assert_eq!(s.kind, SourceKind::Directory);
    }

    #[test]
    fn normalize_source_with_trailing_slash() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        let source_path = format!("{}/", dir.to_str().unwrap());
        let s = normalize_source(&source_path).unwrap();
        assert_eq!(s.source_entry_name, "Videos");
        assert_eq!(s.kind, SourceKind::Directory);
        // operation_path must not have trailing slash
        assert!(!s.operation_path.ends_with('/'));
        // raw_input preserves the trailing slash
        assert!(s.raw_input.ends_with('/'));
    }

    #[test]
    fn normalize_source_for_symlink_uses_symlink_name() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("target");
        fs::write(&target, "data").unwrap();
        let link = tmp.path().join("mylink");
        unix::fs::symlink(&target, &link).unwrap();
        let s = normalize_source(link.to_str().unwrap()).unwrap();
        assert_eq!(s.source_entry_name, "mylink");
        assert_eq!(s.kind, SourceKind::Symlink);
    }

    #[test]
    fn normalize_source_rejects_root() {
        let result = normalize_source("/");
        assert!(result.is_err());
    }

    #[test]
    fn normalize_source_rejects_nonexistent() {
        let result = normalize_source("/nonexistent/path");
        assert!(result.is_err());
    }

    #[test]
    fn manifest_and_cleanup_agree_on_source_name() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.mp4"), "data").unwrap();
        let s = spec(dir.to_str().unwrap());
        let manifest = build_manifest(
            &s,
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            None,
        )
        .unwrap();
        let cleanup = capture_cleanup_identity(&s).unwrap();
        assert_eq!(manifest.entries[0].relative_path.as_str(), "Videos");
        assert!(cleanup.iter().any(|e| e.relative_path == "Videos"));
    }

    #[test]
    fn normalize_source_dot_resolves_to_current_dir_name() {
        let cwd = std::env::current_dir().unwrap();
        let expected_name = cwd.file_name().unwrap().to_string_lossy().to_string();
        let s = normalize_source(".").unwrap();
        assert_eq!(s.source_entry_name, expected_name);
        assert!(!s.operation_path.ends_with('/'));
        assert_ne!(s.operation_path, ".");
        assert!(!s.operation_path.ends_with("/."));
    }

    #[test]
    fn normalize_source_dotdot_resolves_to_parent_dir_name() {
        let cwd = std::env::current_dir().unwrap();
        let parent = cwd.parent().unwrap_or(&cwd).to_owned();
        let expected_name = parent.file_name().unwrap().to_string_lossy().to_string();
        let s = normalize_source("..").unwrap();
        assert_eq!(s.source_entry_name, expected_name);
        assert!(!s.operation_path.ends_with('/'));
        assert_ne!(s.operation_path, "..");
    }

    #[test]
    fn trailing_slash_dir_preserves_source_entry_semantics_in_manifest() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        let source_with_slash = format!("{}/", dir.to_str().unwrap());
        let s = normalize_source(&source_with_slash).unwrap();
        let manifest = build_manifest(
            &s,
            &RunId::new("test".into()).unwrap(),
            &Nickname::new("host".into()).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(manifest.entries[0].relative_path.as_str(), "Videos");
        assert_eq!(manifest.entries[0].staged_path.as_str(), "files/Videos");
    }

    #[test]
    fn trailing_slash_dir_preserves_source_entry_semantics_in_cleanup() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Videos");
        fs::create_dir(&dir).unwrap();
        let source_with_slash = format!("{}/", dir.to_str().unwrap());
        let s = normalize_source(&source_with_slash).unwrap();
        let cleanup = capture_cleanup_identity(&s).unwrap();
        assert!(cleanup.iter().any(|e| e.relative_path == "Videos"));
    }

    #[test]
    fn symlink_to_directory_with_trailing_slash_remains_symlink_kind() {
        let tmp = tempdir().unwrap();
        let real_dir = tmp.path().join("realdir");
        fs::create_dir(&real_dir).unwrap();
        let link = tmp.path().join("linkdir");
        unix::fs::symlink(&real_dir, &link).unwrap();
        let source_with_slash = format!("{}/", link.to_str().unwrap());
        let s = normalize_source(&source_with_slash).unwrap();
        assert_eq!(s.source_entry_name, "linkdir");
        assert_eq!(s.kind, SourceKind::Symlink);
        assert!(!s.operation_path.ends_with('/'));
    }

    #[test]
    fn symlink_to_file_with_trailing_slash_remains_symlink_kind() {
        let tmp = tempdir().unwrap();
        let real_file = tmp.path().join("real.txt");
        fs::write(&real_file, "data").unwrap();
        let link = tmp.path().join("linkfile");
        unix::fs::symlink(&real_file, &link).unwrap();
        let source_with_slash = format!("{}/", link.to_str().unwrap());
        let s = normalize_source(&source_with_slash).unwrap();
        assert_eq!(s.source_entry_name, "linkfile");
        assert_eq!(s.kind, SourceKind::Symlink);
        assert!(!s.operation_path.ends_with('/'));
    }

    #[test]
    fn symlink_source_without_trailing_slash_is_symlink_kind() {
        let tmp = tempdir().unwrap();
        let real_file = tmp.path().join("real.txt");
        fs::write(&real_file, "data").unwrap();
        let link = tmp.path().join("mylink");
        unix::fs::symlink(&real_file, &link).unwrap();
        let s = normalize_source(link.to_str().unwrap()).unwrap();
        assert_eq!(s.source_entry_name, "mylink");
        assert_eq!(s.kind, SourceKind::Symlink);
    }

    #[test]
    fn normalize_source_rejects_root_in_all_forms() {
        assert!(normalize_source("/").is_err());
        assert!(
            normalize_source("//").is_err(),
            "double-slash root should also be invalid"
        );
    }
}
