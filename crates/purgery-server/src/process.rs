use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    path_is_within_root, validate_envelope, work_dir, DestinationPath, EntryStatusEntry,
    FileStatus, Manifest, ManifestEntry, ManifestEntryKind, Nickname, NormalizedRelativePath,
    RunConfig, RunId, RunPhase, RunState, RunStatus, ServerConfig,
};
use std::fmt;
use std::fs;
use tracing::{info, span, warn, Level};

use crate::commit::commit_output_entry;
use crate::gc::run_gc;
use crate::phases::{
    finalize_processing_run, move_to_failed, write_progress_best_effort, write_run_failure,
};
use crate::recover::{recover_or_process_processing_run, RecoveryError};
use crate::transform::apply_transform;
use crate::ResolvedTransform;

/// Outcome of attempting to process a run (ready → processing → done/failed).
#[derive(Debug)]
pub enum ProcessingError {
    /// The run has incompatible state (missing or incompatible purgery_version
    /// in run.toml, manifest.toml, etc.). Must be left in place — no failure
    /// status, no move to failed, no other mutation.
    Incompatible { path: Utf8PathBuf, message: String },
    /// A real processing error (IO, malformed current-version data, etc.).
    /// The outer loop should write a failure status and move the run to failed.
    Other(anyhow::Error),
}

impl fmt::Display for ProcessingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessingError::Incompatible { message, .. } => write!(f, "{message}"),
            ProcessingError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProcessingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProcessingError::Incompatible { .. } => None,
            ProcessingError::Other(e) => Some(e.as_ref()),
        }
    }
}

impl From<anyhow::Error> for ProcessingError {
    fn from(e: anyhow::Error) -> Self {
        ProcessingError::Other(e)
    }
}

pub(crate) enum EntryOutcome {
    Success {
        kind: ManifestEntryKind,
        local_path: String,
        relative_path: String,
        final_paths: Vec<String>,
        transform: Option<String>,
    },
    Failure {
        kind: ManifestEntryKind,
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
                local_path,
                relative_path,
                final_paths,
                transform,
            } => EntryStatusEntry {
                kind,
                local_path,
                relative_path,
                status: FileStatus::Imported,
                final_paths,
                transform,
                error: None,
            },
            EntryOutcome::Failure {
                kind,
                local_path,
                relative_path,
                error,
            } => EntryStatusEntry {
                kind,
                local_path,
                relative_path,
                status: FileStatus::Failed,
                final_paths: vec![],
                transform: None,
                error: Some(error),
            },
        }
    }
}

