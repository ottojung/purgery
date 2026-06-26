use camino::{Utf8Path, Utf8PathBuf};
use std::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitDisposition {
    Created,
    Kept,
    Replaced,
}

pub(crate) fn prepare_destination_for_file_or_symlink(
    final_path: &Utf8Path,
) -> Result<CommitDisposition, String> {
    match fs::symlink_metadata(final_path.as_std_path()) {
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir(final_path.as_std_path()).map_err(|error| {
                format!(
                    "cannot replace non-empty destination directory '{}': {error}",
                    final_path.as_str()
                )
            })?;
            Ok(CommitDisposition::Replaced)
        }
        Ok(_) => Ok(CommitDisposition::Replaced),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(CommitDisposition::Created)
        }
        Err(error) => Err(format!("failed to inspect final destination: {error}")),
    }
}

fn ensure_destination_root(root: &Utf8Path) -> Result<(), String> {
    match fs::symlink_metadata(root.as_std_path()) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(format!(
            "destination root is not a directory: {}",
            root.as_str()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root.as_std_path())
                .map_err(|error| format!("failed to create destination root: {error}"))?;
            let metadata = fs::symlink_metadata(root.as_std_path())
                .map_err(|error| format!("failed to inspect destination root: {error}"))?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                Ok(())
            } else {
                Err(format!(
                    "destination root is not a directory: {}",
                    root.as_str()
                ))
            }
        }
        Err(error) => Err(format!("failed to inspect destination root: {error}")),
    }
}

fn ensure_final_parent(final_path: &Utf8Path, root: &Utf8Path) -> Result<(), String> {
    ensure_destination_root(root)?;
    let parent = final_path
        .parent()
        .ok_or_else(|| format!("final path has no parent: {}", final_path.as_str()))?;
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| format!("final parent escapes root: {}", parent.as_str()))?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component.as_str());
        match fs::symlink_metadata(current.as_std_path()) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(format!(
                    "final parent is not a directory: {}",
                    current.as_str()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(current.as_std_path()).map_err(|error| {
                    format!(
                        "failed to create final parent '{}': {error}",
                        current.as_str()
                    )
                })?;
            }
            Err(error) => return Err(format!("failed to inspect final parent: {error}")),
        }
    }
    Ok(())
}

pub(crate) fn commit_directory_entry(
    final_path: &Utf8Path,
    root: &Utf8Path,
) -> Result<CommitDisposition, String> {
    ensure_final_parent(final_path, root)?;
    match fs::symlink_metadata(final_path.as_std_path()) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Ok(CommitDisposition::Kept)
        }
        Ok(metadata) => {
            if metadata.is_dir() {
                return Err(format!(
                    "unsupported destination directory type: {}",
                    final_path
                ));
            }
            fs::remove_file(final_path.as_std_path())
                .map_err(|error| format!("failed to remove conflicting destination: {error}"))?;
            fs::create_dir(final_path.as_std_path())
                .map_err(|error| format!("failed to create destination directory: {error}"))?;
            Ok(CommitDisposition::Replaced)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(final_path.as_std_path())
                .map_err(|error| format!("failed to create destination directory: {error}"))?;
            Ok(CommitDisposition::Created)
        }
        Err(error) => Err(format!("failed to inspect destination directory: {error}")),
    }
}

pub(crate) fn commit_regular_file_entry(
    source: &Utf8Path,
    final_path: &Utf8Path,
    root: &Utf8Path,
    _run_id: &purgery_core::RunId,
) -> Result<CommitDisposition, String> {
    ensure_final_parent(final_path, root)?;
    let disposition = prepare_destination_for_file_or_symlink(final_path)?;
    fs::rename(source.as_std_path(), final_path.as_std_path())
        .map_err(|error| format!("failed to materialize regular file: {error}"))?;
    Ok(disposition)
}

pub(crate) fn commit_symlink_entry(
    source: &Utf8Path,
    final_path: &Utf8Path,
    root: &Utf8Path,
    _run_id: &purgery_core::RunId,
) -> Result<CommitDisposition, String> {
    ensure_final_parent(final_path, root)?;
    let disposition = prepare_destination_for_file_or_symlink(final_path)?;
    fs::rename(source.as_std_path(), final_path.as_std_path())
        .map_err(|error| format!("failed to materialize symlink: {error}"))?;
    Ok(disposition)
}

pub(crate) fn commit_directory_tree(
    source_root: &Utf8Path,
    final_root: &Utf8Path,
    destination_root: &Utf8Path,
    run_id: &purgery_core::RunId,
) -> Result<CommitDisposition, String> {
    use walkdir::WalkDir;

    let root_disp = commit_directory_entry(final_root, destination_root)?;

    let mut entries: Vec<(Utf8PathBuf, Utf8PathBuf)> = Vec::new();
    for entry in WalkDir::new(source_root.as_std_path())
        .min_depth(1)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|e| format!("failed to walk source directory: {e}"))?;
        let relative = entry
            .path()
            .strip_prefix(source_root.as_std_path())
            .map_err(|_| "failed to compute relative path in source tree".to_string())?;
        let source_entry = Utf8PathBuf::from_path_buf(entry.path().to_path_buf())
            .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref()));
        let final_entry = final_root.join(Utf8Path::new(
            relative
                .to_str()
                .ok_or_else(|| "non-UTF-8 path in directory tree".to_string())?,
        ));
        entries.push((source_entry, final_entry));
    }

    for (source_entry, final_entry) in &entries {
        let meta = fs::symlink_metadata(source_entry.as_std_path())
            .map_err(|e| format!("failed to read output entry metadata: {e}"))?;
        if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
            commit_directory_entry(final_entry, destination_root)?;
        } else if meta.file_type().is_file() {
            commit_regular_file_entry(source_entry, final_entry, destination_root, run_id)?;
        } else if meta.file_type().is_symlink() {
            commit_symlink_entry(source_entry, final_entry, destination_root, run_id)?;
        } else {
            return Err(format!(
                "unsupported output entry type: {}",
                source_entry.as_str()
            ));
        }
    }

    // Remove empty source directories bottom-up.
    for entry in WalkDir::new(source_root.as_std_path())
        .min_depth(1)
        .contents_first(true)
    {
        let entry = entry.map_err(|e| format!("failed to walk source directory: {e}"))?;
        if entry.file_type().is_dir() {
            let _ = fs::remove_dir(entry.path());
        }
    }
    let _ = fs::remove_dir(source_root.as_std_path());

    Ok(root_disp)
}
