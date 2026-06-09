use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    path_is_within_root, validate_envelope, work_dir, EntryStatusEntry, FileStatus, Manifest,
    ManifestEntry, ManifestEntryKind, Nickname, NormalizedRelativePath, PurgeryRoot, RunConfig,
    RunConfigSync, RunId, RunPhase, RunState, RunStatus, ServerConfig,
};
use std::collections::HashMap;
use std::fs;
use tracing::{debug, info, span, warn, Level};

fn publish_status_atomic(directory: &Utf8Path, status: &RunStatus) -> Result<()> {
    let content = status.to_toml().context("failed to serialize status")?;
    let temporary = directory.join("status.toml.tmp");
    let final_path = directory.join("status.toml");
    fs::write(&temporary, content)
        .with_context(|| format!("failed to write temporary status: {}", temporary))?;
    fs::rename(&temporary, &final_path)
        .with_context(|| format!("failed to publish status: {}", final_path))?;
    Ok(())
}

/// Persist a run-level failure and move the processing directory to `failed/`.
fn write_run_failure(
    purgery_root: &PurgeryRoot,
    nickname: &Nickname,
    run_id: &RunId,
    error_msg: &str,
) -> Result<()> {
    let processing_path = purgery_root.run_dir(nickname, run_id, RunPhase::Processing);
    let status = RunStatus {
        run_id: run_id.clone(),
        nickname: nickname.clone(),
        state: RunState::Failed,
        entries: vec![],
        error: Some(error_msg.to_owned()),
    };
    let status_toml = status
        .to_toml()
        .with_context(|| "failed to serialize run failure status")?;
    let status_path = processing_path.join("status.toml");
    let tmp_path = processing_path.join("status.toml.tmp");

    fs::write(&tmp_path, &status_toml).with_context(|| {
        format!(
            "failed to write temporary run failure status: {}",
            tmp_path.as_str()
        )
    })?;
    fs::rename(&tmp_path, &status_path).with_context(|| {
        format!(
            "failed to finalize run failure status: {}",
            status_path.as_str()
        )
    })?;

    let failed_path = purgery_root.run_dir(nickname, run_id, RunPhase::Failed);
    if let Some(parent) = failed_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create failed parent: {}", parent.as_str()))?;
    }
    fs::rename(&processing_path, &failed_path).with_context(|| {
        format!(
            "failed to move run-level failure to failed: {} -> {}",
            processing_path.as_str(),
            failed_path.as_str()
        )
    })?;

    Ok(())
}

/// Find all runs in one durable phase across all nicknames.
fn find_runs_in_phase(
    purgery_root: &PurgeryRoot,
    phase: RunPhase,
) -> Result<Vec<(Nickname, RunId)>> {
    let mut runs = Vec::new();
    let purgery_path = purgery_root.as_path();

    if !purgery_path.exists() {
        return Ok(runs);
    }

    for entry in fs::read_dir(purgery_path)
        .with_context(|| format!("failed to read purgery root: {}", purgery_path.as_str()))?
    {
        let entry = entry?;
        let nickname_path = entry.path();
        if !nickname_path.is_dir() {
            continue;
        }
        let nickname_str = nickname_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let Ok(nickname) = Nickname::new(nickname_str.to_owned()) else {
            continue;
        };

        let phase_path = nickname_path.join(phase.as_str());
        if !phase_path.exists() {
            continue;
        }

        for run_entry in fs::read_dir(&phase_path).with_context(|| {
            format!(
                "failed to read {} dir: {}",
                phase.as_str(),
                phase_path.display()
            )
        })? {
            let run_entry = run_entry?;
            let run_path = run_entry.path();
            if !run_path.is_dir() {
                continue;
            }
            let run_id_str = run_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let Ok(run_id) = RunId::new(run_id_str.to_owned()) else {
                continue;
            };
            runs.push((nickname.clone(), run_id));
        }
    }

    Ok(runs)
}

/// Find all ready runs across all nicknames.
pub fn find_ready_runs(purgery_root: &PurgeryRoot) -> Result<Vec<(Nickname, RunId)>> {
    find_runs_in_phase(purgery_root, RunPhase::Ready)
}

/// Find all interrupted or terminal-pending processing runs across all nicknames.
pub fn find_processing_runs(purgery_root: &PurgeryRoot) -> Result<Vec<(Nickname, RunId)>> {
    find_runs_in_phase(purgery_root, RunPhase::Processing)
}

/// Per-entry outcome.
enum EntryOutcome {
    Success {
        kind: ManifestEntryKind,
        sync_name: purgery_core::SyncName,
        local_path: String,
        relative_path: String,
        final_paths: Vec<String>,
        postprocess: Option<Vec<String>>,
    },
    Failure {
        kind: ManifestEntryKind,
        sync_name: purgery_core::SyncName,
        local_path: String,
        relative_path: String,
        error: String,
    },
    Skipped {
        kind: ManifestEntryKind,
        sync_name: purgery_core::SyncName,
        local_path: String,
        relative_path: String,
        error: String,
    },
}

impl EntryOutcome {
    fn into_entry(self) -> EntryStatusEntry {
        match self {
            EntryOutcome::Success {
                kind,
                sync_name,
                local_path,
                relative_path,
                final_paths,
                postprocess,
            } => EntryStatusEntry {
                kind,
                sync_name,
                local_path,
                relative_path,
                status: FileStatus::Imported,
                final_paths,
                postprocess,
                error: None,
            },
            EntryOutcome::Failure {
                kind,
                sync_name,
                local_path,
                relative_path,
                error,
            } => EntryStatusEntry {
                kind,
                sync_name,
                local_path,
                relative_path,
                status: FileStatus::Failed,
                final_paths: vec![],
                postprocess: None,
                error: Some(error),
            },
            EntryOutcome::Skipped {
                kind,
                sync_name,
                local_path,
                relative_path,
                error,
            } => EntryStatusEntry {
                kind,
                sync_name,
                local_path,
                relative_path,
                status: FileStatus::Skipped,
                final_paths: vec![],
                postprocess: None,
                error: Some(error),
            },
        }
    }
}

/// Result of committing one final tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitDisposition {
    Created,
    Kept,
    Replaced,
}

fn remove_destination_for_non_directory(
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

fn remove_stale_temp(temp_path: &Utf8Path) -> Result<(), String> {
    match fs::symlink_metadata(temp_path.as_std_path()) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(temp_path.as_std_path())
            .map_err(|error| format!("failed to remove stale temporary directory: {error}")),
        Ok(_) => fs::remove_file(temp_path.as_std_path())
            .map_err(|error| format!("failed to remove stale temporary entry: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to inspect temporary entry: {error}")),
    }
}