#[cfg(unix)]
fn prepare_work_entry(
    entry: &ManifestEntry,
    source_path: &Utf8Path,
    work_area: &Utf8Path,
) -> Result<Utf8PathBuf, String> {
    let work_path = work_area.join(entry.relative_path.as_str());

    match entry.kind {
        ManifestEntryKind::Directory => {
            if let Some(parent) = work_path.parent() {
                fs::create_dir_all(parent.as_std_path())
                    .map_err(|e| format!("failed to create work parent: {e}"))?;
            }
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

fn failed_entry(entry: &ManifestEntry, error: impl Into<String>) -> EntryOutcome {
    EntryOutcome::Failure {
        kind: entry.kind,
        local_path: entry.local_path.as_str().to_owned(),
        relative_path: entry.relative_path.as_str().to_owned(),
        error: error.into(),
    }
}

#[allow(clippy::too_many_arguments)]
fn process_manifest_entry(
    config: &ServerConfig,
    entry: &ManifestEntry,
    nickname: &Nickname,
    run_id: &RunId,
    processing_path: &Utf8Path,
    work_area: &Utf8Path,
    entry_index: usize,
    entry_total: usize,
    destination: &DestinationPath,
) -> EntryOutcome {
    let expected_staged = Utf8Path::new("files").join(entry.relative_path.as_str());
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

    let destination_root = destination.as_path();
    let final_path = destination.join(&entry.relative_path);
    if !path_is_within_root(&final_path, destination_root) {
        return failed_entry(
            entry,
            format!("final path escapes destination: {}", final_path.as_str()),
        );
    }
    let work_path = match prepare_work_entry(entry, &source_path, work_area) {
        Ok(p) => p,
        Err(error) => return failed_entry(entry, error.to_string()),
    };

    if entry.transform.is_none() {
        let final_destination = final_path.as_str().to_owned();
        return match commit_output_entry(&work_path, &final_path, destination_root, run_id) {
            Ok(_) => EntryOutcome::Success {
                kind: entry.kind,
                local_path: entry.local_path.as_str().to_owned(),
                relative_path: entry.relative_path.as_str().to_owned(),
                final_paths: vec![final_destination],
                transform: None,
            },
            Err(error) => failed_entry(entry, error),
        };
    }

    let Some(target_directory) = final_path.parent() else {
        return failed_entry(
            entry,
            format!("final path has no parent: {}", final_path.as_str()),
        );
    };
    let target_directory = target_directory.to_owned();

    let mut pp_helper = |update: &purgery_core::ProgressUpdate| {
        write_progress_best_effort(
            processing_path,
            nickname,
            run_id,
            update.state,
            update.entry_index,
            update.entry_total,
            update.current_entry,
            update.current_transform,
        );
    };
    let transform_name = entry.transform.as_deref().unwrap();
    let resolved = match config.transforms.get(transform_name) {
        Some(def) => ResolvedTransform {
            name: transform_name.to_owned(),
            def: def.clone(),
        },
        None => {
            return failed_entry(
                entry,
                format!("transform '{transform_name}' not defined on server"),
            )
        }
    };
    match apply_transform(
        &resolved,
        &work_path,
        destination_root,
        &target_directory,
        &mut pp_helper,
        entry_index,
        entry_total,
        entry.relative_path.as_str(),
    ) {
        Ok(outputs) => {
            let final_paths: Vec<String> = outputs.iter().map(|o| o.as_str().to_owned()).collect();
            EntryOutcome::Success {
                kind: entry.kind,
                local_path: entry.local_path.as_str().to_owned(),
                relative_path: entry.relative_path.as_str().to_owned(),
                final_paths,
                transform: entry.transform.clone(),
            }
        }
        Err(error) => failed_entry(entry, error),
    }
}

/// Read and validate run.toml and manifest.toml from a run directory,
/// checking version compatibility before any mutation.
///
/// Returns `Incompatible` if purgery_version is missing, malformed, or
/// major/minor-incompatible. Returns `Other` for IO errors or current-
/// version parse failures.
fn read_compatible_run_inputs(
    run_dir: &Utf8Path,
    nickname: &Nickname,
    run_id: &RunId,
) -> std::result::Result<(RunConfig, Manifest), ProcessingError> {
    let run_config_path = run_dir.join("run.toml");
    let run_config_content = fs::read_to_string(&run_config_path)
        .map_err(|e| ProcessingError::Other(anyhow::anyhow!("failed to read run config: {e}")))?;
    match purgery_core::check_toml_version(&run_config_content) {
        purgery_core::TomlVersionCheck::Compatible => {}
        purgery_core::TomlVersionCheck::Incompatible { reason, .. } => {
            return Err(ProcessingError::Incompatible {
                path: run_config_path,
                message: format!("incompatible run config version: {reason}"),
            });
        }
        purgery_core::TomlVersionCheck::InvalidToml { error } => {
            return Err(ProcessingError::Other(anyhow::anyhow!(
                "invalid run config TOML: {error}"
            )));
        }
    }
    let run_config = RunConfig::from_toml(&run_config_content)
        .map_err(|e| ProcessingError::Other(anyhow::anyhow!("failed to parse run config: {e}")))?;

    let manifest_path = run_dir.join("manifest.toml");
    let manifest_content = fs::read_to_string(&manifest_path)
        .map_err(|e| ProcessingError::Other(anyhow::anyhow!("failed to read manifest: {e}")))?;
    match purgery_core::check_toml_version(&manifest_content) {
        purgery_core::TomlVersionCheck::Compatible => {}
        purgery_core::TomlVersionCheck::Incompatible { reason, .. } => {
            return Err(ProcessingError::Incompatible {
                path: manifest_path,
                message: format!("incompatible manifest version: {reason}"),
            });
        }
        purgery_core::TomlVersionCheck::InvalidToml { error } => {
            return Err(ProcessingError::Other(anyhow::anyhow!(
                "invalid manifest TOML: {error}"
            )));
        }
    }
    let manifest = Manifest::from_toml(&manifest_content)
        .map_err(|e| ProcessingError::Other(anyhow::anyhow!("failed to parse manifest: {e}")))?;

    if let Err(e) = validate_envelope(nickname, run_id, &run_config, &manifest) {
        return Err(ProcessingError::Other(anyhow::anyhow!(
            "envelope validation failed: {e}"
        )));
    }

    Ok((run_config, manifest))
}

pub fn process_ready_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> std::result::Result<(), ProcessingError> {
    let ready_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Ready);

    // Preflight: check version compatibility before moving.
    // Incompatible ready runs stay in ready — no rename, no move to failed.
    if let Err(e) = read_compatible_run_inputs(&ready_path, nickname, run_id) {
        match &e {
            ProcessingError::Incompatible { .. } => return Err(e),
            // Malformed current-format TOML or IO error: move ready → failed
            // and write a failure status for operator visibility.
            ProcessingError::Other(_) => {
                let msg = format!("{e}");
                let failed_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Failed);
                if let Some(parent) = failed_path.parent() {
                    if let Err(err) = fs::create_dir_all(parent.as_std_path()) {
                        warn!(
                            nickname = %nickname.as_str(),
                            run_id = %run_id.as_str(),
                            error = %err,
                            "failed to create failed directory",
                        );
                    }
                }
                if let Err(rename_err) = fs::rename(&ready_path, failed_path.as_std_path()) {
                    warn!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        error = %rename_err,
                        "failed to move ready run to failed",
                    );
                }
                // Write failure status directly in the failed directory
                let status = purgery_core::RunStatus {
                    purgery_version: purgery_core::current_purgery_version().to_string(),
                    run_id: run_id.clone(),
                    nickname: nickname.clone(),
                    state: purgery_core::RunState::Failed,
                    entries: vec![],
                    error: Some(msg.clone()),
                };
                let status_toml = match status.to_toml() {
                    Ok(t) => t,
                    Err(ser_err) => {
                        warn!(
                            nickname = %nickname.as_str(),
                            run_id = %run_id.as_str(),
                            error = %ser_err,
                            "failed to serialize failure status",
                        );
                        return Err(e);
                    }
                };
                let final_status_path = failed_path.join("status.toml");
                let tmp_status_path = failed_path.join("status.toml.tmp");
                if let Some(parent) = final_status_path.parent() {
                    let _ = fs::create_dir_all(parent.as_std_path());
                }
                if let Err(write_err) = fs::write(&tmp_status_path, &status_toml) {
                    warn!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        error = %write_err,
                        "failed to write failure status",
                    );
                    return Err(e);
                }
                let _ = fs::rename(&tmp_status_path, &final_status_path);
                return Err(e);
            }
        }
    }

    // Compatible — claim the run by moving ready → processing.
    let processing_path = config
        .work_dir
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

