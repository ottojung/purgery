use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    path_is_within_root, validate_envelope, work_dir, EntryStatusEntry, FileStatus, Manifest,
    ManifestEntry, ManifestEntryKind, Nickname, NormalizedRelativePath, RunConfig, RunConfigSync,
    RunId, RunPhase, RunState, RunStatus, ServerConfig,
};
use std::collections::HashMap;
use std::fs;
use tracing::{debug, info, span, warn, Level};

use crate::commit::{
    commit_directory_entry, commit_output_entry, commit_regular_file_entry, commit_symlink_entry,
};
use crate::gc::run_gc;
use crate::phases::{
    finalize_processing_run, move_to_failed, write_progress_best_effort, write_run_failure,
};
use crate::postprocess::apply_postprocessing;
use crate::recover::recover_or_process_processing_run;
use crate::RunPlan;

pub(crate) enum EntryOutcome {
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
        sync_name: entry.sync_name.clone(),
        local_path: entry.local_path.as_str().to_owned(),
        relative_path: entry.relative_path.as_str().to_owned(),
        error: error.into(),
    }
}

pub(crate) fn planned_entry_outputs(
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

    let normalized_path = entry.relative_path.as_str().to_owned();

    let synthetic_work_path = Utf8Path::new(entry.relative_path.as_str());

    let mut outputs: Vec<String> = Vec::new();

    if let Some(rule) = run_plan.first_matching_rule(entry.sync_name.as_str(), &normalized_path) {
        for step in &rule.steps {
            if step.step_def.keep_original {
                outputs.push(entry_final_path.as_str().to_owned());
            }
            for pat in &step.step_def.expected_outputs {
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

    if outputs.is_empty() {
        outputs.push(entry_final_path.as_str().to_owned());
    }

    let mut seen = std::collections::HashSet::new();
    outputs.retain(|p| seen.insert(p.clone()));
    outputs
}

pub(crate) fn validate_unique_final_paths(
    server_config: &ServerConfig,
    nickname: &Nickname,
    run_config: &RunConfig,
    manifest: &Manifest,
    run_plan: &RunPlan,
    covered_indices: &std::collections::HashSet<usize>,
) -> Result<(), String> {
    let sync_map: HashMap<&str, &RunConfigSync> = run_config.sync_map().into_iter().collect();
    let mut destinations: HashMap<String, &ManifestEntry> = HashMap::new();

    for (entry_idx, entry) in manifest.entries.iter().enumerate() {
        if covered_indices.contains(&entry_idx) {
            continue;
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

    let entries_with_rules: std::collections::HashSet<usize> = manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            sync_map
                .get(e.sync_name.as_str())
                .map(|_sync| {
                    let np = e.relative_path.as_str().to_owned();
                    let sync = e.sync_name.as_str();
                    run_plan
                        .rules
                        .iter()
                        .any(|r| r.applies_to(sync) && r.is_match(&np))
                })
                .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();

    for (i, entry_a) in manifest.entries.iter().enumerate() {
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
                        continue;
                    }
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
    entry_index: usize,
    entry_total: usize,
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

    let matched = run_plan.entry_is_postprocess(entry.sync_name.as_str(), &normalized_path);

    let result = if !matched {
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
            ManifestEntryKind::RegularFile => commit_regular_file_entry(
                &source_path,
                &final_path,
                server_config.root.as_path(),
                run_id,
            )
            .map(|_| (vec![final_relative], None)),
        }
    } else {
        let work_path = match prepare_work_entry(entry, sync, &source_path, work_area) {
            Ok(p) => p,
            Err(error) => return failed_entry(entry, error),
        };
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
        match apply_postprocessing(
            run_plan,
            entry.sync_name.as_str(),
            &normalized_path,
            &work_path,
            &mut pp_helper,
            entry_index,
            entry_total,
            entry.relative_path.as_str(),
        ) {
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
                let steps: Vec<String> =
                    run_plan.selected_steps_for(entry.sync_name.as_str(), &normalized_path);
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

    let work_area = work_dir(&config.purgery_root, nickname, run_id);
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

    if let Err(error) = run_config.validate_uploaded_purgatory_run() {
        let msg = format!("uploaded run config validation failed: {error}");
        warn!("{}", msg);
        write_run_failure(&config.purgery_root, nickname, run_id, &msg)?;
        anyhow::bail!("{msg}");
    }

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

    let covered_by_dir: std::collections::HashSet<(String, String)> = manifest
        .entries
        .iter()
        .filter(|e| e.kind == ManifestEntryKind::Directory)
        .filter_map(|dir_entry| {
            let _sync = sync_map.get(dir_entry.sync_name.as_str())?;
            let np = dir_entry.relative_path.as_str().to_owned();
            let matched = run_plan
                .rules
                .iter()
                .any(|rule| rule.applies_to(dir_entry.sync_name.as_str()) && rule.is_match(&np));
            if matched {
                Some((dir_entry.sync_name.as_str().to_owned(), np))
            } else {
                None
            }
        })
        .collect();

    let covered_indices: std::collections::HashSet<usize> = manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            let Some(_sync) = sync_map.get(entry.sync_name.as_str()) else {
                return false;
            };
            let np = entry.relative_path.as_str().to_owned();
            let entry_sync = entry.sync_name.as_str();
            covered_by_dir.iter().any(|(sync_name, prefix)| {
                sync_name.as_str() == entry_sync
                    && match np.as_str().strip_prefix(prefix.as_str()) {
                        Some(tail) => tail.starts_with('/'),
                        None => false,
                    }
            })
        })
        .map(|(i, _)| i)
        .collect();

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

    let used_sync_names: std::collections::HashSet<&str> = manifest
        .entries
        .iter()
        .map(|e| e.sync_name.as_str())
        .collect();

    let mut failed_sync_roots: std::collections::HashSet<String> = std::collections::HashSet::new();

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

    let mut outcomes: Vec<EntryOutcome> = Vec::new();

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

        let np = entry.relative_path.as_str().to_owned();
        let entry_sync = entry.sync_name.as_str();
        let covered = covered_by_dir.iter().any(|(sync_name, prefix)| {
            sync_name.as_str() == entry_sync
                && match np.as_str().strip_prefix(prefix.as_str()) {
                    Some(tail) => tail.starts_with('/'),
                    None => false,
                }
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

        outcomes.push(process_manifest_entry(
            config,
            &run_plan,
            sync,
            entry,
            nickname,
            run_id,
            &processing_path,
            &work_area,
            entry_idx,
            manifest.entries.len(),
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

    let processing_runs = crate::phases::find_processing_runs(&config.purgery_root)?;
    let ready_runs = crate::phases::find_ready_runs(&config.purgery_root)?;
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