/// Verify that every existing final-path ancestor is a real directory.
/// Source directory entries are processed before their descendants, so type
/// conflicts in ancestors are resolved by those directory entries instead of
/// following a destination symlink or treating a file as a directory.
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
                fs::create_dir(current.as_std_path()).map_err(|error| {
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

fn commit_directory_entry(
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

fn commit_regular_file_entry(
    source: &Utf8Path,
    final_path: &Utf8Path,
    root: &Utf8Path,
    run_id: &RunId,
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

fn commit_symlink_entry(
    target: &Utf8Path,
    final_path: &Utf8Path,
    root: &Utf8Path,
    run_id: &RunId,
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

/// Recursively overlay a source directory tree onto final storage with
/// no-delete semantics: directories are created/kept, files and symlinks
/// are committed, existing unrelated descendants are preserved.
fn commit_directory_tree(
    source_root: &Utf8Path,
    final_root: &Utf8Path,
    server_root: &Utf8Path,
    run_id: &RunId,
) -> Result<CommitDisposition, String> {
    use walkdir::WalkDir;

    // Commit the root directory first
    let root_disp = commit_directory_entry(final_root, server_root)?;

    // Walk the source tree in breadth-first order so parents come before children
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

/// Prepare a work-area entry from its staged source.
/// Returns the work-area path.
fn prepare_work_entry(
    entry: &ManifestEntry,
    sync: &RunConfigSync,
    source_path: &Utf8Path,
    work_area: &Utf8Path,
) -> Result<Utf8PathBuf, String> {
    let work_path = work_area
        .join(sync.to_path.as_str())
        .join(entry.relative_path.as_str());

    match entry.kind {
        ManifestEntryKind::Directory => {
            if let Some(parent) = work_path.parent() {
                fs::create_dir_all(parent.as_std_path())
                    .map_err(|e| format!("failed to create work parent: {e}"))?;
            }
            // Recursively copy staged directory subtree into work area
            use walkdir::WalkDir;
            fs::create_dir(work_path.as_std_path())
                .map_err(|e| format!("failed to create work directory: {e}"))?;
            for dir_entry in WalkDir::new(source_path.as_std_path())
                .min_depth(1)
                .sort_by_file_name()
            {
                let dir_entry =
                    dir_entry.map_err(|e| format!("failed to walk staged directory: {e}"))?;
                let relative = dir_entry
                    .path()
                    .strip_prefix(source_path.as_std_path())
                    .map_err(|_| "failed to compute relative path".to_string())?;
                let work_child = work_path.join(Utf8Path::new(
                    relative
                        .to_str()
                        .ok_or_else(|| "non-UTF-8 path in staged directory".to_string())?,
                ));
                let meta = fs::symlink_metadata(dir_entry.path())
                    .map_err(|e| format!("failed to read staged entry metadata: {e}"))?;
                if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                    fs::create_dir(work_child.as_std_path())
                        .map_err(|e| format!("failed to create work subdirectory: {e}"))?;
                } else if meta.file_type().is_file() {
                    if let Some(parent) = work_child.parent() {
                        fs::create_dir_all(parent.as_std_path())
                            .map_err(|e| format!("failed to create work parent: {e}"))?;
                    }
                    fs::copy(dir_entry.path(), work_child.as_std_path())
                        .map_err(|e| format!("failed to copy staged file to work area: {e}"))?;
                } else if meta.file_type().is_symlink() {
                    if let Some(parent) = work_child.parent() {
                        fs::create_dir_all(parent.as_std_path())
                            .map_err(|e| format!("failed to create work parent: {e}"))?;
                    }
                    let target = fs::read_link(dir_entry.path())
                        .map_err(|e| format!("failed to read staged symlink: {e}"))?;
                    std::os::unix::fs::symlink(&target, work_child.as_std_path())
                        .map_err(|e| format!("failed to create work symlink: {e}"))?;
                } else {
                    return Err(format!(
                        "unsupported filesystem object in staged directory: {}",
                        relative.display()
                    ));
                }
            }
            Ok(work_path)
        }
        ManifestEntryKind::RegularFile => {
            if let Some(parent) = work_path.parent() {
                fs::create_dir_all(parent.as_std_path())
                    .map_err(|e| format!("failed to create work parent: {e}"))?;
            }
            fs::copy(source_path.as_std_path(), work_path.as_std_path())
                .map_err(|e| format!("failed to copy to work area: {e}"))?;
            Ok(work_path)
        }
        ManifestEntryKind::Symlink => {
            if let Some(parent) = work_path.parent() {
                fs::create_dir_all(parent.as_std_path())
                    .map_err(|e| format!("failed to create work parent: {e}"))?;
            }
            let target = fs::read_link(source_path.as_std_path())
                .map_err(|e| format!("failed to read staged symlink: {e}"))?;
            std::os::unix::fs::symlink(&target, work_path.as_std_path())
                .map_err(|e| format!("failed to create work symlink: {e}"))?;
            Ok(work_path)
        }
    }
}

/// Commit an output entry (determined by its filesystem kind) from the work
/// area to final storage.
fn commit_output_entry(
    source: &Utf8Path,
    final_path: &Utf8Path,
    server_root: &Utf8Path,
    run_id: &RunId,
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

fn failed_entry(entry: &ManifestEntry, error: impl Into<String>) -> EntryOutcome {
    EntryOutcome::Failure {
        kind: entry.kind,
        sync_name: entry.sync_name.clone(),
        local_path: entry.local_path.as_str().to_owned(),
        relative_path: entry.relative_path.as_str().to_owned(),
        error: error.into(),
    }
}

/// Compute all planned final paths for a manifest entry, including
/// postprocess-derived outputs. This is used by `validate_unique_final_paths`
/// to detect collisions between direct entry paths and postprocess outputs
/// before any files are processed.
fn planned_entry_outputs(
    server_config: &ServerConfig,
    nickname: &Nickname,
    sync: &RunConfigSync,
    entry: &ManifestEntry,
    run_plan: &RunPlan,
) -> Vec<String> {
    let entry_final_path =
        server_config
            .root
            .final_path(nickname, &sync.to_path, &entry.relative_path);

    // Every manifest entry kind uses the same postprocess-dispatch logic.
    // If no rule matches, the only planned output is the entry's own final
    // path.  If a rule matches, include keep_original and expected_outputs.
    let normalized_path = entry.relative_path.as_str().to_owned();

    let synthetic_work_path = Utf8Path::new(entry.relative_path.as_str());

    let mut any_rule_matched = false;
    let mut outputs: Vec<String> = Vec::new();

    for rule in &run_plan.rules {
        if !rule.is_match(&normalized_path) {
            continue;
        }
        any_rule_matched = true;
        for step in &rule.steps {
            if step.step_def.keep_original {
                outputs.push(entry_final_path.as_str().to_owned());
            }
            for pat in &step.step_def.expected_outputs {
                // RunPlan::build already validates expected-output names,
                // so an invalid name here is a logic error — skip defensively.
                if purgery_core::validate_expected_output_name(pat).is_err() {
                    continue;
                }
                let resolved = step.step_def.resolve_placeholders(synthetic_work_path, pat);
                let p = Utf8Path::new(&resolved);
                let fname = p.file_name().unwrap_or(resolved.as_str());
                let output_path = entry_final_path
                    .parent()
                    .map(|parent| parent.join(fname))
                    .unwrap_or_else(|| Utf8PathBuf::from(fname));
                outputs.push(output_path.as_str().to_owned());
            }
        }
    }

    if !any_rule_matched {
        outputs.push(entry_final_path.as_str().to_owned());
    }

    let mut seen = std::collections::HashSet::new();
    outputs.retain(|p| seen.insert(p.clone()));
    outputs
}

/// Validate that no two manifest entries or their postprocess outputs
/// resolve to the same final path on disk.
///
/// This must be called after `RunPlan::build` and before any entry
/// processing, so that collisions are caught before any filesystem
/// mutations.
fn validate_unique_final_paths(
    server_config: &ServerConfig,
    nickname: &Nickname,
    run_config: &RunConfig,
    manifest: &Manifest,
    run_plan: &RunPlan,
    covered_indices: &std::collections::HashSet<usize>,
) -> Result<(), String> {
    let sync_map: HashMap<&str, &RunConfigSync> = run_config.sync_map().into_iter().collect();
    let mut destinations: HashMap<String, &ManifestEntry> = HashMap::new();

    // First pass: collect planned destinations over active entries only.
    for (entry_idx, entry) in manifest.entries.iter().enumerate() {
        if covered_indices.contains(&entry_idx) {
            continue; // covered entries are not active processing units
        }
        let Some(sync) = sync_map.get(entry.sync_name.as_str()) else {
            continue;
        };

        let planned = planned_entry_outputs(server_config, nickname, sync, entry, run_plan);
        for destination in &planned {
            if let Some(previous) = destinations.insert(destination.clone(), entry) {
                return Err(format!(
                    "duplicate final path '{}' from '{}:{}' and '{}:{}'",
                    destination,
                    previous.sync_name.as_str(),
                    previous.relative_path.as_str(),
                    entry.sync_name.as_str(),
                    entry.relative_path.as_str()
                ));
            }
        }
    }

    // Second pass: detect subtree overlaps between postprocessed entries and
    // other active entries.  A postprocessed entry's output (which may be a
    // directory) must not overlap with another active entry's planned root.
    // Normal non-postprocessed parent-child directory relationships are
    // allowed.
    let entries_with_rules: std::collections::HashSet<usize> = manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            sync_map
                .get(e.sync_name.as_str())
                .map(|_sync| {
                    let np = e.relative_path.as_str().to_owned();
                    run_plan.rules.iter().any(|r| r.is_match(&np))
                })
                .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();

    for (i, entry_a) in manifest.entries.iter().enumerate() {
        // Skip covered entries — they are not active processing units.
        if covered_indices.contains(&i) {
            continue;
        }
        let Some(sync_a) = sync_map.get(entry_a.sync_name.as_str()) else {
            continue;
        };
        let planned_a = planned_entry_outputs(server_config, nickname, sync_a, entry_a, run_plan);
        for dest_a in &planned_a {
            for (j, entry_b) in manifest.entries.iter().enumerate() {
                if i == j || covered_indices.contains(&j) {
                    continue;
                }
                // Only flag overlaps if at least one of the two entries has
                // matching postprocess rules (otherwise it is a normal
                // parent-child directory relationship).
                if !entries_with_rules.contains(&i) && !entries_with_rules.contains(&j) {
                    continue;
                }
                let Some(sync_b) = sync_map.get(entry_b.sync_name.as_str()) else {
                    continue;
                };
                let planned_b =
                    planned_entry_outputs(server_config, nickname, sync_b, entry_b, run_plan);
                for dest_b in &planned_b {
                    if dest_a == dest_b {
                        // Already caught by exact-duplicate pass above.
                        continue;
                    }
                    // Check if one is an ancestor of the other (subtree overlap).
                    let a_prefix_of_b = dest_b.as_str().starts_with(dest_a.as_str())
                        && dest_b.as_str().len() > dest_a.as_str().len()
                        && dest_b.as_str().as_bytes().get(dest_a.as_str().len()) == Some(&b'/');
                    let b_prefix_of_a = dest_a.as_str().starts_with(dest_b.as_str())
                        && dest_a.as_str().len() > dest_b.as_str().len()
                        && dest_a.as_str().as_bytes().get(dest_b.as_str().len()) == Some(&b'/');
                    if a_prefix_of_b || b_prefix_of_a {
                        return Err(format!(
                            "planned output subtree overlap between '{}:{}' ({}) and \
                             '{}:{}' ({})",
                            entry_a.sync_name.as_str(),
                            entry_a.relative_path.as_str(),
                            dest_a,
                            entry_b.sync_name.as_str(),
                            entry_b.relative_path.as_str(),
                            dest_b,
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Validate and import one manifest entry using recursive no-delete overlay semantics.
#[allow(clippy::too_many_arguments)]
fn process_manifest_entry(
    server_config: &ServerConfig,
    run_plan: &RunPlan,
    sync: &RunConfigSync,
    entry: &ManifestEntry,
    nickname: &Nickname,
    run_id: &RunId,
    processing_path: &Utf8Path,
    work_area: &Utf8Path,
) -> EntryOutcome {
    let expected_staged = Utf8Path::new("files")
        .join(sync.to_path.as_str())
        .join(entry.relative_path.as_str());
    let Ok(expected_staged) = NormalizedRelativePath::new(expected_staged) else {
        return failed_entry(entry, "failed to normalize expected staged path");
    };
    if entry.staged_path != expected_staged {
        return failed_entry(
            entry,
            format!(
                "staged_path mismatch: expected '{}', got '{}'",
                expected_staged.as_str(),
                entry.staged_path.as_str()
            ),
        );
    }

    let source_path = processing_path.join(entry.staged_path.as_str());
    let staged_metadata = match fs::symlink_metadata(source_path.as_std_path()) {
        Ok(metadata) => metadata,
        Err(error) => {
            return failed_entry(entry, format!("failed to read staged metadata: {error}"))
        }
    };
    let staged_type = staged_metadata.file_type();
    let kind_matches = match entry.kind {
        ManifestEntryKind::Directory => staged_type.is_dir() && !staged_type.is_symlink(),
        ManifestEntryKind::RegularFile => staged_type.is_file(),
        ManifestEntryKind::Symlink => staged_type.is_symlink(),
    };
    if !kind_matches {
        return failed_entry(entry, "staged filesystem kind does not match manifest kind");
    }
    match entry.kind {
        ManifestEntryKind::RegularFile => {
            if let Err(error) = entry.verify_staged(&source_path) {
                return failed_entry(entry, format!("staged file identity check failed: {error}"));
            }
        }
        ManifestEntryKind::Symlink => {
            let Some(expected_target) = entry.link_target.as_deref() else {
                return failed_entry(entry, "symlink manifest entry has no link_target");
            };
            match fs::read_link(source_path.as_std_path()) {
                Ok(actual) if actual == expected_target.as_std_path() => {}
                Ok(_) => {
                    return failed_entry(entry, "staged symlink target does not match manifest")
                }
                Err(error) => {
                    return failed_entry(entry, format!("failed to read staged symlink: {error}"))
                }
            }
        }
        ManifestEntryKind::Directory => {}
    }

    let final_path = server_config
        .root
        .final_path(nickname, &sync.to_path, &entry.relative_path);
    if !path_is_within_root(&final_path, server_config.root.as_path()) {
        return failed_entry(
            entry,
            format!("final path escapes root: {}", final_path.as_str()),
        );
    }
    let final_relative = final_path
        .strip_prefix(server_config.root.as_path())
        .unwrap_or(&final_path)
        .to_string();
    let normalized_path = entry.relative_path.as_str().to_owned();

    // Check whether any postprocess rule matches this entry.  If not, commit
    // directly using the kind-specific path (no work-area overhead).
    let matched = run_plan
        .rules
        .iter()
        .any(|rule| rule.is_match(&normalized_path));

    let result = if !matched {
        // Direct commit — no postprocessing.
        match entry.kind {
            ManifestEntryKind::Directory => {
                commit_directory_entry(&final_path, server_config.root.as_path())
                    .map(|_| (vec![final_relative], None))
            }
            ManifestEntryKind::Symlink => {
                let target = entry.link_target.as_deref().expect("validated target");
                commit_symlink_entry(target, &final_path, server_config.root.as_path(), run_id)
                    .map(|_| (vec![final_relative], None))
            }
            ManifestEntryKind::RegularFile => {
                // Commit directly from staged source to final — no
                // work-area copy needed when no rule matched.
                commit_regular_file_entry(
                    &source_path,
                    &final_path,
                    server_config.root.as_path(),
                    run_id,
                )
                .map(|_| (vec![final_relative], None))
            }
        }
    } else {
        // Entry matches a postprocess rule — place in work area, run
        // subprocesses, commit outputs by their detected filesystem kind.
        let work_path = match prepare_work_entry(entry, sync, &source_path, work_area) {
            Ok(p) => p,
            Err(error) => return failed_entry(entry, error),
        };
        match apply_postprocessing(run_plan, &normalized_path, &work_path) {
            Ok(outputs) => {
                let mut final_paths = Vec::new();
                for output in outputs {
                    let output_final = if output == work_path {
                        final_path.clone()
                    } else {
                        let filename = output.file_name().unwrap_or("");
                        final_path.parent().map_or_else(
                            || Utf8PathBuf::from(filename),
                            |parent| parent.join(filename),
                        )
                    };
                    if !path_is_within_root(&output_final, server_config.root.as_path()) {
                        return failed_entry(entry, "output escapes root");
                    }
                    if let Err(error) = commit_output_entry(
                        &output,
                        &output_final,
                        server_config.root.as_path(),
                        run_id,
                    ) {
                        return failed_entry(entry, format!("commit failed: {error}"));
                    }
                    final_paths.push(
                        output_final
                            .strip_prefix(server_config.root.as_path())
                            .unwrap_or(&output_final)
                            .to_string(),
                    );
                }
                let steps: Vec<String> = run_plan
                    .rules
                    .iter()
                    .filter(|rule| rule.is_match(&normalized_path))
                    .flat_map(|rule| rule.steps.iter().map(|step| step.step_name.clone()))
                    .collect();
                Ok((final_paths, (!steps.is_empty()).then_some(steps)))
            }
            Err(error) => Err(error),
        }
    };

    match result {
        Ok((final_paths, postprocess)) => EntryOutcome::Success {
            kind: entry.kind,
            sync_name: entry.sync_name.clone(),
            local_path: entry.local_path.as_str().to_owned(),
            relative_path: entry.relative_path.as_str().to_owned(),
            final_paths,
            postprocess,
        },
        Err(error) => failed_entry(entry, error),
    }
}

/// Move a processing run to the terminal phase represented by its status.
fn finalize_processing_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    state: &RunState,
) -> Result<()> {
    let processing_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Processing);
    let dest_phase = match state {
        RunState::Done | RunState::Partial => RunPhase::Done,
        RunState::Failed => RunPhase::Failed,
    };
    let dest_path = config.purgery_root.run_dir(nickname, run_id, dest_phase);
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create {} parent: {}",
                dest_phase.as_str(),
                parent.as_str()
            )
        })?;
    }
    fs::rename(&processing_path, &dest_path).with_context(|| {
        format!(
            "failed to move run to {}: {} -> {}",
            dest_phase.as_str(),
            processing_path.as_str(),
            dest_path.as_str()
        )
    })?;
    Ok(())
}

/// Claim a ready run and process it from its durable processing directory.
pub fn process_ready_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    let ready_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Ready);
    let processing_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Processing);

    if let Some(parent) = processing_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create processing parent: {}", parent.as_str()))?;
    }

    fs::rename(&ready_path, &processing_path).with_context(|| {
        format!(
            "failed to claim run: {} -> {}",
            ready_path.as_str(),
            processing_path.as_str()
        )
    })?;

    process_processing_run(config, nickname, run_id)
}

/// Process a claimed run entirely from filesystem state in `processing/`.
pub fn process_processing_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<()> {
    let _span = span!(Level::INFO, "run", nickname = %nickname.as_str(), run_id = %run_id.as_str())
        .entered();
    let processing_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Processing);

    // Every attempt rebuilds the work area from immutable staged files. This
    // makes an interrupted attempt replayable without hidden process state.
    let work_area = work_dir(config.root.as_path(), nickname, run_id);
    if let Err(error) = fs::remove_dir_all(&work_area) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = "processing",
                error = %error,
                "failed to clean stale work area"
            );
        }
    }
    fs::create_dir_all(&work_area)
        .with_context(|| format!("failed to create work area: {}", work_area.as_str()))?;

    let run_config_path = processing_path.join("run.toml");
    let run_config_content = match fs::read_to_string(&run_config_path) {
        Ok(content) => content,
        Err(error) => {
            let msg = format!("failed to read run config: {error}");
            warn!("{}", msg);
            write_run_failure(&config.purgery_root, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };
    let run_config = match RunConfig::from_toml(&run_config_content) {
        Ok(run_config) => run_config,
        Err(error) => {
            let msg = format!("failed to parse run config: {error}");
            warn!("{}", msg);
            write_run_failure(&config.purgery_root, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };

    let run_plan = match RunPlan::build(config, &run_config) {
        Ok(plan) => plan,
        Err(error) => {
            let msg = format!("run plan validation failed: {error}");
            warn!("{}", msg);
            write_run_failure(&config.purgery_root, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };

    let manifest_path = processing_path.join("manifest.toml");
    let manifest_content = match fs::read_to_string(&manifest_path) {
        Ok(content) => content,
        Err(error) => {
            let msg = format!("failed to read manifest: {error}");
            warn!("{}", msg);
            write_run_failure(&config.purgery_root, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };
    let manifest = match Manifest::from_toml(&manifest_content) {
        Ok(manifest) => manifest,
        Err(error) => {
            let msg = format!("failed to parse manifest: {error}");
            warn!("{}", msg);
            write_run_failure(&config.purgery_root, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };

    if let Err(error) = validate_envelope(nickname, run_id, &run_config, &manifest) {
        let msg = format!("envelope validation failed: {error}");
        warn!("{}", msg);
        write_run_failure(&config.purgery_root, nickname, run_id, &msg)?;
        anyhow::bail!("{msg}");
    }

    let sync_map: HashMap<&str, &RunConfigSync> = run_config.sync_map().into_iter().collect();

    // --- Phase 1: Coverage pre-pass ---
    // Identify directories whose normalized path matches a postprocess rule.
    // Those directories become transformation boundaries — their descendants
    // are covered/skipped.
    let covered_by_dir: std::collections::HashSet<String> = manifest
        .entries
        .iter()
        .filter(|e| e.kind == ManifestEntryKind::Directory)
        .filter_map(|dir_entry| {
            let _sync = sync_map.get(dir_entry.sync_name.as_str())?;
            let np = dir_entry.relative_path.as_str().to_owned();
            let matched = run_plan.rules.iter().any(|rule| rule.is_match(&np));
            if matched {
                Some(np)
            } else {
                None
            }
        })
        .collect();

    // Determine which entries are covered (descendants of a postprocessed directory).
    let covered_indices: std::collections::HashSet<usize> = manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            let Some(_sync) = sync_map.get(entry.sync_name.as_str()) else {
                return false;
            };
            let np = entry.relative_path.as_str().to_owned();
            covered_by_dir
                .iter()
                .any(|prefix| match np.as_str().strip_prefix(prefix.as_str()) {
                    Some(tail) => tail.starts_with('/'),
                    None => false,
                })
        })
        .map(|(i, _)| i)
        .collect();

    // --- Phase 2: Planned-path validation over active (non-covered) entries ---
    // This must happen before any final-storage mutation (sync-root setup).
    if let Err(error) = validate_unique_final_paths(
        config,
        nickname,
        &run_config,
        &manifest,
        &run_plan,
        &covered_indices,
    ) {
        let msg = format!("manifest destination validation failed: {error}");
        warn!("{}", msg);
        write_run_failure(&config.purgery_root, nickname, run_id, &msg)?;
        anyhow::bail!("{msg}");
    }

    // --- Phase 3: Sync-root setup for used syncs only ---
    // This mutates final storage, so it must run after planned-path validation
    // has confirmed the run is semantically valid.
    let used_sync_names: std::collections::HashSet<&str> = manifest
        .entries
        .iter()
        .map(|e| e.sync_name.as_str())
        .collect();

    let mut failed_sync_roots: std::collections::HashSet<String> = std::collections::HashSet::new();

    for sync in run_config.sync.iter() {
        if !used_sync_names.contains(sync.name.as_str()) {
            continue;
        }
        let root_dir = config
            .root
            .as_path()
            .join(nickname.as_str())
            .join(sync.to_path.as_str());
        if let Err(error) = commit_directory_entry(&root_dir, config.root.as_path()) {
            warn!(
                sync_name = %sync.name.as_str(),
                path = %root_dir.as_str(),
                error = %error,
                "sync root directory setup failed"
            );
            failed_sync_roots.insert(sync.name.as_str().to_owned());
        }
    }

    // --- Phase 4: Process entries ---
    let mut outcomes: Vec<EntryOutcome> = Vec::new();

    for entry in &manifest.entries {
        let sync_name = entry.sync_name.as_str();
        let Some(sync) = sync_map.get(sync_name) else {
            warn!(
                sync_name = sync_name,
                "sync mapping not found in run config, skipping"
            );
            outcomes.push(EntryOutcome::Skipped {
                kind: entry.kind,
                sync_name: entry.sync_name.clone(),
                local_path: entry.local_path.as_str().to_owned(),
                relative_path: entry.relative_path.as_str().to_owned(),
                error: format!("sync mapping '{sync_name}' not found"),
            });
            continue;
        };

        // Check if this entry's sync root setup failed.
        if failed_sync_roots.contains(sync.name.as_str()) {
            outcomes.push(EntryOutcome::Skipped {
                kind: entry.kind,
                sync_name: entry.sync_name.clone(),
                local_path: entry.local_path.as_str().to_owned(),
                relative_path: entry.relative_path.as_str().to_owned(),
                error: format!("sync root setup failed for '{}'", sync.to_path.as_str()),
            });
            continue;
        }

        // Check if this entry is covered by a postprocessed ancestor directory.
        let np = entry.relative_path.as_str().to_owned();
        let covered = covered_by_dir.iter().any(|prefix| {
            let tail = match np.as_str().strip_prefix(prefix.as_str()) {
                Some(t) => t,
                None => return false,
            };
            tail.starts_with('/')
        });
        if covered {
            debug!(sync_name = sync_name, path = %np, "entry covered by postprocessed ancestor directory, skipping");
            outcomes.push(EntryOutcome::Skipped {
                kind: entry.kind,
                sync_name: entry.sync_name.clone(),
                local_path: entry.local_path.as_str().to_owned(),
                relative_path: entry.relative_path.as_str().to_owned(),
                error: "covered by postprocessed ancestor directory".into(),
            });
            continue;
        }

        // Passthrough entries are imported by the client directly to final
        // storage via rsync.  Status is derived from the passthrough receipt,
        // not from staged content verification.  No receipt means the entry
        // was not imported — it must not be processed from staging.
        if entry.mode == purgery_core::ManifestEntryMode::Passthrough {
            let receipt_path = processing_path.join(format!("passthrough.{sync_name}.toml"));
            let receipt_ok = receipt_path.exists()
                && fs::read_to_string(&receipt_path)
                    .ok()
                    .and_then(|c| toml::from_str::<purgery_core::PassthroughReceipt>(&c).ok())
                    .map(|r| r.status == "imported")
                    .unwrap_or(false);

            if receipt_ok {
                let final_path =
                    config
                        .root
                        .final_path(nickname, &sync.to_path, &entry.relative_path);
                let final_relative = final_path
                    .strip_prefix(config.root.as_path())
                    .unwrap_or(&final_path)
                    .to_string();
                outcomes.push(EntryOutcome::Success {
                    kind: entry.kind,
                    sync_name: entry.sync_name.clone(),
                    local_path: entry.local_path.as_str().to_owned(),
                    relative_path: entry.relative_path.as_str().to_owned(),
                    final_paths: vec![final_relative],
                    postprocess: None,
                });
            } else {
                outcomes.push(EntryOutcome::Skipped {
                    kind: entry.kind,
                    sync_name: entry.sync_name.clone(),
                    local_path: entry.local_path.as_str().to_owned(),
                    relative_path: entry.relative_path.as_str().to_owned(),
                    error: "passthrough receipt missing or failed".into(),
                });
            }
            continue;
        }

        outcomes.push(process_manifest_entry(
            config,
            &run_plan,
            sync,
            entry,
            nickname,
            run_id,
            &processing_path,
            &work_area,
        ));
    }

    let all_imported = outcomes
        .iter()
        .all(|outcome| matches!(outcome, EntryOutcome::Success { .. }));
    let any_imported = outcomes
        .iter()
        .any(|outcome| matches!(outcome, EntryOutcome::Success { .. }));
    let run_state = if all_imported {
        RunState::Done
    } else if any_imported {
        RunState::Partial
    } else {
        RunState::Failed
    };

    if run_state == RunState::Done {
        let _ = fs::remove_dir_all(&work_area);
    }

    info!(state = %run_state.as_str(), "run complete");
    let run_status = RunStatus {
        run_id: run_id.clone(),
        nickname: nickname.clone(),
        state: run_state.clone(),
        entries: outcomes.into_iter().map(EntryOutcome::into_entry).collect(),
        error: None,
    };
    let status_toml = run_status
        .to_toml()
        .with_context(|| "failed to serialize status")?;
    let status_path = processing_path.join("status.toml");
    let status_tmp_path = processing_path.join("status.toml.tmp");
    fs::write(&status_tmp_path, &status_toml).with_context(|| {
        format!(
            "failed to write temporary status: {}",
            status_tmp_path.as_str()
        )
    })?;
    fs::rename(&status_tmp_path, &status_path)
        .with_context(|| format!("failed to finalize status: {}", status_path.as_str()))?;

    finalize_processing_run(config, nickname, run_id, &run_state)
}

/// Recover a processing run or finalize its pending terminal transition.
pub fn recover_or_process_processing_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<()> {
    let processing_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Processing);
    let status_path = processing_path.join("status.toml");

    match fs::read_to_string(&status_path) {
        Ok(content) => match RunStatus::from_toml(&content) {
            Ok(status) if status.nickname != *nickname || status.run_id != *run_id => {
                let error = "interrupted processing had mismatched status envelope";
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    status_nickname = %status.nickname.as_str(),
                    status_run_id = %status.run_id.as_str(),
                    phase = "processing",
                    run_status = "failed",
                    recovery_action = "replace_mismatched_status",
                    error,
                    "processing run recovery failed"
                );
                write_run_failure(&config.purgery_root, nickname, run_id, error)
            }
            Ok(status) => {
                info!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    phase = "processing",
                    run_status = %status.state.as_str(),
                    recovery_action = "finalize_terminal_move",
                    "processing run had valid status, finalizing terminal move"
                );
                finalize_processing_run(config, nickname, run_id, &status.state)?;
                info!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    phase = "processing",
                    run_status = %status.state.as_str(),
                    recovery_action = "terminal_move_complete",
                    "processing run recovered"
                );
                Ok(())
            }
            Err(_) => {
                let error = "interrupted processing had malformed status";
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    phase = "processing",
                    run_status = "failed",
                    recovery_action = "replace_malformed_status",
                    error,
                    "processing run recovery failed"
                );
                write_run_failure(&config.purgery_root, nickname, run_id, error)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = "processing",
                recovery_action = "replay_staged_files",
                "processing run interrupted, replaying from staged files"
            );
            process_processing_run(config, nickname, run_id)?;
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = "processing",
                recovery_action = "replay_complete",
                "processing run recovered"
            );
            Ok(())
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to read processing status: {}", status_path.as_str())),
    }
}

/// Process a ready run. Kept as the public single-run entry point.
pub fn process_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    process_ready_run(config, nickname, run_id)
}

/// A compiled postprocess rule with resolved step definitions.
#[derive(Debug)]
pub struct CompiledRule {
    pub pattern: String,
    pub steps: Vec<ResolvedStep>,
}

impl CompiledRule {
    /// Returns true if the normalized path matches this rule's rsync pattern.
    pub fn is_match(&self, normalized_path: &str) -> bool {
        purgery_core::rsync_pattern_match(&self.pattern, normalized_path)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedStep {
    pub step_name: String,
    pub step_def: purgery_core::PostprocessStepDefinition,
}

/// A validated run plan: precompiled rsync patterns and resolved step definitions.
#[derive(Debug)]
pub struct RunPlan {
    pub rules: Vec<CompiledRule>,
}

impl RunPlan {
    /// Build a run plan from server config and run config.
    ///
    /// Validates all patterns and step references. Returns an error
    /// (suitable for run-level failure) if anything is invalid.
    pub fn build(
        server_config: &ServerConfig,
        run_config: &purgery_core::RunConfig,
    ) -> Result<Self, String> {
        let mut rules = Vec::new();

        for rule in &run_config.postprocess.rules {
            if rule.pattern.is_empty() {
                return Err("postprocess rule has empty pattern".into());
            }

            let mut steps = Vec::new();
            for step_name in &rule.steps {
                let Some(def) = server_config.postprocess.steps.get(step_name.as_str()) else {
                    return Err(format!(
                        "postprocess step '{step_name}' referenced by rule is not defined on server"
                    ));
                };

                // Validate expected_outputs patterns at plan-build time so
                // that bad server configuration is caught before entry
                // processing mutates final storage.
                for output in &def.expected_outputs {
                    purgery_core::validate_expected_output_name(output).map_err(|e| {
                        format!("postprocess step '{step_name}': expected_output {output:?}: {e}")
                    })?;
                }

                if !def.keep_original && def.expected_outputs.is_empty() {
                    return Err(format!(
                        "postprocess step '{step_name}': keep_original=false with no \
                         expected_outputs would produce zero committed outputs"
                    ));
                }

                steps.push(ResolvedStep {
                    step_name: step_name.clone(),
                    step_def: def.clone(),
                });
            }

            rules.push(CompiledRule {
                pattern: rule.pattern.clone(),
                steps,
            });
        }

        Ok(RunPlan { rules })
    }
}

/// Apply postprocessing rules to an entry root in the work area using a precompiled RunPlan.
///
/// `normalized_path` is the logical path used for rule matching (e.g. `videos/video.mp4`).
/// `work_path` is the absolute work area path used for subprocess execution.
/// Returns the list of work area paths to commit, deduplicated and ordered.
pub fn apply_postprocessing(
    run_plan: &RunPlan,
    normalized_path: &str,
    work_path: &Utf8Path,
) -> Result<Vec<Utf8PathBuf>, String> {
    let mut results: Vec<Utf8PathBuf> = Vec::new();
    let mut any_rule_matched = false;

    let work_parent = work_path
        .parent()
        .ok_or_else(|| "work path has no parent directory".to_string())?;

    for compiled in &run_plan.rules {
        if !compiled.is_match(normalized_path) {
            continue;
        }
        any_rule_matched = true;

        for step in &compiled.steps {
            let step_def = &step.step_def;

            match step_def.kind {
                purgery_core::PostprocessKind::Subprocess => {
                    let args = step_def.build_args(work_path);
                    info!(step = %step.step_name, program = %step_def.program, "running postprocess step");

                    let status = std::process::Command::new(&step_def.program)
                        .args(&args)
                        .status()
                        .map_err(|e| format!("failed to run {}: {e}", step.step_name))?;

                    if !status.success() {
                        return Err(format!(
                            "{} failed with exit code {:?}",
                            step.step_name,
                            status.code()
                        ));
                    }

                    // Check expected outputs exist and are within the work area
                    let expected = step_def
                        .resolve_expected_outputs(work_path)
                        .map_err(|e| format!("{}: {e}", step.step_name))?;
                    for exp in &expected {
                        if !exp.starts_with(work_parent) {
                            return Err(format!(
                                "expected output '{}' is outside work area '{}'",
                                exp.as_str(),
                                work_parent.as_str()
                            ));
                        }
                        let metadata =
                            fs::symlink_metadata(exp.as_std_path()).map_err(|error| {
                                if error.kind() == std::io::ErrorKind::NotFound {
                                    format!("expected output not found: {}", exp.as_str())
                                } else {
                                    format!(
                                        "failed to inspect expected output '{}': {error}",
                                        exp.as_str()
                                    )
                                }
                            })?;
                        let file_type = metadata.file_type();
                        let ok = file_type.is_dir() && !file_type.is_symlink()
                            || file_type.is_file()
                            || file_type.is_symlink();
                        if !ok {
                            return Err(format!(
                                "expected output is not a supported entry type: {}",
                                exp.as_str()
                            ));
                        }
                    }

                    if step_def.keep_original {
                        results.push(work_path.to_owned());
                    }
                    results.extend(expected);
                }
            }
        }
    }

    if !any_rule_matched {
        // No matching rules, just commit the original
        results.push(work_path.to_owned());
    }

    // Deduplicate while preserving order
    {
        let mut seen = std::collections::HashSet::new();
        results.retain(|p| seen.insert(p.as_str().to_owned()));
    }

    if results.is_empty() {
        return Err("postprocessing produced zero outputs, but at least one is required".into());
    }

    Ok(results)
}

/// Move a failed run's directory from processing to failed.
pub fn move_to_failed(
    purgery_root: &PurgeryRoot,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<()> {
    let processing_path = purgery_root.run_dir(nickname, run_id, RunPhase::Processing);
    let failed_path = purgery_root.run_dir(nickname, run_id, RunPhase::Failed);

    if processing_path.exists() {
        if let Some(parent) = failed_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create failed parent: {}", parent.as_str()))?;
        }
        fs::rename(&processing_path, &failed_path).with_context(|| {
            format!(
                "failed to move run to failed: {} -> {}",
                processing_path.as_str(),
                failed_path.as_str()
            )
        })?;
    }

    Ok(())
}

/// Process once: run GC, recover processing runs, then claim ready runs.
pub fn process_once_raw(config: &ServerConfig) -> Result<()> {
    if let Err(error) = run_gc(config) {
        warn!(error = %error, "opportunistic GC failed");
    }

    let processing_runs = find_processing_runs(&config.purgery_root)?;
    let ready_runs = find_ready_runs(&config.purgery_root)?;
    if processing_runs.is_empty() && ready_runs.is_empty() {
        info!("no ready or processing runs found");
        return Ok(());
    }

    for (nickname, run_id) in &processing_runs {
        if let Err(error) = recover_or_process_processing_run(config, nickname, run_id) {
            warn!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = "processing",
                error = %error,
                "processing run recovery failed"
            );
            let processing_path =
                config
                    .purgery_root
                    .run_dir(nickname, run_id, RunPhase::Processing);
            if processing_path.exists() {
                write_run_failure(
                    &config.purgery_root,
                    nickname,
                    run_id,
                    &format!("processing recovery failed: {error}"),
                )?;
            }
        }
    }

    for (nickname, run_id) in &ready_runs {
        info!(
            nickname = %nickname.as_str(),
            run_id = %run_id.as_str(),
            phase = "ready",
            "processing run"
        );
        if let Err(error) = process_ready_run(config, nickname, run_id) {
            warn!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = "processing",
                error = %error,
                "run failed"
            );
            move_to_failed(&config.purgery_root, nickname, run_id)?;
        }
    }

    Ok(())
}

/// Server-side subcommand: begin a new run.
///
/// Creates the incoming directory and prints a machine-readable TOML
/// response with server-derived paths.
pub fn begin_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<String> {
    // Run GC opportunistically before creating the run
    if let Err(e) = run_gc(config) {
        warn!(error = %e, "opportunistic GC failed");
    }

    // Check run does not already exist in any phase
    let phases = [
        RunPhase::Incoming,
        RunPhase::Ready,
        RunPhase::Processing,
        RunPhase::Done,
        RunPhase::Failed,
    ];
    for phase in &phases {
        let phase_path = config.purgery_root.run_dir(nickname, run_id, *phase);
        if phase_path.exists() {
            anyhow::bail!(
                "run {}/{} already exists in '{}' phase at '{}'",
                nickname.as_str(),
                run_id.as_str(),
                phase.as_str(),
                phase_path.as_str()
            );
        }
    }

    let incoming_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Incoming);
    let files_dir = incoming_path.join("files");
    let run_config_path = incoming_path.join("run.toml");
    let manifest_path = incoming_path.join("manifest.toml");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Atomic single-use: create_dir fails atomically if the directory already exists.
    // This prevents two concurrent clients from both accepting the same run ID.
    if let Some(parent) = incoming_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create incoming parent: {}", parent.as_str()))?;
    }
    fs::create_dir(incoming_path.as_std_path()).with_context(|| {
        format!(
            "failed to create incoming dir '{}' (race: run may have been created concurrently)",
            incoming_path.as_str()
        )
    })?;
    if let Err(error) = fs::create_dir(files_dir.as_std_path()) {
        let _ = fs::remove_dir_all(&incoming_path);
        return Err(error).with_context(|| format!("failed to create files dir: {}", files_dir));
    }

    // Write lease file
    let lease = purgery_core::LeaseFile {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        created_at_unix_secs: now,
        last_heartbeat_unix_secs: now,
        expires_at_unix_secs: now + config.gc.incoming_lease_secs,
    };
    let lease_content =
        toml::to_string(&lease).map_err(|e| anyhow::anyhow!("failed to serialize lease: {e}"))?;
    let lease_tmp = incoming_path.join("lease.toml.tmp");
    let lease_write_result = (|| -> Result<()> {
        fs::write(lease_tmp.as_std_path(), &lease_content)?;
        fs::rename(
            lease_tmp.as_std_path(),
            incoming_path.join("lease.toml").as_std_path(),
        )?;
        Ok(())
    })();

    if let Err(e) = lease_write_result {
        let _ = fs::remove_dir_all(&incoming_path);
        return Err(e.context("failed to write lease file"));
    }

    let response = purgery_core::BeginRunResponse {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        incoming_dir: incoming_path.as_str().to_owned(),
        files_dir: files_dir.as_str().to_owned(),
        run_config_path: run_config_path.as_str().to_owned(),
        manifest_path: manifest_path.as_str().to_owned(),
        heartbeat_interval_secs: config.gc.heartbeat_interval_secs,
    };

    let response_str = toml::to_string(&response).map_err(|e| {
        let _ = fs::remove_dir_all(&incoming_path);
        anyhow::anyhow!("failed to serialize begin-run response: {e}")
    })?;
    Ok(response_str)
}