pub fn process_processing_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> std::result::Result<(), ProcessingError> {
    let _span = span!(Level::INFO, "run", nickname = %nickname.as_str(), run_id = %run_id.as_str())
        .entered();
    let processing_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Processing);

    // Phase 1 — check version compatibility before any mutation
    let (run_config, manifest) =
        match read_compatible_run_inputs(&processing_path, nickname, run_id) {
            Ok(v) => v,
            Err(e @ ProcessingError::Incompatible { .. }) => return Err(e),
            Err(ProcessingError::Other(e)) => {
                let msg = format!("{e}");
                warn!("{}", msg);
                write_run_failure(&config.work_dir, nickname, run_id, &msg)?;
                return Err(ProcessingError::Other(e));
            }
        };

    // Phase 2 — version is compatible, mutate work area
    let work_area = work_dir(&config.work_dir, nickname, run_id);
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

    // Write initial progress before processing entries
    write_progress_best_effort(
        &processing_path,
        nickname,
        run_id,
        "processing_started",
        0,
        manifest.entries.len(),
        "",
        "",
    );

    let mut outcomes: Vec<EntryOutcome> = Vec::new();
    // Map: directory entry relative_path -> its outcome index.
    let mut dir_outcomes: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for (entry_idx, entry) in manifest.entries.iter().enumerate() {
        write_progress_best_effort(
            &processing_path,
            nickname,
            run_id,
            "processing_entry",
            entry_idx,
            manifest.entries.len(),
            entry.relative_path.as_str(),
            "",
        );

        outcomes.push(process_manifest_entry(
            config,
            entry,
            nickname,
            run_id,
            &processing_path,
            &work_area,
            entry_idx,
            manifest.entries.len(),
            &run_config.destination,
        ));

        if entry.kind == ManifestEntryKind::Directory {
            dir_outcomes.insert(entry.relative_path.as_str().to_owned(), outcomes.len() - 1);
        }
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

    // Best-effort publishing_status progress before terminal status publication
    write_progress_best_effort(
        &processing_path,
        nickname,
        run_id,
        "publishing_status",
        0,
        manifest.entries.len(),
        "",
        "",
    );

    info!(state = %run_state.as_str(), "run complete");
    let run_status = RunStatus {
        purgery_version: purgery_core::current_purgery_version().to_string(),
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

    finalize_processing_run(config, nickname, run_id, &run_state).map_err(ProcessingError::Other)
}

pub fn process_once_raw(config: &ServerConfig) -> Result<()> {
    if let Err(error) = run_gc(config) {
        warn!(error = %error, "opportunistic GC failed");
    }

    let processing_runs = crate::phases::find_processing_runs(&config.work_dir)?;
    let ready_runs = crate::phases::find_ready_runs(&config.work_dir)?;
    if processing_runs.is_empty() && ready_runs.is_empty() {
        info!("no ready or processing runs found");
        return Ok(());
    }

    for (nickname, run_id) in &processing_runs {
        match recover_or_process_processing_run(config, nickname, run_id) {
            Ok(()) => {}
            Err(RecoveryError::IncompatibleStatus { message }) => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    error = %message,
                    "processing run has incompatible status; leaving in place for operator inspection"
                );
            }
            Err(RecoveryError::Other(error)) => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    phase = "processing",
                    error = %error,
                    "processing run recovery failed"
                );
                let processing_path =
                    config
                        .work_dir
                        .run_dir(nickname, run_id, RunPhase::Processing);
                if processing_path.exists() {
                    write_run_failure(
                        &config.work_dir,
                        nickname,
                        run_id,
                        &format!("processing recovery failed: {error}"),
                    )?;
                }
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
        match process_ready_run(config, nickname, run_id) {
            Ok(()) => {}
            Err(ProcessingError::Incompatible { message, .. }) => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    error = %message,
                    "ready run has incompatible state; leaving in place",
                );
            }
            Err(ProcessingError::Other(error)) => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    phase = "processing",
                    error = %error,
                    "run failed"
                );
                move_to_failed(&config.work_dir, nickname, run_id)?;
            }
        }
    }

    Ok(())
}

