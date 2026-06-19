use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    path_is_within_root, validate_envelope, work_dir, DestinationPath, EntryStatusEntry,
    FileStatus, Manifest, ManifestEntry, ManifestEntryKind, Nickname, NormalizedRelativePath,
    ProcessRunOutcome, RunConfig, RunId, RunPhase, RunState, RunStatus, ServerConfig,
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

/// Outcome of attempting to claim a ready run.
///
/// Used by both targeted `process-run` and batch `process-once`.
#[derive(Debug)]
pub(crate) enum ReadyClaimOutcome {
    /// Ready was successfully renamed to processing. The caller holds
    /// the processor lock and must call `process_processing_run`.
    Claimed(crate::phases::ProcessingRunLock),
    /// Ready did not exist but processing did (another process claimed it).
    AlreadyProcessing,
    /// Ready did not exist and a terminal phase was found.
    AlreadyTerminal,
    /// Ready exists but its processor lock is held by another process.
    /// This is a normal concurrent-claim race, not corruption.
    ActiveClaimer,
    /// Ready exists but has incompatible version metadata. Left in place.
    IncompatibleReady { message: String },
    /// Ready exists but is malformed current-format state. Successfully
    /// moved to failed with atomically published failure status.
    MalformedReadyMovedToFailed { error: anyhow::Error },
    /// Ready exists but is malformed current-format state, and the
    /// move-to-failed or status publication failed.
    MalformedReadyMoveFailed {
        original_error: anyhow::Error,
        publish_error: anyhow::Error,
    },
    /// Ready was not found anywhere (no ready, processing, or terminal phase).
    NotFound,
    /// Ready existed but claim (rename) failed for an unexpected reason,
    /// and the current state could not be determined.
    ClaimFailed { error: anyhow::Error },
}

/// Claim a ready run.  The processor lock is acquired before any
/// read or mutation of the ready directory so that all ready-run
/// mutation, including malformed-ready failure publication, is
/// protected by the lock.
///
/// Returns `Claimed(lock)` on success — the caller MUST run
/// `process_processing_run` while holding the lock.
///
/// For all other outcomes the caller should not attempt to process the run.
pub(crate) fn claim_ready_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> ReadyClaimOutcome {
    let ready_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Ready);

    if !ready_path.exists() {
        return recheck_state(config, nickname, run_id);
    }

    // Acquire the processor lock BEFORE reading or mutating the ready
    // directory.  This ensures all ready-run mutation is protected.
    let lock = match crate::phases::try_lock_existing_run_dir_processor(&ready_path) {
        Ok(crate::phases::ProcessorLockAttempt::Acquired(l)) => l,
        Ok(crate::phases::ProcessorLockAttempt::Busy) => {
            return ReadyClaimOutcome::ActiveClaimer;
        }
        Ok(crate::phases::ProcessorLockAttempt::Missing) => {
            return recheck_state(config, nickname, run_id);
        }
        Err(e) => {
            return ReadyClaimOutcome::ClaimFailed {
                error: anyhow::anyhow!("failed to acquire processor lock: {e}"),
            };
        }
    };

    // Lock acquired — now read inputs.
    match read_compatible_run_inputs(&ready_path, nickname, run_id) {
        Err(ProcessingError::Incompatible { message, .. }) => {
            drop(lock);
            return ReadyClaimOutcome::IncompatibleReady { message };
        }
        Err(ProcessingError::Other(e)) => {
            return handle_malformed_ready(config, nickname, run_id, e, lock);
        }
        Ok(_) => {}
    }

    // Compatible and locked — claim by renaming ready → processing.
    let processing_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Processing);
    if let Some(parent) = processing_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ReadyClaimOutcome::ClaimFailed {
                error: anyhow::anyhow!("failed to create processing parent: {e}"),
            };
        }
    }
    match fs::rename(&ready_path, &processing_path) {
        Ok(()) => ReadyClaimOutcome::Claimed(lock),
        Err(e) => {
            warn!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                error = %e,
                "failed to claim ready run (race); re-checking state",
            );
            drop(lock);
            recheck_state(config, nickname, run_id)
        }
    }
}