/// Server-side subcommand: finish a run by moving from incoming to ready.
/// Server-side subcommand: validate the run plan and return transfer destinations.
///
/// Must be called after the client has written `run.toml` and `manifest.toml`
/// into the incoming directory but before any rsync transfer.
/// This is the gate that prevents passthrough transfers into final storage
/// for an invalid run plan.
pub fn prepare_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<String> {
    let incoming_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Incoming);
    if !incoming_path.exists() {
        anyhow::bail!(
            "incoming directory does not exist for run {}/{} at '{}'",
            nickname.as_str(),
            run_id.as_str(),
            incoming_path.as_str()
        );
    }

    let run_config_path = incoming_path.join("run.toml");
    let run_config_content =
        fs::read_to_string(&run_config_path).with_context(|| "failed to read run config")?;
    let run_config = purgery_core::RunConfig::from_toml(&run_config_content)
        .with_context(|| "failed to parse run config")?;

    let manifest_path = incoming_path.join("manifest.toml");
    let manifest_content =
        fs::read_to_string(&manifest_path).with_context(|| "failed to read manifest")?;
    let manifest = purgery_core::Manifest::from_toml(&manifest_content)
        .with_context(|| "failed to parse manifest")?;

    // Validate envelope
    if let Err(e) = purgery_core::validate_envelope(nickname, run_id, &run_config, &manifest) {
        anyhow::bail!("envelope validation failed: {e}");
    }

    // Validate manifest classification
    {
        let sync_map = run_config.sync_map();
        for entry in &manifest.entries {
            let _sync = sync_map.get(entry.sync_name.as_str());
            let rp = entry.relative_path.as_str();
            let matched = run_config
                .postprocess
                .rules
                .iter()
                .find(|r| purgery_core::rsync_pattern_match(&r.pattern, rp));
            let expected_mode = match matched {
                Some(_) => purgery_core::ManifestEntryMode::Postprocess,
                None => purgery_core::ManifestEntryMode::Passthrough,
            };

            // Check for covered entries and validate covered_by
            let covering_ancestor = manifest.entries.iter().find(|de| {
                de.kind == purgery_core::ManifestEntryKind::Directory
                    && de.mode == purgery_core::ManifestEntryMode::Postprocess
                    && de.sync_name.as_str() == entry.sync_name.as_str()
                    && rp.starts_with(de.relative_path.as_str())
                    && rp.as_bytes().get(de.relative_path.as_str().len()) == Some(&b'/')
            });

            if let Some(ancestor) = covering_ancestor {
                let expected_covered_by = ancestor.relative_path.as_str();
                if entry.mode != purgery_core::ManifestEntryMode::Covered {
                    anyhow::bail!(
                        "classification mismatch: '{}' is a descendant of postprocessed \
                         directory but has mode '{:?}' instead of 'covered'",
                        rp,
                        entry.mode
                    );
                }
                if entry.covered_by.as_deref() != Some(expected_covered_by) {
                    anyhow::bail!(
                        "covered entry '{}' has covered_by {:?} but expected '{}'",
                        rp,
                        entry.covered_by,
                        expected_covered_by
                    );
                }
                if !entry.postprocess_steps.is_empty() {
                    anyhow::bail!(
                        "covered entry '{}' has non-empty postprocess_steps {:?}",
                        rp,
                        entry.postprocess_steps
                    );
                }
                continue;
            }

            if entry.mode != expected_mode {
                anyhow::bail!(
                    "classification mismatch for '{}': manifest says '{:?}' but \
                     pattern classification says '{:?}'",
                    rp,
                    entry.mode,
                    expected_mode
                );
            }

            if entry.mode == purgery_core::ManifestEntryMode::Postprocess {
                let Some(rule) = matched else {
                    anyhow::bail!(
                        "classification mismatch for '{}': postprocess mode but no matching rule",
                        rp
                    );
                };
                if entry.postprocess_steps != rule.steps {
                    anyhow::bail!(
                        "classification mismatch for '{}': postprocess_steps {:?} do not \
                         match rule steps {:?}",
                        rp,
                        entry.postprocess_steps,
                        rule.steps
                    );
                }
            }
        }
    }

    // Build run plan (validates patterns, step references, expected outputs)
    let run_plan = RunPlan::build(config, &run_config)
        .map_err(|e| anyhow::anyhow!("run plan validation failed: {e}"))?;

    // Compute covered descendants
    let sync_map = run_config.sync_map();
    let covered_by_dir: std::collections::HashSet<String> = manifest
        .entries
        .iter()
        .filter(|e| e.kind == purgery_core::ManifestEntryKind::Directory)
        .filter_map(|dir_entry| {
            let _sync = sync_map.get(dir_entry.sync_name.as_str())?;
            let np = dir_entry.relative_path.as_str().to_owned();
            let matched = run_plan.rules.iter().any(|rule| rule.is_match(&np));
            if matched {
                Some(np)
            } else {
                None
            }
        })
        .collect();

    // Validate planned final paths over active (non-covered) entries
    let sync_map2 = run_config.sync_map();
    let covered_indices: std::collections::HashSet<usize> = manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            let Some(_sync) = sync_map2.get(entry.sync_name.as_str()) else {
                return false;
            };
            let np2 = entry.relative_path.as_str().to_owned();
            covered_by_dir
                .iter()
                .any(|prefix| match np2.as_str().strip_prefix(prefix.as_str()) {
                    Some(tail) => tail.starts_with('/'),
                    None => false,
                })
        })
        .map(|(i, _)| i)
        .collect();

    validate_unique_final_paths(
        config,
        nickname,
        &run_config,
        &manifest,
        &run_plan,
        &covered_indices,
    )
    .map_err(|e| anyhow::anyhow!("destination validation failed: {e}"))?;

    // Build response with per-sync transfer destinations
    let final_root = config.root.as_path().join(nickname.as_str());
    let purgatory_root = incoming_path.join("files");
    let destinations: Vec<purgery_core::SyncDestination> = run_config
        .sync
        .iter()
        .map(|sync| {
            let passthrough_dest = final_root.join(sync.to_path.as_str());
            let purgatory_dest = purgatory_root.join(sync.to_path.as_str());
            purgery_core::SyncDestination {
                sync_name: sync.name.as_str().to_owned(),
                passthrough_dest: passthrough_dest.as_str().to_owned(),
                purgatory_dest: purgatory_dest.as_str().to_owned(),
            }
        })
        .collect();

    let response = purgery_core::PrepareRunResponse {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        destinations,
    };

    toml::to_string(&response)
        .map_err(|e| anyhow::anyhow!("failed to serialize prepare-run response: {e}"))
}

