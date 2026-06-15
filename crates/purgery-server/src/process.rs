use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    path_is_within_root, validate_envelope, work_dir, DestinationPath, EntryStatusEntry,
    FileStatus, Manifest, ManifestEntry, ManifestEntryKind, Nickname, NormalizedRelativePath,
    RunConfig, RunId, RunPhase, RunState, RunStatus, ServerConfig,
};
use std::fs;
use tracing::{info, span, warn, Level};

use crate::commit::commit_output_entry;
use crate::gc::run_gc;
use crate::phases::{
    finalize_processing_run, move_to_failed, write_progress_best_effort, write_run_failure,
};
use crate::recover::recover_or_process_processing_run;
use crate::transform::apply_transforms;
use crate::RunPlan;

pub(crate) enum EntryOutcome {
    Success {
        kind: ManifestEntryKind,
        local_path: String,
        relative_path: String,
        final_paths: Vec<String>,
        transform: Option<Vec<String>>,
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
    run_plan: &RunPlan,
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

    // No steps → commit the original work path directly (identity transform).
    if entry.transform_steps.is_empty() {
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

    let mut pp_helper = |update: &purgery_core::ProgressUpdate| {
        write_progress_best_effort(
            processing_path,
            nickname,
            run_id,
            update.state,
            update.entry_index,
            update.entry_total,
            update.current_entry,
            update.current_step,
        );
    };
    let resolved_steps = match run_plan.resolve_steps(&entry.transform_steps) {
        Ok(s) => s,
        Err(error) => return failed_entry(entry, error),
    };
    let final_destination = final_path.clone();
    match apply_transforms(
        &resolved_steps,
        &work_path,
        &final_destination,
        &mut pp_helper,
        entry_index,
        entry_total,
        entry.relative_path.as_str(),
    ) {
        Ok(outputs) => {
            let mut final_paths = Vec::new();
            for output in outputs {
                let output_final = if output == work_path {
                    final_destination.clone()
                } else {
                    let filename = output.file_name().unwrap_or("");
                    final_destination.parent().map_or_else(
                        || Utf8PathBuf::from(filename),
                        |parent| parent.join(filename),
                    )
                };
                if !path_is_within_root(&output_final, destination_root) {
                    return failed_entry(entry, "output escapes root");
                }
                let output_relative = if output == work_path {
                    entry.relative_path.clone()
                } else {
                    let filename = output.file_name().unwrap_or("");
                    let relative = entry.relative_path.as_path().parent().map_or_else(
                        || Utf8PathBuf::from(filename),
                        |parent| parent.join(filename),
                    );
                    match NormalizedRelativePath::new(relative) {
                        Ok(path) => path,
                        Err(error) => {
                            return failed_entry(entry, format!("invalid output path: {error}"))
                        }
                    }
                };
                final_paths.push(destination.join(&output_relative).as_str().to_owned());
            }
            let steps: Vec<String> = entry.transform_steps.clone();
            EntryOutcome::Success {
                kind: entry.kind,
                local_path: entry.local_path.as_str().to_owned(),
                relative_path: entry.relative_path.as_str().to_owned(),
                final_paths,
                transform: (!steps.is_empty()).then_some(steps),
            }
        }
        Err(error) => failed_entry(entry, error),
    }
}

pub fn process_ready_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    let ready_path = config.work_dir.run_dir(nickname, run_id, RunPhase::Ready);
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
) -> Result<()> {
    let _span = span!(Level::INFO, "run", nickname = %nickname.as_str(), run_id = %run_id.as_str())
        .entered();
    let processing_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Processing);

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

    let run_config_path = processing_path.join("run.toml");
    let run_config_content = match fs::read_to_string(&run_config_path) {
        Ok(content) => content,
        Err(error) => {
            let msg = format!("failed to read run config: {error}");
            warn!("{}", msg);
            write_run_failure(&config.work_dir, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };
    let run_config = match RunConfig::from_toml(&run_config_content) {
        Ok(run_config) => run_config,
        Err(error) => {
            let msg = format!("failed to parse run config: {error}");
            warn!("{}", msg);
            write_run_failure(&config.work_dir, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };

    let run_plan = match RunPlan::build(config) {
        Ok(plan) => plan,
        Err(error) => {
            let msg = format!("run plan validation failed: {error}");
            warn!("{}", msg);
            write_run_failure(&config.work_dir, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };

    let manifest_path = processing_path.join("manifest.toml");
    let manifest_content = match fs::read_to_string(&manifest_path) {
        Ok(content) => content,
        Err(error) => {
            let msg = format!("failed to read manifest: {error}");
            warn!("{}", msg);
            write_run_failure(&config.work_dir, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };
    let manifest = match Manifest::from_toml(&manifest_content) {
        Ok(manifest) => manifest,
        Err(error) => {
            let msg = format!("failed to parse manifest: {error}");
            warn!("{}", msg);
            write_run_failure(&config.work_dir, nickname, run_id, &msg)?;
            anyhow::bail!("{msg}");
        }
    };

    if let Err(error) = validate_envelope(nickname, run_id, &run_config, &manifest) {
        let msg = format!("envelope validation failed: {error}");
        warn!("{}", msg);
        write_run_failure(&config.work_dir, nickname, run_id, &msg)?;
        anyhow::bail!("{msg}");
    }

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
            &run_plan,
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
        if let Err(error) = recover_or_process_processing_run(config, nickname, run_id) {
            warn!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = "processing",
                error = %error,
                "processing run recovery failed"
            );
            let processing_path = config
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
            move_to_failed(&config.work_dir, nickname, run_id)?;
        }
    }

    Ok(())
}