/// Move a malformed (but version-present/current-format) ready run to
/// failed, publishing a failure status atomically.
///
/// Returns `MalformedReadyMovedToFailed` only if both the rename and
/// status publication succeed. Returns `MalformedReadyMoveFailed` if
/// either step fails.
///
/// Writes the status before the directory rename so that a failure to
/// publish the status leaves the run safely in `ready/` — never in
/// `failed/` without a valid status.
fn handle_malformed_ready(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    original_error: anyhow::Error,
    _lock: crate::phases::ProcessingRunLock,
) -> ReadyClaimOutcome {
    let ready_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Ready);
    let failed_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Failed);

    let status = purgery_core::RunStatus {
        purgery_version: purgery_core::current_purgery_version().to_string(),
        run_id: run_id.clone(),
        nickname: nickname.clone(),
        state: purgery_core::RunState::Failed,
        entries: vec![],
        error: Some(original_error.to_string()),
    };

    // Publish status inside the ready directory first.  If this fails,
    // the run is still safely in ready/ and nothing has moved.
    if let Err(e) = crate::phases::publish_status_atomic(&ready_path, &status) {
        // Clean up temp file that atomic write may have created.
        let _ = fs::remove_file(ready_path.join("status.toml.tmp"));
        return ReadyClaimOutcome::MalformedReadyMoveFailed {
            original_error,
            publish_error: anyhow::anyhow!("failed to write failure status to ready: {e}"),
        };
    }

    // Now atomically rename the entire ready directory to failed.
    // The status.toml we just wrote comes along with the rename.
    if let Some(parent) = failed_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ReadyClaimOutcome::MalformedReadyMoveFailed {
                original_error,
                publish_error: anyhow::anyhow!("failed to create failed parent: {e}"),
            };
        }
    }
    if let Err(e) = fs::rename(&ready_path, failed_path.as_std_path()) {
        // Rename failed — clean up the status file we wrote in ready.
        let _ = fs::remove_file(ready_path.join("status.toml"));
        let _ = fs::remove_file(ready_path.join("status.toml.tmp"));
        return ReadyClaimOutcome::MalformedReadyMoveFailed {
            original_error,
            publish_error: anyhow::anyhow!("failed to move malformed ready to failed: {e}"),
        };
    }

    // After renaming to failed, terminal dirs must not retain processor.lock.
    // The lock was acquired in ready and came along with the rename.
    let _ = fs::remove_file(failed_path.join("processor.lock"));

    ReadyClaimOutcome::MalformedReadyMovedToFailed {
        error: original_error,
    }
}