pub fn finish_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    let incoming_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Incoming);
    if !incoming_path.exists() {
        anyhow::bail!(
            "incoming directory does not exist for run {}/{} at '{}'",
            nickname.as_str(),
            run_id.as_str(),
            incoming_path.as_str()
        );
    }

    // Validate lease envelope before accepting the finish
    let lease_path = incoming_path.join("lease.toml");
    if lease_path.exists() {
        let lease_content =
            fs::read_to_string(&lease_path).with_context(|| "failed to read lease file")?;
        let lease: purgery_core::LeaseFile =
            toml::from_str(&lease_content).with_context(|| "failed to parse lease file")?;
        if lease.protocol_version != 1 {
            anyhow::bail!(
                "lease protocol version {} does not match expected 1",
                lease.protocol_version
            );
        }
        if lease.nickname != nickname.as_str() {
            anyhow::bail!(
                "lease nickname '{}' does not match expected '{}'",
                lease.nickname,
                nickname.as_str()
            );
        }
        if lease.run_id != run_id.as_str() {
            anyhow::bail!(
                "lease run_id '{}' does not match expected '{}'",
                lease.run_id,
                run_id.as_str()
            );
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now >= lease.expires_at_unix_secs {
            anyhow::bail!(
                "cannot finish run: incoming lease expired at {}",
                lease.expires_at_unix_secs
            );
        }
    } else {
        anyhow::bail!("cannot finish run: no lease file found, run may be incomplete");
    }

    let ready_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Ready);
    if ready_path.exists() {
        anyhow::bail!(
            "ready directory already exists for run {}/{} at '{}'",
            nickname.as_str(),
            run_id.as_str(),
            ready_path.as_str()
        );
    }

    if let Some(parent) = ready_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create ready parent: {}", parent.as_str()))?;
    }

    fs::rename(&incoming_path, &ready_path).with_context(|| {
        format!(
            "failed to move incoming to ready: {} -> {}",
            incoming_path.as_str(),
            ready_path.as_str()
        )
    })?;

    Ok(())
}