/// Process a specific run by nickname and run_id.
///
/// First starts opportunistic global GC in a background thread
/// (best-effort — failure does not block target processing).
/// Then dispatches based on the run's current phase:
/// - `ready`: process via `process_ready_run`
/// - `processing`: recover via `recover_or_process_processing_run`
/// - terminal: idempotent success
/// - not found: error
///
/// Does not process unrelated runs.
pub fn process_run_target(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<()> {
    // Start opportunistic GC best-effort in the background.
    let gc_config = config.clone();
    std::thread::spawn(move || {
        if let Err(error) = run_gc(&gc_config) {
            warn!(error = %error, "opportunistic GC failed");
        }
    });

    // Check run phases in order: ready, processing, then terminal.
    let ready_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Ready);
    if ready_path.exists() {
        return process_ready_run(config, nickname, run_id).map_err(|e| match e {
            ProcessingError::Incompatible { message, .. } => {
                anyhow::anyhow!("incompatible run left in place: {message}")
            }
            ProcessingError::Other(e) => e,
        });
    }

    let processing_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Processing);
    if processing_path.exists() {
        return recover_or_process_processing_run(config, nickname, run_id).map_err(|e| match e {
            RecoveryError::IncompatibleStatus { message } => {
                anyhow::anyhow!("incompatible run left in place: {message}")
            }
            RecoveryError::Other(e) => e,
        });
    }

    // Check terminal phases.
    let terminal_phases = [RunPhase::Done, RunPhase::Failed];
    for phase in &terminal_phases {
        let dir = config.work_dir.run_dir(nickname, run_id, *phase);
        if dir.exists() {
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = %phase.as_str(),
                "run already terminal",
            );
            return Ok(());
        }
    }

    anyhow::bail!(
        "run {}/{} not found in ready, processing, or terminal phases",
        nickname.as_str(),
        run_id.as_str(),
    )
}