/// Re-check run state after determining that ready is absent or claim
/// failed.  Used internally by `claim_ready_run`.
fn recheck_state(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> ReadyClaimOutcome {
    let processing_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Processing);
    if processing_path.exists() {
        return ReadyClaimOutcome::AlreadyProcessing;
    }
    for phase in &[RunPhase::Done, RunPhase::Failed] {
        let dir = config.work_dir.run_dir(nickname, run_id, *phase);
        if dir.exists() {
            return ReadyClaimOutcome::AlreadyTerminal;
        }
    }
    let ready_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Ready);
    if ready_path.exists() {
        return ReadyClaimOutcome::ClaimFailed {
            error: anyhow::anyhow!(
                "failed to claim run {}/{}: ready still exists but rename failed",
                nickname.as_str(),
                run_id.as_str(),
            ),
        };
    }
    ReadyClaimOutcome::NotFound
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
        match recover_processing_run_if_unlocked(config, nickname, run_id) {
            Ok(ProcessingTargetOutcome::Recovered) => {}
            Ok(ProcessingTargetOutcome::ActiveProcessor) => {
                info!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    "processing run is locked by another processor; skipping",
                );
            }
            Ok(ProcessingTargetOutcome::Incompatible { message }) => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    error = %message,
                    "processing run has incompatible status; leaving in place for operator inspection"
                );
            }
            Ok(ProcessingTargetOutcome::NotFound) => {
                info!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    "processing run disappeared before recovery",
                );
            }
            Ok(ProcessingTargetOutcome::FailedPublished { error }) => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    error = %error,
                    "processing run recovery failed; failure status published while holding processor lock",
                );
            }
            Ok(ProcessingTargetOutcome::FailurePublishFailed {
                recovery_error,
                publish_error,
            }) => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    recovery_error = %recovery_error,
                    publish_error = %publish_error,
                    "processing run recovery failed and failure status could not be published while holding processor lock",
                );
            }
            Err(e) => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    error = %e,
                    "failed to check processor lock for processing run",
                );
                // Lock setup/check error is not a recovery error — do not
                // move the run to failed.  Log and skip.
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
        match claim_ready_run(config, nickname, run_id) {
            ReadyClaimOutcome::Claimed(lock) => {
                let result = process_processing_run(config, nickname, run_id);
                if let Err(e) = result {
                    warn!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        phase = "processing",
                        error = %e,
                        "run failed"
                    );
                    // Lock is still held here — must not drop before mutation is complete.
                    if let Err(move_err) = move_to_failed(&config.work_dir, nickname, run_id) {
                        warn!(
                            nickname = %nickname.as_str(),
                            run_id = %run_id.as_str(),
                            error = %move_err,
                            "also failed to move run to failed",
                        );
                    }
                }
                drop(lock);
            }
            ReadyClaimOutcome::AlreadyProcessing => {
                info!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    "ready run was claimed by another processor; skipping",
                );
            }
            ReadyClaimOutcome::AlreadyTerminal => {
                info!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    "ready run already completed by another processor",
                );
            }
            ReadyClaimOutcome::IncompatibleReady { message } => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    error = %message,
                    "ready run has incompatible state; leaving in place",
                );
            }
            ReadyClaimOutcome::MalformedReadyMovedToFailed { .. } => {
                info!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    "malformed ready run moved to failed",
                );
            }
            ReadyClaimOutcome::MalformedReadyMoveFailed {
                original_error,
                publish_error,
            } => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    original_error = %original_error,
                    publish_error = %publish_error,
                    "malformed ready run could not be fully published as failed; \
                     run may require operator inspection",
                );
            }
            ReadyClaimOutcome::NotFound => {
                info!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    "ready run disappeared before we could claim it",
                );
            }
            ReadyClaimOutcome::ActiveClaimer => {
                info!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    "ready run is being claimed by another processor; skipping",
                );
            }
            ReadyClaimOutcome::ClaimFailed { error } => {
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    error = %error,
                    "failed to claim ready run",
                );
            }
        }
    }

    Ok(())
}

/// Outcome of attempting to recover a processing run via the shared
/// lock-aware helper.
///
/// Used by both targeted `process-run` and batch `process-once`.
#[derive(Debug)]
pub(crate) enum ProcessingTargetOutcome {
    /// Processing run was successfully recovered to terminal.
    Recovered,
    /// Processing run is locked by another active processor.
    ActiveProcessor,
    /// Processing directory has an incompatible status that must be
    /// left in place.
    Incompatible { message: String },
    /// Processing directory does not exist (race: it was removed
    /// between discovery and lock attempt).
    NotFound,
    /// Recovery failed; failure status was published while the
    /// processor lock was still held.
    FailedPublished { error: anyhow::Error },
    /// Recovery failed and publishing the failure status also
    /// failed. Both happened while the lock was still held.
    FailurePublishFailed {
        recovery_error: anyhow::Error,
        publish_error: anyhow::Error,
    },
}

/// Try to recover a processing run, but only if the processor lock is
/// free.
///
/// If the lock is busy, another process owns the run and we must not
/// touch it (`ActiveProcessor`).  If the lock is acquired, recovery
/// proceeds while the lock is held.  Lock setup/check errors are
/// returned as errors, not swallowed.
///
/// This is the single shared helper for:
/// - `process_run_target` (targeted processing)
/// - `process_once_raw` (batch processing)
pub(crate) fn recover_processing_run_if_unlocked(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<ProcessingTargetOutcome> {
    let processing_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Processing);

    if !processing_path.exists() {
        return Ok(ProcessingTargetOutcome::NotFound);
    }

    let lock = match crate::phases::try_lock_existing_run_dir_processor(&processing_path) {
        Ok(crate::phases::ProcessorLockAttempt::Acquired(l)) => l,
        Ok(crate::phases::ProcessorLockAttempt::Busy) => {
            return Ok(ProcessingTargetOutcome::ActiveProcessor);
        }
        Ok(crate::phases::ProcessorLockAttempt::Missing) => {
            return Ok(ProcessingTargetOutcome::NotFound);
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to check processor lock on processing run {}/{}: {e}",
                nickname.as_str(),
                run_id.as_str(),
            ));
        }
    };

    info!(
        nickname = %nickname.as_str(),
        run_id = %run_id.as_str(),
        "acquired processing run lock; recovering",
    );

    let result = recover_or_process_processing_run(config, nickname, run_id);

    let outcome = match result {
        Ok(()) => ProcessingTargetOutcome::Recovered,

        Err(RecoveryError::IncompatibleStatus { message }) => {
            ProcessingTargetOutcome::Incompatible { message }
        }

        Err(RecoveryError::Other(recovery_error)) => {
            // Still holding the processor lock here.  Failure
            // publication must complete before the lock is dropped
            // so no other process sees an unlocked processing run.
            let processing_path = config
                .work_dir
                .run_dir(nickname, run_id, RunPhase::Processing);

            if processing_path.exists() {
                match write_run_failure(
                    &config.work_dir,
                    nickname,
                    run_id,
                    &format!("processing recovery failed: {recovery_error}"),
                ) {
                    Ok(()) => ProcessingTargetOutcome::FailedPublished {
                        error: recovery_error,
                    },
                    Err(publish_error) => ProcessingTargetOutcome::FailurePublishFailed {
                        recovery_error,
                        publish_error,
                    },
                }
            } else {
                ProcessingTargetOutcome::FailedPublished {
                    error: recovery_error,
                }
            }
        }
    };

    drop(lock);
    Ok(outcome)
}