/// Server-side subcommand: read the run status from done or failed.
pub fn read_run_status(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStatus> {
    let phases = [RunPhase::Done, RunPhase::Failed];

    for phase in &phases {
        let status_path = config
            .purgery_root
            .run_dir(nickname, run_id, *phase)
            .join("status.toml");
        if !status_path.exists() {
            continue;
        }
        let content = fs::read_to_string(&status_path)
            .with_context(|| format!("failed to read status from '{}'", status_path.as_str()))?;
        match RunStatus::from_toml(&content) {
            Ok(status) => {
                if status.nickname != *nickname || status.run_id != *run_id {
                    anyhow::bail!(
                        "status envelope mismatch in '{}': expected {}/{}, got {}/{}",
                        status_path,
                        nickname.as_str(),
                        run_id.as_str(),
                        status.nickname.as_str(),
                        status.run_id.as_str()
                    );
                }
                return Ok(status);
            }
            Err(e) => {
                anyhow::bail!("malformed status file '{}': {e}", status_path.as_str());
            }
        }
    }

    anyhow::bail!(
        "status not found for run {}/{} in done or failed",
        nickname.as_str(),
        run_id.as_str()
    );
}

/// Side-effect-free server check: verify config and programs without creating anything.
pub fn server_check(config: &ServerConfig) -> Result<()> {
    info!("checking server configuration");

    // Validate GC config: heartbeat must be frequent enough to keep the lease alive
    if config.gc.incoming_lease_secs == 0 {
        anyhow::bail!("gc.incoming_lease_secs must be greater than 0");
    }
    if config.gc.heartbeat_interval_secs == 0 {
        anyhow::bail!("gc.heartbeat_interval_secs must be greater than 0");
    }
    if config.gc.heartbeat_interval_secs > config.gc.incoming_lease_secs / 2 {
        anyhow::bail!(
            "gc.heartbeat_interval_secs ({}) must be <= half of gc.incoming_lease_secs ({}) \
             to provide a safety margin for lease renewal",
            config.gc.heartbeat_interval_secs,
            config.gc.incoming_lease_secs
        );
    }

    // Check root path exists and is a directory
    let root_path = config.root.as_path();
    if !root_path.exists() {
        anyhow::bail!(
            "root path '{}' does not exist (run `purgery-server bootstrap` to create it)",
            root_path.as_str()
        );
    }
    if !root_path.is_dir() {
        anyhow::bail!(
            "root path '{}' exists but is not a directory",
            root_path.as_str()
        );
    }
    info!(path = %root_path.as_str(), "root: OK");

    // Check purgery_root path exists and is a directory
    let purgery_path = config.purgery_root.as_path();
    if !purgery_path.exists() {
        anyhow::bail!(
            "purgery_root '{}' does not exist (run `purgery-server bootstrap` to create it)",
            purgery_path.as_str()
        );
    }
    if !purgery_path.is_dir() {
        anyhow::bail!(
            "purgery_root '{}' exists but is not a directory",
            purgery_path.as_str()
        );
    }
    info!(path = %purgery_path.as_str(), "purgery_root: OK");

    // Check postprocess programs and validate step definitions
    for (name, step) in &config.postprocess.steps {
        let program = &step.program;
        if program.is_empty() {
            anyhow::bail!("postprocess step '{}' has empty program", name);
        }

        // Validate step produces at least one output
        if !step.keep_original && step.expected_outputs.is_empty() {
            anyhow::bail!(
                "postprocess step '{}': keep_original=false with no expected_outputs \
                 would produce zero committed outputs",
                name
            );
        }

        // Validate expected_outputs are plain file names
        for output in &step.expected_outputs {
            purgery_core::validate_expected_output_name(output).map_err(|e| {
                anyhow::anyhow!("postprocess step '{name}': expected_output {output:?}: {e}")
            })?;
        }

        purgery_core::resolve_executable(program)
            .map(|r| info!(step = name, path = %r.path.as_str(), "postprocess program found"))?;
    }

    info!("server configuration: OK");
    Ok(())
}

/// Bootstrap: create root and purgery_root directories.
pub fn bootstrap(config: &ServerConfig) -> Result<()> {
    info!("bootstrapping server directories");

    let root_path = config.root.as_path();
    fs::create_dir_all(root_path.as_std_path())
        .with_context(|| format!("failed to create root: {}", root_path.as_str()))?;
    info!(path = %root_path.as_str(), "created root");

    let purgery_path = config.purgery_root.as_path();
    fs::create_dir_all(purgery_path.as_std_path())
        .with_context(|| format!("failed to create purgery_root: {}", purgery_path.as_str()))?;
    info!(path = %purgery_path.as_str(), "created purgery_root");

    info!("bootstrap complete");
    Ok(())
}

/// Run GC: collect expired incoming runs.
pub fn run_gc(config: &ServerConfig) -> Result<()> {
    let gc_config = &config.gc;
    let purgery_path = config.purgery_root.as_path();

    if !purgery_path.exists() {
        return Ok(());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for entry in fs::read_dir(purgery_path.as_std_path())
        .with_context(|| format!("failed to read purgery root: {}", purgery_path.as_str()))?
    {
        let entry = entry?;
        let nickname_path = entry.path();
        if !nickname_path.is_dir() {
            continue;
        }
        let nickname_str = match nickname_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let Ok(nickname) = Nickname::new(nickname_str) else {
            continue;
        };

        let incoming_dir = nickname_path.join("incoming");
        if !incoming_dir.exists() {
            continue;
        }

        for run_entry in fs::read_dir(&incoming_dir)
            .with_context(|| format!("failed to read incoming dir: {}", incoming_dir.display()))?
        {
            let run_entry = run_entry?;
            let run_path = run_entry.path();
            if !run_path.is_dir() {
                continue;
            }
            let run_id_str = match run_path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_owned(),
                None => continue,
            };
            let Ok(run_id) = RunId::new(run_id_str) else {
                continue;
            };

            let lease_path = Utf8PathBuf::from_path_buf(run_path.join("lease.toml"))
                .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref()));

            let expired = if lease_path.exists() {
                match fs::read_to_string(lease_path.as_std_path()) {
                    Ok(content) => {
                        match toml::from_str::<purgery_core::LeaseFile>(&content) {
                            Ok(lease) => {
                                // Validate lease envelope — mismatched lease is treated as expired
                                let valid = lease.protocol_version == 1
                                    && lease.nickname == nickname.as_str()
                                    && lease.run_id == run_id.as_str();
                                if !valid {
                                    warn!(
                                        nickname = %nickname.as_str(),
                                        run_id = %run_id.as_str(),
                                        protocol = lease.protocol_version,
                                        lease_nickname = %lease.nickname,
                                        lease_run_id = %lease.run_id,
                                        "gc: lease envelope mismatch",
                                    );
                                }
                                !valid || now >= lease.expires_at_unix_secs
                            }
                            Err(_) => true, // malformed lease -> expire
                        }
                    }
                    Err(_) => true,
                }
            } else {
                // No lease file — use mtime as fallback
                let metadata = match fs::metadata(&run_path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .unwrap_or_default()
                    .as_secs();
                // Treat as expired if mtime is older than lease_secs
                now.saturating_sub(mtime) > gc_config.incoming_lease_secs * 2
            };

            if !expired {
                continue;
            }

            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                "gc: collecting expired incoming run"
            );

            // Move to failed with a status
            let failed_path = config
                .purgery_root
                .run_dir(&nickname, &run_id, RunPhase::Failed);
            if failed_path.exists() {
                // GC quarantine path — apply same cleanup as normal collection
                let quarantine_name = format!("gc-quarantine-{}-{}", run_id.as_str(), now);
                let quarantine_path = config.purgery_root.run_dir(
                    &nickname,
                    &RunId::new(quarantine_name).unwrap(),
                    RunPhase::Failed,
                );
                if let Some(parent) = quarantine_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if fs::rename(&run_path, quarantine_path.as_std_path()).is_ok() {
                    let status = RunStatus {
                        run_id: run_id.clone(),
                        nickname: nickname.clone(),
                        state: RunState::Failed,
                        entries: vec![],
                        error: Some("abandoned upload expired (quarantined)".into()),
                    };
                    if let Err(error) = publish_status_atomic(&quarantine_path, &status) {
                        warn!(nickname = %nickname.as_str(), run_id = %run_id.as_str(), error = %error, "gc: failed to publish quarantine status");
                    }
                    let files_dir = quarantine_path.join("files");
                    if files_dir.exists() {
                        if let Err(error) = fs::remove_dir_all(files_dir.as_std_path()) {
                            warn!(nickname = %nickname.as_str(), run_id = %run_id.as_str(), error = %error, "gc: failed to remove quarantined files");
                        }
                    }
                }
                continue;
            }

            if let Some(parent) = failed_path.parent() {
                let _ = fs::create_dir_all(parent.as_std_path());
            }

            // Claim the abandoned run by renaming incoming to failed
            if let Err(e) = fs::rename(&run_path, failed_path.as_std_path()) {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    error = %e,
                    "gc: failed to claim abandoned run"
                );
                continue;
            }

            // Write failed status
            let status = RunStatus {
                run_id: run_id.clone(),
                nickname: nickname.clone(),
                state: RunState::Failed,
                entries: vec![],
                error: Some("abandoned upload expired".into()),
            };
            if let Err(error) = publish_status_atomic(&failed_path, &status) {
                warn!(nickname = %nickname.as_str(), run_id = %run_id.as_str(), error = %error, "gc: failed to publish failed status");
            }

            // Remove uploaded files to reclaim disk, keep metadata
            let files_dir = failed_path.join("files");
            if files_dir.exists() {
                if let Err(error) = fs::remove_dir_all(files_dir.as_std_path()) {
                    warn!(nickname = %nickname.as_str(), run_id = %run_id.as_str(), error = %error, "gc: failed to remove collected files");
                }
            }
        }
    }

    Ok(())
}

/// Heartbeat: update lease file for an incoming run.
pub fn heartbeat_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    let incoming_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Incoming);
    if !incoming_path.exists() {
        anyhow::bail!(
            "run {}/{} is not in incoming phase",
            nickname.as_str(),
            run_id.as_str()
        );
    }

    let lease_path = incoming_path.join("lease.toml");
    let lease_content = fs::read_to_string(lease_path.as_std_path())
        .with_context(|| "failed to read lease file")?;
    let mut lease: purgery_core::LeaseFile =
        toml::from_str(&lease_content).with_context(|| "failed to parse lease file")?;

    // Validate lease envelope — confirm this lease really belongs to this run
    if lease.protocol_version != 1 {
        anyhow::bail!(
            "lease protocol version {} does not match expected 1",
            lease.protocol_version
        );
    }
    if lease.nickname != nickname.as_str() {
        anyhow::bail!(
            "lease nickname '{}' does not match expected '{}'",
            lease.nickname,
            nickname.as_str()
        );
    }
    if lease.run_id != run_id.as_str() {
        anyhow::bail!(
            "lease run_id '{}' does not match expected '{}'",
            lease.run_id,
            run_id.as_str()
        );
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    lease.last_heartbeat_unix_secs = now;
    lease.expires_at_unix_secs = now + config.gc.incoming_lease_secs;

    let new_content = toml::to_string(&lease).with_context(|| "failed to serialize lease")?;
    let tmp_path = incoming_path.join("lease.toml.tmp");
    fs::write(tmp_path.as_std_path(), &new_content).with_context(|| "failed to write lease")?;
    fs::rename(tmp_path.as_std_path(), lease_path.as_std_path())
        .with_context(|| "failed to commit lease")?;

    Ok(())
}

// ── Remote shell escaping ──────────────────────────────────────────

