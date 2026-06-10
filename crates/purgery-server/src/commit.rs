use camino::{Utf8Path, Utf8PathBuf};
use std::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitDisposition {
    Created,
    Kept,
    Replaced,
}

pub(crate) fn remove_destination_for_non_directory(
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

pub(crate) fn remove_stale_temp(temp_path: &Utf8Path) -> Result<(), String> {
    match fs::symlink_metadata(temp_path.as_std_path()) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(temp_path.as_std_path())
            .map_err(|error| format!("failed to remove stale temporary directory: {error}")),
        Ok(_) => fs::remove_file(temp_path.as_std_path())
            .map_err(|error| format!("failed to remove stale temporary entry: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to inspect temporary entry: {error}")),
    }
}

fn ensure_final_parent(final_path: &Utf8Path, root: &Utf8Path) -> Result<(), String> {
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
    run_id: &purgery_core::RunId,
) -> Result<CommitDisposition, String> {
    ensure_final_parent(final_path, root)?;
    let disposition = remove_destination_for_non_directory(final_path)?;
    let temp_path = purgery_core::commit_temp_path(final_path, run_id);
    remove_stale_temp(&temp_path)?;
    fs::copy(source.as_std_path(), temp_path.as_std_path())
        .map_err(|error| format!("failed to copy regular file to temporary path: {error}"))?;
    if let Err(error) = fs::rename(temp_path.as_std_path(), final_path.as_std_path()) {
        let _ = fs::remove_file(temp_path.as_std_path());
        return Err(format!("failed to commit regular file: {error}"));
    }
    Ok(disposition)
}

#[cfg(unix)]
fn create_symlink(target: &Utf8Path, link: &Utf8Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target.as_std_path(), link.as_std_path())
}

pub(crate) fn commit_symlink_entry(
    target: &Utf8Path,
    final_path: &Utf8Path,
    root: &Utf8Path,
    run_id: &purgery_core::RunId,
) -> Result<CommitDisposition, String> {
    ensure_final_parent(final_path, root)?;
    let disposition = remove_destination_for_non_directory(final_path)?;
    let temp_path = purgery_core::commit_temp_path(final_path, run_id);
    remove_stale_temp(&temp_path)?;
    create_symlink(target, &temp_path)
        .map_err(|error| format!("failed to create temporary symlink: {error}"))?;
    if let Err(error) = fs::rename(temp_path.as_std_path(), final_path.as_std_path()) {
        let _ = fs::remove_file(temp_path.as_std_path());
        return Err(format!("failed to commit symlink: {error}"));
    }
    Ok(disposition)
}

pub(crate) fn commit_directory_tree(
    source_root: &Utf8Path,
    final_root: &Utf8Path,
    server_root: &Utf8Path,
    run_id: &purgery_core::RunId,
) -> Result<CommitDisposition, String> {
    use walkdir::WalkDir;

    let root_disp = commit_directory_entry(final_root, server_root)?;

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
            commit_directory_entry(final_entry, server_root)?;
        } else if meta.file_type().is_file() {
            commit_regular_file_entry(source_entry, final_entry, server_root, run_id)?;
        } else if meta.file_type().is_symlink() {
            let target = fs::read_link(source_entry.as_std_path())
                .map_err(|e| format!("failed to read output symlink target: {e}"))?;
            let target_utf8 = Utf8PathBuf::from_path_buf(target)
                .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref()));
            commit_symlink_entry(&target_utf8, final_entry, server_root, run_id)?;
        } else {
            return Err(format!(
                "unsupported output entry type: {}",
                source_entry.as_str()
            ));
        }
    }

    Ok(root_disp)
}

pub(crate) fn commit_output_entry(
    source: &Utf8Path,
    final_path: &Utf8Path,
    server_root: &Utf8Path,
    run_id: &purgery_core::RunId,
) -> Result<CommitDisposition, String> {
    let meta = fs::symlink_metadata(source.as_std_path())
        .map_err(|e| format!("failed to inspect output entry: {e}"))?;
    if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
        commit_directory_tree(source, final_path, server_root, run_id)
    } else if meta.file_type().is_file() {
        commit_regular_file_entry(source, final_path, server_root, run_id)
    } else if meta.file_type().is_symlink() {
        let target = fs::read_link(source.as_std_path())
            .map_err(|e| format!("failed to read output symlink target: {e}"))?;
        let target_utf8 = Utf8PathBuf::from_path_buf(target)
            .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref()));
        commit_symlink_entry(&target_utf8, final_path, server_root, run_id)
    } else {
        Err(format!(
            "unsupported output entry type: {}",
            source.as_str()
        ))
    }
}