/// Process a specific run by nickname and run_id.
///
/// Foreground targeted processor:
/// - ready: claim with processor lock and process
/// - processing: recover if lock is free; no-op if actively locked
/// - terminal: idempotent success
/// - not found: error
///
/// Does not process unrelated runs.
/// Does not run GC.
/// Does not detach.
pub fn process_run_target(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<purgery_core::ProcessRunResponse> {
    use purgery_core::ProcessRunResponse;

    let result = process_run_inner(config, nickname, run_id)?;

    Ok(ProcessRunResponse {
        protocol_version: purgery_core::PROTOCOL_VERSION,
        purgery_version: purgery_core::current_purgery_version().to_string(),
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        outcome: result.outcome.as_str().to_owned(),
        run_phase: result.run_phase,
        status_state: result.status_state,
        message: result.message,
    })
}

/// Structured result from `process_run_inner` that maps directly to
/// `ProcessRunResponse`.  For terminal outcomes (`Processed`,
/// `AlreadyTerminal`) both `run_phase` and `status_state` are populated
/// from verified terminal status.  For nonterminal outcomes
/// `status_state` is `None`.
struct ProcessRunInnerResult {
    outcome: ProcessRunOutcome,
    run_phase: Option<String>,
    status_state: Option<String>,
    message: Option<String>,
}

/// Build a terminal `ProcessRunInnerResult` from verified terminal status.
/// Fails if terminal status cannot be verified (missing, malformed,
/// incompatible, or envelope-mismatched).
fn terminal_response(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    outcome: ProcessRunOutcome,
    message: String,
) -> Result<ProcessRunInnerResult> {
    let Some((run_phase, status_state)) = verified_terminal_status(config, nickname, run_id)?
    else {
        anyhow::bail!(
            "run {}/{} expected terminal status after outcome {} but no verified terminal status exists",
            nickname.as_str(),
            run_id.as_str(),
            outcome.as_str(),
        );
    };
    Ok(ProcessRunInnerResult {
        outcome,
        run_phase: Some(run_phase),
        status_state: Some(status_state),
        message: Some(message),
    })
}

/// Inner dispatch.  Returns a `ProcessRunInnerResult`.
fn process_run_inner(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<ProcessRunInnerResult> {
    // 1. Try to claim from ready.
    match claim_ready_run(config, nickname, run_id) {
        ReadyClaimOutcome::Claimed(lock) => {
            let result = process_processing_run(config, nickname, run_id);
            let inner = match result {
                Ok(()) => terminal_response(
                    config,
                    nickname,
                    run_id,
                    ProcessRunOutcome::Processed,
                    "run processed successfully".to_string(),
                ),
                Err(ProcessingError::Incompatible { message, .. }) => {
                    anyhow::bail!("incompatible run in processing: {message}")
                }
                Err(ProcessingError::Other(error)) => {
                    warn!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        phase = "processing",
                        error = %error,
                        "run failed",
                    );
                    match move_to_failed(&config.work_dir, nickname, run_id) {
                        Ok(()) => terminal_response(
                            config,
                            nickname,
                            run_id,
                            ProcessRunOutcome::Processed,
                            error.to_string(),
                        ),
                        Err(move_err) => anyhow::bail!(
                            "{error}; failed to move target run to failed: {move_err}"
                        ),
                    }
                }
            };
            drop(lock);
            return inner;
        }
        ReadyClaimOutcome::ActiveClaimer => {
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                "ready run is being claimed by another processor",
            );
            return Ok(ProcessRunInnerResult {
                outcome: ProcessRunOutcome::ClaimInProgress,
                run_phase: Some("ready".to_string()),
                status_state: None,
                message: Some("another process owns the ready processor lock".to_string()),
            });
        }
        ReadyClaimOutcome::AlreadyProcessing => {
            return handle_process_run_processing_outcome(config, nickname, run_id);
        }
        ReadyClaimOutcome::AlreadyTerminal => {
            match verified_terminal_status(config, nickname, run_id)? {
                Some((run_phase, status_state)) => {
                    return Ok(ProcessRunInnerResult {
                        outcome: ProcessRunOutcome::AlreadyTerminal,
                        run_phase: Some(run_phase),
                        status_state: Some(status_state),
                        message: Some("run was already in a terminal phase".to_string()),
                    });
                }
                None => anyhow::bail!(
                    "run {}/{} reported as terminal by claim ready but no verified terminal status found",
                    nickname.as_str(),
                    run_id.as_str(),
                ),
            }
        }
        ReadyClaimOutcome::IncompatibleReady { message } => {
            anyhow::bail!("incompatible run left in place: {message}")
        }
        ReadyClaimOutcome::MalformedReadyMovedToFailed { error } => {
            let term_phase = detect_terminal_run_phase(config, nickname, run_id);
            let status_state = read_terminal_status_state(config, nickname, run_id);
            return Ok(ProcessRunInnerResult {
                outcome: ProcessRunOutcome::Processed,
                run_phase: Some(term_phase),
                status_state,
                message: Some(format!("malformed ready run moved to failed: {error}")),
            });
        }
        ReadyClaimOutcome::MalformedReadyMoveFailed { .. } => {
            anyhow::bail!("malformed ready run could not be moved to failed")
        }
        ReadyClaimOutcome::NotFound => { /* fall through */ }
        ReadyClaimOutcome::ClaimFailed { error } => anyhow::bail!("{error}"),
    }

    // 2. Not ready — check processing.
    let processing_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Processing);
    if processing_path.exists() {
        return handle_process_run_processing_outcome(config, nickname, run_id);
    }

    // 3. Check terminal phases — must use verified terminal status, not
    //    directory existence alone.
    match verified_terminal_status(config, nickname, run_id)? {
        Some((run_phase, status_state)) => {
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = %run_phase,
                "run already terminal",
            );
            Ok(ProcessRunInnerResult {
                outcome: ProcessRunOutcome::AlreadyTerminal,
                run_phase: Some(run_phase),
                status_state: Some(status_state),
                message: Some("run was already in a terminal phase".to_string()),
            })
        }
        None => {
            anyhow::bail!(
                "run {}/{} not found in ready, processing, or terminal phases",
                nickname.as_str(),
                run_id.as_str(),
            )
        }
    }
}