/// Build a remote SSH command from a program and arguments.
///
/// Each argument is shell-escaped individually to avoid shell injection
/// from paths containing spaces or special characters.
pub fn build_remote_command(program: &str, args: &[String]) -> String {
    let mut cmd = String::new();
    cmd.push_str(program);
    for a in args {
        cmd.push(' ');
        cmd.push_str(&purgery_core::shell_escape(a));
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use purgery_core::{
        ClientLocalPath, ManifestEntry, NormalizedRelativePath, PostprocessConfig, PostprocessKind,
        PostprocessStepDefinition, ServerRoot, SyncName,
    };

    fn test_server_config(purgery_root: &Utf8Path, server_root: &Utf8Path) -> ServerConfig {
        ServerConfig {
            root: ServerRoot::new(server_root.to_owned()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.to_owned()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        }
    }

    fn write_run_toml(dir: &Utf8Path, nickname: &Nickname) {
        let content = format!(
            r#"nickname = "{}"
"#,
            nickname.as_str()
        );
        fs::write(dir.join("run.toml"), &content).unwrap();
    }

    fn write_run_toml_with_sync(
        dir: &Utf8Path,
        nickname: &Nickname,
        sync_name: &str,
        to_path: &str,
    ) {
        let content = format!(
            r#"nickname = "{}"

[[sync]]
name = "{}"
to = "{}"
"#,
            nickname.as_str(),
            sync_name,
            to_path,
        );
        fs::write(dir.join("run.toml"), &content).unwrap();
    }

    /// Helper to create a basic setup with a ready run containing one file.
    #[allow(clippy::too_many_arguments)]
    fn setup_single_file_ready(
        purgery_root: &Utf8Path,
        server_root: &Utf8Path,
        nickname: &Nickname,
        run_id: &RunId,
        sync_name: &str,
        to_path: &str,
        staged_rel: &str,
        content: &[u8],
    ) -> (ServerConfig, Utf8PathBuf) {
        let config = test_server_config(purgery_root, server_root);
        let ready_path = config
            .purgery_root
            .run_dir(nickname, run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        let staged_path = ready_path.join(staged_rel);
        if let Some(parent) = staged_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&staged_path, content).unwrap();

        write_run_toml_with_sync(&ready_path, nickname, sync_name, to_path);

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new(sync_name.into()).unwrap(),
                local_path: ClientLocalPath::new(format!("/home/user/{sync_name}/{staged_rel}"))
                    .unwrap(),
                staged_path: NormalizedRelativePath::new(staged_rel.into()).unwrap(),
                relative_path: NormalizedRelativePath::new(
                    staged_rel
                        .rsplit_once('/')
                        .map(|(_, f)| f)
                        .unwrap_or(staged_rel)
                        .into(),
                )
                .unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: content.len() as u64,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        (config, staged_path)
    }

    // ── Core pipeline test ──

    #[test]
    fn test_full_processing_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-001".into()).unwrap();

        let (config, staged_file_path) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"hello world",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(
            status.entries[0].final_paths,
            vec!["laptop/videos/test.mp4"],
            "single-output import must record one final path"
        );

        let final_path = server_root.join("laptop/videos/test.mp4");
        assert!(final_path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "hello world");
        assert!(!staged_file_path.exists());
    }

    #[test]
    fn test_processing_skips_unknown_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-002".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        // Run config has no sync mappings
        write_run_toml(&ready_path, &nickname);

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("unknown-sync".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Skipped);
    }

    #[test]
    fn test_processing_missing_staged_file() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-003".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        write_run_toml_with_sync(&ready_path, &nickname, "videos", "videos");

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/missing.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/missing.mp4".into())
                    .unwrap(),
                relative_path: NormalizedRelativePath::new("missing.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("failed to read staged metadata"));
    }

    #[test]
    fn test_rule_matching() {
        use purgery_core::rsync_pattern_match;
        // Unanchored patterns match at any position
        assert!(rsync_pattern_match("*.mp4", "videos/a.mp4"));
        assert!(rsync_pattern_match("*.mov", "videos/subdir/b.mov"));
        assert!(rsync_pattern_match("*.webm", "videos/c.webm"));
        assert!(rsync_pattern_match("*.mp3", "audio/song.mp3")); // unanchored matches at "song.mp3"
        assert!(!rsync_pattern_match("*.mp4", "videos/a.txt"));
        // Anchored patterns match from start of path
        assert!(rsync_pattern_match("/videos/*", "videos/a.mp4"));
        assert!(!rsync_pattern_match("/audio/*", "videos/a.mp4"));
        // ** patterns
        assert!(rsync_pattern_match("**/*.mp4", "videos/sub/a.mp4"));
        assert!(rsync_pattern_match("cache/**", "cache/sub/file.txt"));
    }

    #[test]
    fn test_find_ready_runs_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let root =
            PurgeryRoot::new(Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap())
                .unwrap();
        let runs = find_ready_runs(&root).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn test_find_ready_runs_with_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root =
            PurgeryRoot::new(Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap())
                .unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();

        let run1 = root.run_dir(
            &nickname,
            &RunId::new("run-1".into()).unwrap(),
            RunPhase::Ready,
        );
        let run2 = root.run_dir(
            &nickname,
            &RunId::new("run-2".into()).unwrap(),
            RunPhase::Ready,
        );
        fs::create_dir_all(&run1).unwrap();
        fs::create_dir_all(&run2).unwrap();

        let runs = find_ready_runs(&root).unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn test_nickname_mismatch_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let dir_nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-env-001".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&dir_nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        // Run config has different nickname than the directory
        let run_config_content = r#"nickname = "other-machine""#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: Nickname::new("other-machine".into()).unwrap(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&config, &dir_nickname, &run_id);
        assert!(result.is_err());

        let failed_path = config
            .purgery_root
            .run_dir(&dir_nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("envelope validation failed"));
    }

    #[test]
    fn test_bad_manifest_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-manifest".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        write_run_toml(&ready_path, &nickname);
        fs::write(ready_path.join("manifest.toml"), "not valid toml {{{").unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(result.is_err());

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("failed to parse manifest"));
    }

    #[test]
    fn test_bad_run_config_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-config".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        fs::write(ready_path.join("run.toml"), "not valid toml {{{").unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(result.is_err());

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("failed to parse run config"));
    }

    #[test]
    fn test_build_remote_command() {
        let args = vec!["--input".to_string(), "/path/file.mp4".to_string()];
        let cmd = build_remote_command("my-compress-video", &args);
        assert_eq!(cmd, "my-compress-video '--input' '/path/file.mp4'");
    }

    #[test]
    fn test_build_remote_command_with_spaces() {
        let args = vec![
            "--input".to_string(),
            "/path/with spaces/file.mp4".to_string(),
        ];
        let cmd = build_remote_command("rsync", &args);
        assert_eq!(cmd, "rsync '--input' '/path/with spaces/file.mp4'");
    }

    #[test]
    fn test_postprocessing_path_with_spaces() {
        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".to_owned(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                }],
            },
        };

        let tmp = tempfile::tempdir().unwrap();
        let work_area = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_path = work_area.join("some file.mp4");
        fs::write(&work_path, b"test data").unwrap();

        let run_plan = RunPlan::build(&server_config, &run_config).unwrap();
        let results = apply_postprocessing(&run_plan, "videos/some file.mp4", &work_path);
        assert!(results.is_ok(), "postprocess with spaces should succeed");
        let outputs = results.unwrap();
        assert!(!outputs.is_empty());
        assert!(outputs.contains(&work_path));
    }

    #[test]
    fn test_postprocessing_failure_does_not_create_final_output() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let server_str = server_root.as_str();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_str.into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.as_str().into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "false".to_owned(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-fail-pp".into()).unwrap();

        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/test.mp4"), b"video content").unwrap();

        write_run_toml_with_sync(&ready_path, &nickname, "videos", "videos");
        let run_config_content = r#"nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#
        .to_string();
        fs::write(ready_path.join("run.toml"), &run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let failed_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0].error.as_ref().unwrap().contains("failed"));

        let final_path = server_root.join("laptop/videos/test.mp4");
        assert!(
            !final_path.exists(),
            "failed postprocess must not create final output"
        );
    }

    #[test]
    fn test_compress_video_verify_output_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let work_area = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_path = work_area.join("video.mp4");
        fs::write(&work_path, b"video").unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".to_owned(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*.mp4".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                }],
            },
        };

        let pp_run_plan = RunPlan::build(&server_config, &run_config).unwrap();
        let result = apply_postprocessing(&pp_run_plan, "videos/video.mp4", &work_path);
        assert!(result.is_ok());
        let outputs = result.unwrap();
        assert!(outputs.contains(&work_path));
    }

    #[test]
    fn test_keep_original_true_commits_both() {
        let tmp = tempfile::tempdir().unwrap();
        let work_area = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_path = work_area.join("video.mp4");
        fs::write(&work_path, b"video").unwrap();

        let compressed = work_area.join("video.Z.webm");
        fs::write(&compressed, b"compressed").unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".to_owned(),
                            args: vec![],
                            expected_outputs: vec!["{stem}.Z.webm".into()],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                }],
            },
        };

        let pp_run_plan = RunPlan::build(&server_config, &run_config).unwrap();
        let result = apply_postprocessing(&pp_run_plan, "videos/video.mp4", &work_path);
        assert!(result.is_ok());
        let outputs = result.unwrap();
        assert!(
            outputs.contains(&work_path),
            "keep_original=true must include original"
        );
        assert!(
            outputs.contains(&compressed),
            "keep_original=true must include compressed"
        );
    }

    #[test]
    fn test_keep_original_false_commits_only_compressed() {
        let tmp = tempfile::tempdir().unwrap();
        let work_area = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_path = work_area.join("video.mp4");
        fs::write(&work_path, b"video").unwrap();

        let compressed = work_area.join("video.Z.webm");
        fs::write(&compressed, b"compressed").unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".to_owned(),
                            args: vec![],
                            expected_outputs: vec!["{stem}.Z.webm".into()],
                            keep_original: false,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                }],
            },
        };

        let pp_run_plan = RunPlan::build(&server_config, &run_config).unwrap();
        let result = apply_postprocessing(&pp_run_plan, "videos/video.mp4", &work_path);
        assert!(result.is_ok());
        let outputs = result.unwrap();
        assert!(
            !outputs.contains(&work_path),
            "keep_original=false must NOT include original"
        );
        assert!(
            outputs.contains(&compressed),
            "keep_original=false must include compressed"
        );
    }

    // ── Temp-file commit test ──

    #[test]
    fn test_temp_file_commit_no_direct_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-tmp-commit".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"hello",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let final_path = server_root.join("laptop/videos/test.mp4");
        assert!(final_path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "hello");

        let has_temp_files = std::fs::read_dir(final_path.parent().unwrap())
            .unwrap()
            .any(|e| {
                e.ok()
                    .and_then(|e| e.file_name().to_str().map(|s| s.to_owned()))
                    .map(|s| s.starts_with(".purgery-commit"))
                    .unwrap_or(false)
            });
        assert!(
            !has_temp_files,
            "temp files must be cleaned up after commit"
        );
    }

    // ── Atomic replacement tests ──

    #[test]
    fn test_existing_regular_final_output_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-replace".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"new content",
        );

        let final_path = server_root.join("laptop/videos/test.mp4");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"old content").unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "new content");
        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done_path.join("status.toml")).unwrap())
                .unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    #[test]
    fn test_regular_file_replaces_existing_empty_directory_like_rsync() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-directory-block".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"content",
        );
        let final_path = server_root.join("laptop/videos/test.mp4");
        fs::create_dir_all(&final_path).unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "content");
        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done_path.join("status.toml")).unwrap())
                .unwrap();
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    #[test]
    fn test_regular_file_replaces_existing_symlink_like_rsync() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-final-symlink".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "documents",
            "documents",
            "files/documents/a.txt",
            b"content",
        );
        let final_path = server_root.join("laptop/documents/a.txt");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("missing-target", &final_path).unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "content");
        assert!(!fs::symlink_metadata(&final_path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    // ── Work area namespacing test ──

    #[test]
    fn test_work_area_namespacing_no_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-ns".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::create_dir_all(ready_path.join("files/pictures")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"video content").unwrap();
        fs::write(ready_path.join("files/pictures/a.mp4"), b"picture content").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[sync]]
name = "pictures"
to = "pictures"
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("videos".into()).unwrap(),
                    local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 13,
                    mtime_ns: 1000000,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("pictures".into()).unwrap(),
                    local_path: ClientLocalPath::new("/home/user/Pictures/a.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/pictures/a.mp4".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 15,
                    mtime_ns: 1000001,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let video_final = server_root.join("laptop/videos/a.mp4");
        let picture_final = server_root.join("laptop/pictures/a.mp4");
        assert!(video_final.exists());
        assert!(picture_final.exists());
        assert_eq!(fs::read_to_string(&video_final).unwrap(), "video content");
        assert_eq!(
            fs::read_to_string(&picture_final).unwrap(),
            "picture content"
        );

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[1].status, FileStatus::Imported);
    }

    // ── Staged path mismatch test ──

    #[test]
    fn test_manifest_staged_path_mismatch_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-sp-mismatch".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"content").unwrap();

        write_run_toml_with_sync(&ready_path, &nickname, "videos", "videos");

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/other/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 7,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("staged_path mismatch"));
    }

    #[test]
    fn test_manifest_staged_path_match_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-sp-match".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/a.mp4",
            b"content",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    // ── Staged symlink rejection test ──

    #[test]
    fn test_staged_symlink_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-symlink".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();

        let real_file = ready_path.join("files/videos/real.mp4");
        fs::write(&real_file, b"real content").unwrap();
        let staged_link = ready_path.join("files/videos/a.mp4");
        std::os::unix::fs::symlink(&real_file, &staged_link).unwrap();

        write_run_toml_with_sync(&ready_path, &nickname, "videos", "videos");

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 12,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let final_path = server_root.join("laptop/videos/a.mp4");
        assert!(
            !final_path.exists(),
            "symlink must not be imported to final path"
        );

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("kind does not match"));
    }

    // ── Invalid regex test ──

    #[test]
    fn test_empty_postprocess_pattern_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-pattern".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"content").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[postprocess.rules]]
match = ""
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(result.is_err(), "process_run must error on empty pattern");

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed_path.exists());
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(
            status.error.as_deref().unwrap().contains("pattern")
                || status.error.as_deref().unwrap().contains("invalid")
        );
    }

    // ── Work area cleanup tests ──

    #[test]
    fn test_run_state_done_removes_work_area() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-done-wa".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/a.mp4",
            b"hello",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let work_area = purgery_core::work_dir(config.root.as_path(), &nickname, &run_id);
        assert!(!work_area.exists(), "work area must be removed on Done");
    }

    #[test]
    fn test_run_state_partial_keeps_work_area() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_root.as_str().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.as_str().into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "false".to_owned(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-partial-wa".into()).unwrap();

        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/test.mp4"), b"video content").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let work_area = purgery_core::work_dir(server_config.root.as_path(), &nickname, &run_id);
        let failed_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed_path.exists());
        assert!(
            work_area.exists(),
            "work area must be kept for Failed state"
        );
    }

    // ── compress-video keep_original end-to-end ──

    #[test]
    fn test_compress_video_keep_original_records_both_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();

        let script_path = tmp.path().join("compress.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nbase=$(basename \"$2\");stem=\"${base%.*}\";dir=$(dirname \"$2\");touch \"$dir/$stem.Z.webm\"\n",
        ).unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_root.as_str().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.as_str().into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: script_path.to_string_lossy().to_string(),
                            args: vec!["--input".into(), "{input}".into()],
                            expected_outputs: vec!["{stem}.Z.webm".into()],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-both".into()).unwrap();

        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/video.mp4"), b"video").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/video.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/video.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("video.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let done_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[0].final_paths.len(), 2);

        let original_final = server_root.join("laptop/videos/video.mp4");
        let compressed_final = server_root.join("laptop/videos/video.Z.webm");
        assert!(original_final.exists());
        assert!(compressed_final.exists());
    }

    #[test]
    fn test_compress_video_keep_original_false_records_one_path() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();

        let script_path = tmp.path().join("compress.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nbase=$(basename \"$2\");stem=\"${base%.*}\";dir=$(dirname \"$2\");touch \"$dir/$stem.Z.webm\"\n",
        ).unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_root.as_str().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.as_str().into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: script_path.to_string_lossy().to_string(),
                            args: vec!["--input".into(), "{input}".into()],
                            expected_outputs: vec!["{stem}.Z.webm".into()],
                            keep_original: false,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-comp-only".into()).unwrap();

        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/video.mp4"), b"video").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/video.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/video.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("video.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let done_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[0].final_paths.len(), 1);

        let original_final = server_root.join("laptop/videos/video.mp4");
        let compressed_final = server_root.join("laptop/videos/video.Z.webm");
        assert!(
            !original_final.exists(),
            "original must NOT exist with keep_original=false"
        );
        assert!(compressed_final.exists());
    }

    // ── Run Plan tests ──

    #[test]
    fn test_run_plan_validates_empty_pattern() {
        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "".into(),
                    steps: vec!["compress-video".into()],
                }],
            },
        };
        let result = RunPlan::build(&server_config, &run_config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty pattern"));
    }

    #[test]
    fn test_run_plan_validates_step_references() {
        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*".into(),
                    steps: vec!["nonexistent-step".into()],
                }],
            },
        };
        let result = RunPlan::build(&server_config, &run_config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not defined on server"));
    }

    // ── begin_run / finish_run tests ──

    #[test]
    fn test_begin_run_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-begin".into()).unwrap();

        let response_str = begin_run(&server_config, &nickname, &run_id).unwrap();
        let response: purgery_core::BeginRunResponse = toml::from_str(&response_str).unwrap();
        assert_eq!(response.protocol_version, 1);
        assert_eq!(response.nickname, "laptop");
        assert_eq!(response.run_id, "test-begin");

        let incoming_path = Utf8Path::new(&response.incoming_dir);
        assert!(incoming_path.exists(), "incoming dir must exist");
        assert!(
            Utf8Path::new(&response.files_dir).exists(),
            "files dir must exist"
        );
    }

    #[test]
    fn test_finish_run_moves_from_incoming_to_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-finish".into()).unwrap();

        // Begin the run
        begin_run(&server_config, &nickname, &run_id).unwrap();

        let incoming_path =
            server_config
                .purgery_root
                .run_dir(&nickname, &run_id, RunPhase::Incoming);
        assert!(incoming_path.exists());

        // Finish it
        finish_run(&server_config, &nickname, &run_id).unwrap();

        assert!(
            !incoming_path.exists(),
            "incoming must be gone after finish"
        );
        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        assert!(ready_path.exists(), "ready dir must exist after finish");
    }

    #[test]
    fn test_read_run_status_from_done() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-status".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/a.mp4",
            b"data",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let status = read_run_status(&config, &nickname, &run_id).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.nickname, nickname);
        assert_eq!(status.run_id, run_id);
    }

    #[test]
    fn test_read_run_status_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("nonexistent".into()).unwrap();

        let result = read_run_status(&server_config, &nickname, &run_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_finish_run_rejects_expired_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-expired-lease".into()).unwrap();

        begin_run(&server_config, &nickname, &run_id).unwrap();

        let incoming_path =
            server_config
                .purgery_root
                .run_dir(&nickname, &run_id, RunPhase::Incoming);
        let lease_path = incoming_path.join("lease.toml");
        let mut lease: purgery_core::LeaseFile =
            toml::from_str(&fs::read_to_string(&lease_path).unwrap()).unwrap();
        lease.expires_at_unix_secs = 0;
        fs::write(&lease_path, toml::to_string(&lease).unwrap()).unwrap();

        let result = finish_run(&server_config, &nickname, &run_id);
        assert!(result.is_err(), "finish-run must reject expired lease");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expired") || err.contains("lease"),
            "error: {err}"
        );
    }

    #[test]
    fn test_finish_run_rejects_mismatched_lease_nickname() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-wrong-nickname".into()).unwrap();

        begin_run(&server_config, &nickname, &run_id).unwrap();

        let incoming_path =
            server_config
                .purgery_root
                .run_dir(&nickname, &run_id, RunPhase::Incoming);
        let lease_path = incoming_path.join("lease.toml");
        let mut lease: purgery_core::LeaseFile =
            toml::from_str(&fs::read_to_string(&lease_path).unwrap()).unwrap();
        lease.nickname = "wrong-machine".into();
        fs::write(&lease_path, toml::to_string(&lease).unwrap()).unwrap();

        let result = finish_run(&server_config, &nickname, &run_id);
        assert!(
            result.is_err(),
            "finish-run must reject mismatched nickname"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nickname"), "error: {err}");
    }

    #[test]
    fn test_process_once_processes_ready_run_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ready-after-restart".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "documents",
            "documents",
            "files/documents/a.txt",
            b"ready",
        );

        process_once_raw(&config).unwrap();

        assert!(config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done)
            .exists());
        assert_eq!(
            fs::read_to_string(server_root.join("laptop/documents/a.txt")).unwrap(),
            "ready"
        );
    }

    #[test]
    fn test_process_once_recovers_processing_run_without_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-interrupted".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "documents",
            "documents",
            "files/documents/a.txt",
            b"hello",
        );
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.parent().unwrap()).unwrap();
        fs::rename(&ready, &processing).unwrap();
        let stale_work = work_dir(config.root.as_path(), &nickname, &run_id);
        fs::create_dir_all(&stale_work).unwrap();
        fs::write(stale_work.join("stale"), b"stale").unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done.join("status.toml").exists());
        assert_eq!(
            fs::read_to_string(server_root.join("laptop/documents/a.txt")).unwrap(),
            "hello"
        );
        assert!(!stale_work.exists());
    }

    #[test]
    fn test_process_once_finalizes_processing_run_with_valid_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-status".into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        let status = RunStatus {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            state: RunState::Partial,
            entries: vec![],
            error: None,
        };
        fs::write(processing.join("status.toml"), status.to_toml().unwrap()).unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        assert!(config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done)
            .exists());
    }

    fn assert_mismatched_processing_status_fails(
        status_nickname: Nickname,
        status_run_id: RunId,
        directory_run_id: &str,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new(directory_run_id.into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        let status = RunStatus {
            run_id: status_run_id,
            nickname: status_nickname,
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(processing.join("status.toml"), status.to_toml().unwrap()).unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        assert!(!config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done)
            .exists());
        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let failed_status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert_eq!(failed_status.nickname, nickname);
        assert_eq!(failed_status.run_id, run_id);
        assert_eq!(failed_status.state, RunState::Failed);
        assert_eq!(
            failed_status.error.as_deref(),
            Some("interrupted processing had mismatched status envelope")
        );
    }

    #[test]
    fn test_process_once_fails_processing_run_with_mismatched_status_nickname() {
        assert_mismatched_processing_status_fails(
            Nickname::new("other-machine".into()).unwrap(),
            RunId::new("recover-wrong-nickname".into()).unwrap(),
            "recover-wrong-nickname",
        );
    }

    #[test]
    fn test_process_once_fails_processing_run_with_mismatched_status_run_id() {
        assert_mismatched_processing_status_fails(
            Nickname::new("laptop".into()).unwrap(),
            RunId::new("other-run".into()).unwrap(),
            "recover-wrong-run-id",
        );
    }

    #[test]
    fn test_mismatched_status_recovery_propagates_terminal_move_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("blocked-failed-move".into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(&failed).unwrap();
        fs::write(failed.join("existing"), b"occupied").unwrap();
        let mismatched_status = RunStatus {
            run_id: RunId::new("other-run".into()).unwrap(),
            nickname: nickname.clone(),
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(
            processing.join("status.toml"),
            mismatched_status.to_toml().unwrap(),
        )
        .unwrap();

        let error = recover_or_process_processing_run(&config, &nickname, &run_id).unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to move run-level failure to failed"));
        assert!(processing.exists());
        let status =
            RunStatus::from_toml(&fs::read_to_string(processing.join("status.toml")).unwrap())
                .unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("interrupted processing had mismatched status envelope")
        );
    }

    #[test]
    fn test_malformed_status_recovery_propagates_failed_status_write_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("blocked-status-write".into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.join("status.toml.tmp")).unwrap();
        fs::write(processing.join("status.toml"), "not valid = [toml").unwrap();

        let error = recover_or_process_processing_run(&config, &nickname, &run_id).unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to write temporary run failure status"));
        assert!(processing.exists());
        assert_eq!(
            fs::read_to_string(processing.join("status.toml")).unwrap(),
            "not valid = [toml"
        );
    }

    #[test]
    fn test_process_once_fails_processing_run_with_malformed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-malformed".into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        fs::write(processing.join("status.toml"), "not valid = [toml").unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("interrupted processing had malformed status")
        );
    }

    #[test]
    fn test_replay_after_final_replacement_without_status_converges() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-committed-output".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "documents",
            "documents",
            "files/documents/a.txt",
            b"new",
        );
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.parent().unwrap()).unwrap();
        fs::rename(&ready, &processing).unwrap();
        let final_path = server_root.join("laptop/documents/a.txt");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"new").unwrap();
        assert!(!processing.join("status.toml").exists());

        process_once_raw(&config).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "new");
        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Done);
    }

    #[test]
    fn test_repeated_imports_same_destination_are_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();

        for (run, content) in [("repeat-1", b"hello".as_slice()), ("repeat-2", b"hello")] {
            let run_id = RunId::new(run.into()).unwrap();
            let (config, _) = setup_single_file_ready(
                &purgery_root,
                &server_root,
                &nickname,
                &run_id,
                "documents",
                "documents",
                "files/documents/a.txt",
                content,
            );
            process_run(&config, &nickname, &run_id).unwrap();
            assert!(config
                .purgery_root
                .run_dir(&nickname, &run_id, RunPhase::Done)
                .exists());
        }

        assert_eq!(
            fs::read_to_string(server_root.join("laptop/documents/a.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn test_repeated_import_replaces_changed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();

        for (run, content) in [("version-1", b"v1".as_slice()), ("version-2", b"v2")] {
            let run_id = RunId::new(run.into()).unwrap();
            let (config, _) = setup_single_file_ready(
                &purgery_root,
                &server_root,
                &nickname,
                &run_id,
                "documents",
                "documents",
                "files/documents/a.txt",
                content,
            );
            process_run(&config, &nickname, &run_id).unwrap();
        }

        assert_eq!(
            fs::read_to_string(server_root.join("laptop/documents/a.txt")).unwrap(),
            "v2"
        );
    }

    #[test]
    fn test_gc_collects_abandoned_incoming_upload() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("abandoned-upload".into()).unwrap();
        begin_run(&config, &nickname, &run_id).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::write(incoming.join("files/partial.txt"), b"partial").unwrap();
        let lease_path = incoming.join("lease.toml");
        let mut lease: purgery_core::LeaseFile =
            toml::from_str(&fs::read_to_string(&lease_path).unwrap()).unwrap();
        lease.expires_at_unix_secs = 0;
        fs::write(&lease_path, toml::to_string(&lease).unwrap()).unwrap();

        run_gc(&config).unwrap();

        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.join("files").exists());
        let status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Failed);
    }

    /// begin-run output must be parseable as BeginRunResponse TOML.
    /// This is a stdout-clean invariant: protocol output must never be
    /// contaminated by log output, and the returned string must always
    /// be valid TOML regardless of logging configuration.
    #[test]
    fn test_begin_run_stdout_is_parseable_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-stdout-begin".into()).unwrap();

        let response_str = begin_run(&server_config, &nickname, &run_id).unwrap();
        // Must parse as BeginRunResponse — if logging contaminated stdout,
        // this parse would fail.
        let response: purgery_core::BeginRunResponse = toml::from_str(&response_str)
            .expect("begin-run stdout must be valid BeginRunResponse TOML");
        assert_eq!(response.protocol_version, 1);
        assert_eq!(response.nickname, "laptop");
        assert_eq!(response.run_id, "test-stdout-begin");
    }

    /// status output must be parseable as RunStatus TOML.
    #[test]
    fn test_status_stdout_is_parseable_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-stdout-status".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"hello",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let status = read_run_status(&config, &nickname, &run_id).unwrap();
        let status_str = status.to_toml().unwrap();
        // Must parse back as RunStatus
        let parsed: purgery_core::RunStatus = purgery_core::RunStatus::from_toml(&status_str)
            .expect("status stdout must be valid RunStatus TOML");
        assert_eq!(parsed.state, purgery_core::RunState::Done);
    }

    #[test]
    fn test_rsync_oracle_directory_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();

        let missing = root.join("missing");
        assert_eq!(
            commit_directory_entry(&missing, &root).unwrap(),
            CommitDisposition::Created
        );

        let existing = root.join("existing");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("extra"), "keep").unwrap();
        assert_eq!(
            commit_directory_entry(&existing, &root).unwrap(),
            CommitDisposition::Kept
        );
        assert_eq!(fs::read_to_string(existing.join("extra")).unwrap(), "keep");

        let file = root.join("file");
        fs::write(&file, "old").unwrap();
        assert_eq!(
            commit_directory_entry(&file, &root).unwrap(),
            CommitDisposition::Replaced
        );
        assert!(file.is_dir());

        let symlink = root.join("symlink");
        std::os::unix::fs::symlink("elsewhere", &symlink).unwrap();
        assert_eq!(
            commit_directory_entry(&symlink, &root).unwrap(),
            CommitDisposition::Replaced
        );
        assert!(symlink.is_dir());
    }

    #[test]
    fn test_rsync_oracle_regular_file_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let source = Utf8PathBuf::from_path_buf(tmp.path().join("source")).unwrap();
        fs::write(&source, "new content").unwrap();
        let run_id = RunId::new("oracle-file".into()).unwrap();

        for name in ["missing", "file", "symlink", "empty-dir"] {
            let destination = root.join(name);
            match name {
                "file" => fs::write(&destination, "old").unwrap(),
                "symlink" => std::os::unix::fs::symlink("target", &destination).unwrap(),
                "empty-dir" => fs::create_dir(&destination).unwrap(),
                _ => {}
            }
            commit_regular_file_entry(&source, &destination, &root, &run_id).unwrap();
            assert_eq!(fs::read_to_string(&destination).unwrap(), "new content");
            assert!(!fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink());
        }

        let nonempty = root.join("nonempty-dir");
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("extra"), "keep").unwrap();
        assert!(commit_regular_file_entry(&source, &nonempty, &root, &run_id).is_err());
        assert_eq!(fs::read_to_string(nonempty.join("extra")).unwrap(), "keep");
    }

    #[test]
    fn test_rsync_oracle_symlink_conflicts_and_literal_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let run_id = RunId::new("oracle-link".into()).unwrap();
        let target = Utf8Path::new("../literal-target");

        for name in ["missing", "file", "symlink", "empty-dir"] {
            let destination = root.join(name);
            match name {
                "file" => fs::write(&destination, "old").unwrap(),
                "symlink" => std::os::unix::fs::symlink("old-target", &destination).unwrap(),
                "empty-dir" => fs::create_dir(&destination).unwrap(),
                _ => {}
            }
            commit_symlink_entry(target, &destination, &root, &run_id).unwrap();
            assert_eq!(fs::read_link(&destination).unwrap(), target.as_std_path());
        }

        let nonempty = root.join("nonempty-dir");
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("extra"), "keep").unwrap();
        assert!(commit_symlink_entry(target, &nonempty, &root, &run_id).is_err());
        assert_eq!(fs::read_to_string(nonempty.join("extra")).unwrap(), "keep");
    }

    #[test]
    fn test_rsync_oracle_parent_conflicts_are_resolved_by_directory_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let source = Utf8PathBuf::from_path_buf(tmp.path().join("source")).unwrap();
        fs::write(&source, "child").unwrap();
        let run_id = RunId::new("oracle-parent".into()).unwrap();

        for name in ["file-parent", "symlink-parent"] {
            let parent = root.join(name);
            if name == "file-parent" {
                fs::write(&parent, "old").unwrap();
            } else {
                std::os::unix::fs::symlink("elsewhere", &parent).unwrap();
            }
            commit_directory_entry(&parent, &root).unwrap();
            let child = parent.join("child");
            commit_regular_file_entry(&source, &child, &root, &run_id).unwrap();
            assert_eq!(fs::read_to_string(child).unwrap(), "child");
        }
    }

    #[test]
    fn test_process_run_overlays_directory_file_and_symlink_without_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("tree-overlay".into()).unwrap();
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        let staged = ready.join("files/data/tree");
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("new.txt"), "new").unwrap();
        std::os::unix::fs::symlink("../target", staged.join("link")).unwrap();
        write_run_toml_with_sync(&ready, &nickname, "data", "data");

        let entry = |relative: &str, kind, size, target: Option<&str>| ManifestEntry {
            sync_name: SyncName::new("data".into()).unwrap(),
            local_path: ClientLocalPath::new(format!("/source/{relative}")).unwrap(),
            staged_path: NormalizedRelativePath::new(format!("files/data/{relative}").into())
                .unwrap(),
            relative_path: NormalizedRelativePath::new(relative.into()).unwrap(),
            kind,
            size,
            mtime_ns: 0,
            sha256: None,
            link_target: target.map(Utf8PathBuf::from),
            mode: Default::default(),
            postprocess_steps: Vec::new(),
            covered_by: None,
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                entry("tree", ManifestEntryKind::Directory, 0, None),
                entry(
                    "tree/link",
                    ManifestEntryKind::Symlink,
                    0,
                    Some("../target"),
                ),
                entry("tree/new.txt", ManifestEntryKind::RegularFile, 3, None),
            ],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let final_tree = server_root.join("laptop/data/tree");
        fs::create_dir_all(&final_tree).unwrap();
        fs::write(final_tree.join("extra.txt"), "keep").unwrap();
        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(
            fs::read_to_string(final_tree.join("new.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(final_tree.join("extra.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::read_link(final_tree.join("link")).unwrap(),
            std::path::Path::new("../target")
        );
        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 3);
        assert_eq!(status.entries[0].kind, ManifestEntryKind::Directory);
        assert_eq!(status.entries[1].kind, ManifestEntryKind::Symlink);
        assert_eq!(status.entries[2].kind, ManifestEntryKind::RegularFile);
    }

    #[test]
    fn test_read_run_status_rejects_mismatched_terminal_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("requested".into()).unwrap();
        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        fs::create_dir_all(&done).unwrap();
        let status = RunStatus {
            run_id: RunId::new("different".into()).unwrap(),
            nickname: nickname.clone(),
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(done.join("status.toml"), status.to_toml().unwrap()).unwrap();

        let error = read_run_status(&config, &nickname, &run_id).unwrap_err();
        assert!(error.to_string().contains("status envelope mismatch"));
    }

    fn expected_output_test_plan() -> RunPlan {
        RunPlan {
            rules: vec![CompiledRule {
                pattern: "data/*.txt".into(),
                steps: vec![ResolvedStep {
                    step_name: "generate".into(),
                    step_def: PostprocessStepDefinition {
                        kind: PostprocessKind::Subprocess,
                        program: "true".into(),
                        args: vec![],
                        expected_outputs: vec!["{stem}.out".into()],
                        keep_original: false,
                    },
                }],
            }],
        }
    }

    #[test]
    fn postprocess_regular_expected_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        fs::write(work_path.with_file_name("input.out"), "output").unwrap();

        let outputs =
            apply_postprocessing(&expected_output_test_plan(), "data/input.txt", &work_path)
                .unwrap();
        assert_eq!(outputs, vec![work_path.with_file_name("input.out")]);
    }

    #[test]
    fn postprocess_missing_expected_output_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();

        let error =
            apply_postprocessing(&expected_output_test_plan(), "data/input.txt", &work_path)
                .unwrap_err();
        assert!(error.contains("expected output not found"));
    }

    #[test]
    fn postprocess_symlink_expected_output_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        let target = work_path.with_file_name("target.txt");
        fs::write(&target, "secret target contents").unwrap();
        // Place a symlink to the target as the expected output.  The symlink
        // itself must be accepted — Purgery must not follow or reject it.
        std::os::unix::fs::symlink(&target, work_path.with_file_name("input.out")).unwrap();

        let outputs =
            apply_postprocessing(&expected_output_test_plan(), "data/input.txt", &work_path)
                .unwrap();
        assert!(
            outputs.contains(&work_path.with_file_name("input.out")),
            "symlink expected output must be accepted"
        );
        // The symlink must still point to the original target (not be
        // replaced by the target's content).
        let link = fs::read_link(work_path.with_file_name("input.out")).unwrap();
        assert_eq!(
            link,
            target.as_std_path(),
            "symlink target must be preserved"
        );
    }

    #[test]
    fn postprocess_directory_expected_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        fs::create_dir(work_path.with_file_name("input.out")).unwrap();

        let outputs =
            apply_postprocessing(&expected_output_test_plan(), "data/input.txt", &work_path)
                .unwrap();
        assert!(outputs.contains(&work_path.with_file_name("input.out")));
    }

    #[test]
    fn postprocess_symlink_expected_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        std::os::unix::fs::symlink("some-target", work_path.with_file_name("input.out")).unwrap();

        let outputs =
            apply_postprocessing(&expected_output_test_plan(), "data/input.txt", &work_path)
                .unwrap();
        assert!(outputs.contains(&work_path.with_file_name("input.out")));
    }

    #[test]
    fn postprocess_fifo_expected_output_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        // Create a FIFO (named pipe)
        std::process::Command::new("mkfifo")
            .arg(work_path.with_file_name("input.out").as_std_path())
            .status()
            .unwrap();

        let error =
            apply_postprocessing(&expected_output_test_plan(), "data/input.txt", &work_path)
                .unwrap_err();
        assert!(error.contains("expected output is not a supported entry type"));
    }

    fn duplicate_path_test_entry(
        sync_name: &str,
        relative_path: &str,
        kind: ManifestEntryKind,
    ) -> ManifestEntry {
        ManifestEntry {
            sync_name: SyncName::new(sync_name.into()).unwrap(),
            local_path: ClientLocalPath::new(format!("/source/{sync_name}/{relative_path}"))
                .unwrap(),
            staged_path: NormalizedRelativePath::new(
                format!("files/{sync_name}/{relative_path}").into(),
            )
            .unwrap(),
            relative_path: NormalizedRelativePath::new(relative_path.into()).unwrap(),
            kind,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            mode: purgery_core::ManifestEntryMode::Postprocess,
            postprocess_steps: Vec::new(),
            covered_by: None,
        }
    }

    fn duplicate_path_run_config(first_to: &str, second_to: &str) -> RunConfig {
        RunConfig::from_toml(&format!(
            r#"
nickname = "laptop"

[[sync]]
name = "first"
to = "{first_to}"

[[sync]]
name = "second"
to = "{second_to}"
"#,
        ))
        .unwrap()
    }

    #[test]
    fn duplicate_final_file_paths_across_syncs_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = duplicate_path_run_config("shared", "shared");
        let manifest = Manifest {
            run_id: RunId::new("duplicate-files".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                duplicate_path_test_entry("first", "same.txt", ManifestEntryKind::RegularFile),
                duplicate_path_test_entry("second", "same.txt", ManifestEntryKind::RegularFile),
            ],
        };

        let empty_plan = RunPlan { rules: vec![] };
        let empty_covered = std::collections::HashSet::new();
        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &empty_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(error.contains("duplicate final path"));
    }

    #[test]
    fn identical_relative_paths_under_different_destinations_are_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = duplicate_path_run_config("first-dest", "second-dest");
        let empty_plan = RunPlan { rules: vec![] };
        let empty_covered = std::collections::HashSet::new();
        let manifest = Manifest {
            run_id: RunId::new("distinct-files".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                duplicate_path_test_entry("first", "same.txt", ManifestEntryKind::RegularFile),
                duplicate_path_test_entry("second", "same.txt", ManifestEntryKind::RegularFile),
            ],
        };

        validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &empty_plan,
            &empty_covered,
        )
        .unwrap();
    }

    #[test]
    fn duplicate_final_directory_paths_across_syncs_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = duplicate_path_run_config("shared", "shared");
        let empty_plan = RunPlan { rules: vec![] };
        let empty_covered = std::collections::HashSet::new();
        let manifest = Manifest {
            run_id: RunId::new("duplicate-directories".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                duplicate_path_test_entry("first", "same-dir", ManifestEntryKind::Directory),
                duplicate_path_test_entry("second", "same-dir", ManifestEntryKind::Directory),
            ],
        };

        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &empty_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(error.contains("duplicate final path"));
    }

    #[test]
    fn processing_rejects_duplicate_final_paths_before_importing_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("duplicate-run".into()).unwrap();
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready.join("files/shared")).unwrap();
        fs::write(ready.join("files/shared/same.txt"), "staged").unwrap();
        fs::write(
            ready.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "first"
to = "shared"

[[sync]]
name = "second"
to = "shared"
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("first".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/first/same.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/shared/same.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("same.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 6,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("second".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/second/same.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/shared/same.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("same.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 6,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        assert!(process_run(&config, &nickname, &run_id).is_err());
        assert!(!server_root.join("laptop/shared/same.txt").exists());
        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert!(status.entries.is_empty());
        assert!(status
            .error
            .as_deref()
            .unwrap()
            .contains("duplicate final path"));
    }

    // ── Postprocess-derived duplicate final path tests ──

    #[test]
    fn postprocessed_directory_does_not_cause_false_overlap_rejection() {
        // Postprocessed directory + descendant file must not trigger a false
        // overlap validation failure.  The descendant should be skipped as
        // covered, not rejected as a planned-path conflict.
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("dir-transform".into()).unwrap();
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);

        // Create staged directory with child file
        fs::create_dir_all(ready.join("files/data/photos")).unwrap();
        fs::write(ready.join("files/data/photos/photo.txt"), "content").unwrap();

        // Run config with a postprocess rule that matches the directory
        let run_config_src = r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "photos"
steps = ["pack"]
"#;
        fs::write(ready.join("run.toml"), run_config_src).unwrap();
        // Server config with a matching step
        let config = ServerConfig {
            root: ServerRoot::new(server_root.clone()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.clone()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/photos".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/photos".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("photos".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/photos/photo.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/photos/photo.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("photos/photo.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 7,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        // This must succeed — no false overlap rejection.
        assert!(
            process_run(&config, &nickname, &run_id).is_ok(),
            "postprocessed directory with descendant must not be rejected by overlap validation"
        );

        // The descendant should be skipped, not imported independently.
        let status = read_run_status(&config, &nickname, &run_id).unwrap();
        assert_eq!(status.entries.len(), 2);
        let dir_entry = &status.entries[0];
        let child_entry = &status.entries[1];
        assert_eq!(dir_entry.kind, ManifestEntryKind::Directory);
        assert_eq!(dir_entry.status, FileStatus::Imported);
        assert_eq!(child_entry.status, FileStatus::Skipped);
        assert!(
            child_entry
                .error
                .as_deref()
                .unwrap()
                .contains("covered by postprocessed ancestor"),
            "child must be skipped: {:?}",
            child_entry.error
        );
    }

    fn postprocess_collision_run_config() -> RunConfig {
        RunConfig::from_toml(
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "*.txt"
steps = ["compress"]
"#,
        )
        .unwrap()
    }

    fn postprocess_collision_run_plan() -> RunPlan {
        RunPlan {
            rules: vec![CompiledRule {
                pattern: "*.txt".into(),
                steps: vec![ResolvedStep {
                    step_name: "compress".into(),
                    step_def: PostprocessStepDefinition {
                        kind: PostprocessKind::Subprocess,
                        program: "true".into(),
                        args: vec![],
                        expected_outputs: vec!["{stem}.Z.webm".into()],
                        keep_original: true,
                    },
                }],
            }],
        }
    }

    #[test]
    fn postprocess_output_collides_with_manifest_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = postprocess_collision_run_config();
        let run_plan = postprocess_collision_run_plan();

        let manifest = Manifest {
            run_id: RunId::new("pp-collision".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                // document.txt → postprocess (keep_original) produces document.txt + document.Z.webm
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/document.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/document.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("document.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                // document.Z.webm — would collide with the postprocess output above
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/document.Z.webm".into())
                        .unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/document.Z.webm".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("document.Z.webm".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 200,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };

        let empty_covered = std::collections::HashSet::new();
        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &run_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(
            error.contains("duplicate final path"),
            "error must mention duplicate final path: {error}"
        );
        assert!(
            error.contains("document.Z.webm"),
            "error must mention the colliding filename: {error}"
        );
    }

    #[test]
    fn postprocess_output_from_two_entries_collides() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = RunConfig::from_toml(
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "*.txt"
steps = ["compress"]
"#,
        )
        .unwrap();

        let pp_plan = RunPlan {
            rules: vec![CompiledRule {
                pattern: "*.txt".into(),
                steps: vec![ResolvedStep {
                    step_name: "generate".into(),
                    step_def: PostprocessStepDefinition {
                        kind: PostprocessKind::Subprocess,
                        program: "true".into(),
                        args: vec![],
                        expected_outputs: vec!["result.bin".into()],
                        keep_original: false,
                    },
                }],
            }],
        };

        let manifest = Manifest {
            run_id: RunId::new("pp-cross-entry".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/a.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/a.txt".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("a.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 50,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/b.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/b.txt".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("b.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 60,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };

        let empty_covered = std::collections::HashSet::new();
        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &pp_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(
            error.contains("duplicate final path"),
            "error must mention duplicate final path: {error}"
        );
    }

    #[test]
    fn postprocess_output_collides_with_directory_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = RunConfig::from_toml(
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "*.txt"
steps = ["compress"]
"#,
        )
        .unwrap();
        let run_plan = RunPlan {
            rules: vec![CompiledRule {
                pattern: "*.txt".into(),
                steps: vec![ResolvedStep {
                    step_name: "compress".into(),
                    step_def: PostprocessStepDefinition {
                        kind: PostprocessKind::Subprocess,
                        program: "true".into(),
                        args: vec![],
                        expected_outputs: vec!["output_dir".into()],
                        keep_original: true,
                    },
                }],
            }],
        };

        let manifest = Manifest {
            run_id: RunId::new("pp-dir-collision".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/input.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/input.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("input.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 50,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                // Directory with the same name as the postprocess output
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/output_dir".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/output_dir".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("output_dir".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };

        let empty_covered = std::collections::HashSet::new();
        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &run_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(
            error.contains("duplicate final path"),
            "error must mention duplicate final path: {error}"
        );
    }

    #[test]
    fn source_relative_classification_does_not_use_sync_to_prefix() {
        // Classification must evaluate match patterns against the source-relative
        // path, not the sync.to-prefixed path.
        let matched_mp4 = purgery_core::rsync_pattern_match("*.mp4", "a.mp4");
        assert!(matched_mp4, "*.mp4 must match a.mp4");
        let matched_videos = purgery_core::rsync_pattern_match("videos/*.mp4", "a.mp4");
        assert!(
            !matched_videos,
            "videos/*.mp4 must NOT match a.mp4 (source-relative)"
        );
        let matched_nested = purgery_core::rsync_pattern_match("**/*.mp4", "sub/b.mp4");
        assert!(matched_nested, "**/*.mp4 must match sub/b.mp4");
    }

    #[test]
    fn covered_entries_have_covered_mode_and_covered_by() {
        let entry_descendant = ManifestEntry {
            sync_name: SyncName::new("data".into()).unwrap(),
            local_path: ClientLocalPath::new("/source/photos/photo.txt".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("files/data/photos/photo.txt".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("photos/photo.txt".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 7,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            mode: purgery_core::ManifestEntryMode::Covered,
            postprocess_steps: Vec::new(),
            covered_by: Some("photos".into()),
        };
        assert_eq!(
            entry_descendant.mode,
            purgery_core::ManifestEntryMode::Covered
        );
        assert_eq!(entry_descendant.covered_by.as_deref(), Some("photos"));
    }

    // ── prepare-run covered_by validation tests ──

    #[test]
    fn prepare_run_rejects_covered_entry_with_missing_covered_by() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        // Set up a postprocess step so the directory can be postprocessed
        let config = ServerConfig {
            root: config.root,
            purgery_root: config.purgery_root,
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("covered-by-missing".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        write_run_toml_with_sync(&incoming, &nickname, "data", "data");
        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#;
        fs::write(incoming.join("run.toml"), run_config_content).unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Covered,
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("covered_by"),
            "must reject missing covered_by: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_covered_entry_with_wrong_covered_by() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("covered-by-wrong".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Covered,
                    postprocess_steps: Vec::new(),
                    covered_by: Some("wrong-path".into()),
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("covered_by"),
            "must reject wrong covered_by: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_covered_entry_with_non_empty_postprocess_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("covered-steps".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Covered,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: Some("album".into()),
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("postprocess_steps"),
            "must reject non-empty steps: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_descendant_marked_passthrough_under_postprocessed_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("descendant-passthrough".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Passthrough,
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("covered"),
            "must reject passthrough descendant: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_descendant_marked_postprocess_under_postprocessed_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("descendant-postprocess".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("covered"),
            "must reject postprocess descendant: {error}"
        );
    }
}