/// Like `handle_process_run_processing` but returns structured outcomes.
fn handle_process_run_processing_outcome(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<ProcessRunInnerResult> {
    match recover_processing_run_if_unlocked(config, nickname, run_id) {
        Ok(ProcessingTargetOutcome::Recovered) => terminal_response(
            config,
            nickname,
            run_id,
            ProcessRunOutcome::Processed,
            "processing run recovered and completed".to_string(),
        ),
        Ok(ProcessingTargetOutcome::ActiveProcessor) => Ok(ProcessRunInnerResult {
            outcome: ProcessRunOutcome::AlreadyActive,
            run_phase: Some("processing".to_string()),
            status_state: None,
            message: Some("processor lock is held by another process".to_string()),
        }),
        Ok(ProcessingTargetOutcome::FailedPublished { error }) => terminal_response(
            config,
            nickname,
            run_id,
            ProcessRunOutcome::Processed,
            format!("processing recovery failed: {error}"),
        ),
        Ok(ProcessingTargetOutcome::Incompatible { message }) => {
            anyhow::bail!("abandoned processing run is incompatible: {message}")
        }
        Ok(ProcessingTargetOutcome::NotFound) => {
            match verified_terminal_status(config, nickname, run_id)? {
                Some((run_phase, status_state)) => Ok(ProcessRunInnerResult {
                    outcome: ProcessRunOutcome::AlreadyTerminal,
                    run_phase: Some(run_phase),
                    status_state: Some(status_state),
                    message: Some("processing run completed before recovery".to_string()),
                }),
                None => anyhow::bail!(
                    "run {}/{} disappeared from processing but no verified terminal status exists",
                    nickname.as_str(),
                    run_id.as_str(),
                ),
            }
        }
        Ok(ProcessingTargetOutcome::FailurePublishFailed { .. }) => {
            anyhow::bail!("processing recovery failed and could not publish failure")
        }
        Err(e) => Err(e),
    }
}

/// Detect the filesystem/protocol terminal phase: returns the directory
/// phase name ("done" or "failed") if the directory exists, or empty
/// string if neither exists.  This is purely directory-based and never
/// returns "partial".
fn detect_terminal_run_phase(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> String {
    for (phase, name) in &[(RunPhase::Done, "done"), (RunPhase::Failed, "failed")] {
        let dir = config.work_dir.run_dir(nickname, run_id, *phase);
        if dir.exists() {
            return name.to_string();
        }
    }
    String::new()
}

/// Read the terminal `RunStatus.state` from `status.toml`.  Returns
/// `Some("done"|"partial"|"failed")` when readable, `None` otherwise.
/// This is not authoritative — use `verified_terminal_status` for that.
fn read_terminal_status_state(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Option<String> {
    for phase in &[RunPhase::Done, RunPhase::Failed] {
        let dir = config.work_dir.run_dir(nickname, run_id, *phase);
        if !dir.exists() {
            continue;
        }
        let status_path = dir.join("status.toml");
        if let Ok(content) = fs::read_to_string(status_path.as_std_path()) {
            if let Ok(status) = purgery_core::RunStatus::from_toml(&content) {
                return Some(status.state.as_str().to_owned());
            }
        }
    }
    None
}

/// Verify that a terminal run has authoritative terminal status.
///
/// Returns the directory phase (`"done"` or `"failed"`) and the
/// `RunStatus.state` (`"done"`, `"partial"`, or `"failed"`) only when:
///
/// * a terminal directory exists;
/// * `status.toml` is readable and parseable;
/// * `purgery_version` is compatible;
/// * nickname/run_id envelope matches.
///
/// Errors if the directory exists but any of the above is missing or
/// invalid.  Returns `None` if neither terminal directory exists.
fn verified_terminal_status(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<Option<(String, String)>> {
    for phase in &[RunPhase::Done, RunPhase::Failed] {
        let dir = config.work_dir.run_dir(nickname, run_id, *phase);
        if !dir.exists() {
            continue;
        }
        let phase_name = phase.as_str().to_owned();
        let status_path = dir.join("status.toml");
        let content = fs::read_to_string(status_path.as_std_path()).with_context(|| {
            format!(
                "terminal directory exists for run {}/{} in phase {} \
                 but status.toml is unreadable",
                nickname.as_str(),
                run_id.as_str(),
                phase_name,
            )
        })?;
        let status = purgery_core::RunStatus::from_toml(&content).with_context(|| {
            format!(
                "terminal status.toml is invalid for run {}/{} in phase {}",
                nickname.as_str(),
                run_id.as_str(),
                phase_name,
            )
        })?;
        purgery_core::require_compatible_purgery_version(
            &status.purgery_version,
            "terminal status",
        )
        .with_context(|| {
            format!(
                "incompatible terminal status version for run {}/{} in phase {}",
                nickname.as_str(),
                run_id.as_str(),
                phase_name,
            )
        })?;
        // Validate envelope: nickname/run_id must match.
        if status.nickname != *nickname || status.run_id != *run_id {
            anyhow::bail!(
                "terminal status envelope mismatch for run {}/{} in phase {}: \
                 status has nickname={}, run_id={}",
                nickname.as_str(),
                run_id.as_str(),
                phase_name,
                status.nickname.as_str(),
                status.run_id.as_str(),
            );
        }
        let status_state = status.state.as_str().to_owned();
        return Ok(Some((phase_name, status_state)));
    }
    Ok(None)
}
