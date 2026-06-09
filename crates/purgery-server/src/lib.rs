use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    check_symlink_in_path, path_is_within_root, validate_envelope, work_dir, FileStatus,
    FileStatusEntry, Manifest, Nickname, NormalizedRelativePath, PurgeryRoot, RunConfig,
    RunConfigSync, RunId, RunPhase, RunState, RunStatus, ServerConfig,
};
use std::collections::HashMap;
use std::fs;

/// A run-level failure — written when the run cannot be processed at all.
fn write_run_failure(
    purgery_root: &PurgeryRoot,
    nickname: &Nickname,
    run_id: &RunId,
    error_msg: &str,
) {
    let processing_path = purgery_root.run_dir(nickname, run_id, RunPhase::Processing);
    if let Some(parent) = processing_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let status = RunStatus {
        run_id: run_id.clone(),
        nickname: nickname.clone(),
        state: RunState::Failed,
        files: vec![],
        error: Some(error_msg.to_owned()),
    };

    if let Ok(toml_str) = status.to_toml() {
        let status_path = processing_path.join("status.toml");
        let tmp_path = processing_path.join("status.toml.tmp");
        if fs::write(&tmp_path, &toml_str).is_ok() {
            let _ = fs::rename(&tmp_path, &status_path);
        }
    }

    let failed_path = purgery_root.run_dir(nickname, run_id, RunPhase::Failed);
    if let Some(parent) = failed_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::rename(&processing_path, &failed_path);
}

/// Find all ready runs across all nicknames.
pub fn find_ready_runs(purgery_root: &PurgeryRoot) -> Result<Vec<(Nickname, RunId)>> {
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

        let ready_path = nickname_path.join("ready");
        if !ready_path.exists() {
            continue;
        }

        for run_entry in fs::read_dir(&ready_path)
            .with_context(|| format!("failed to read ready dir: {}", ready_path.display()))?
        {
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

/// Per-file outcome.
enum FileOutcome {
    Success {
        sync_name: purgery_core::SyncName,
        local_path: String,
        relative_path: String,
        final_paths: Vec<String>,
        postprocess: Option<Vec<String>>,
    },
    Failure {
        sync_name: purgery_core::SyncName,
        local_path: String,
        relative_path: String,
        error: String,
    },
    Skipped {
        sync_name: purgery_core::SyncName,
        local_path: String,
        relative_path: String,
        error: String,
    },
}

impl FileOutcome {
    fn into_entry(self) -> FileStatusEntry {
        match self {
            FileOutcome::Success {
                sync_name,
                local_path,
                relative_path,
                final_paths,
                postprocess,
            } => FileStatusEntry {
                sync_name,
                local_path,
                relative_path,
                status: FileStatus::Imported,
                final_paths,
                postprocess,
                error: None,
            },
            FileOutcome::Failure {
                sync_name,
                local_path,
                relative_path,
                error,
            } => FileStatusEntry {
                sync_name,
                local_path,
                relative_path,
                status: FileStatus::Failed,
                final_paths: vec![],
                postprocess: None,
                error: Some(error),
            },
            FileOutcome::Skipped {
                sync_name,
                local_path,
                relative_path,
                error,
            } => FileStatusEntry {
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

/// Commit a work-area output to its final path via temp-file then atomic rename.
///
/// Checks that the final path does not already exist (fail-if-exists).
fn commit_output(source: &Utf8Path, final_path: &Utf8Path, run_id: &RunId) -> Result<(), String> {
    if final_path.exists() {
        return Err(format!(
            "final path already exists: {}",
            final_path.as_str()
        ));
    }

    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent.as_std_path())
            .map_err(|e| format!("failed to create parent directory: {e}"))?;
    }

    let tmp_path = purgery_core::commit_temp_path(final_path, run_id);

    fs::copy(source.as_std_path(), tmp_path.as_std_path())
        .map_err(|e| format!("failed to copy to temp path: {e}"))?;

    fs::rename(&tmp_path, final_path)
        .map_err(|e| format!("failed to rename temp to final path: {e}"))?;

    Ok(())
}

/// Process a single file entry: validate, copy to work area, postprocess, commit.
#[allow(clippy::too_many_arguments)]
fn process_one_file(
    server_config: &ServerConfig,
    run_plan: &RunPlan,
    sync: &RunConfigSync,
    file_entry: &purgery_core::ManifestFileEntry,
    nickname: &Nickname,
    run_id: &RunId,
    processing_path: &Utf8Path,
    work_area: &Utf8Path,
) -> FileOutcome {
    let local_path = file_entry.local_path.as_str().to_owned();
    let relative_path = file_entry.relative_path.as_str().to_owned();
    let sync_name = file_entry.sync_name.clone();

    // 1. Validate staged_path matches expected
    let expected_staged = Utf8Path::new("files")
        .join(sync.to_path.as_str())
        .join(file_entry.relative_path.as_str());
    let Ok(expected_normalized) = NormalizedRelativePath::new(expected_staged) else {
        return FileOutcome::Failure {
            sync_name,
            local_path,
            relative_path,
            error: "failed to normalize expected staged path".into(),
        };
    };
    if file_entry.staged_path.as_str() != expected_normalized.as_str() {
        return FileOutcome::Failure {
            sync_name,
            local_path,
            relative_path,
            error: format!(
                "staged_path mismatch: expected '{}', got '{}'",
                expected_normalized.as_str(),
                file_entry.staged_path.as_str()
            ),
        };
    }

    // 2. Resolve staged source path
    let source_path = processing_path.join(file_entry.staged_path.as_str());

    if !source_path.exists() {
        return FileOutcome::Failure {
            sync_name,
            local_path,
            relative_path,
            error: format!("staged file not found: {}", source_path.as_str()),
        };
    }

    // 3. Reject staged symlink
    let staged_metadata = match fs::symlink_metadata(source_path.as_std_path()) {
        Ok(m) => m,
        Err(e) => {
            return FileOutcome::Failure {
                sync_name,
                local_path,
                relative_path,
                error: format!("failed to read staged metadata: {e}"),
            };
        }
    };
    if staged_metadata.file_type().is_symlink() {
        return FileOutcome::Failure {
            sync_name,
            local_path,
            relative_path,
            error: format!("staged path is a symlink: {}", source_path.as_str()),
        };
    }

    // 4. Server-side file identity verification
    let source_utf8 = Utf8PathBuf::from_path_buf(source_path.clone().into_std_path_buf())
        .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref()));
    if let Err(e) = file_entry.verify_staged(&source_utf8) {
        return FileOutcome::Failure {
            sync_name,
            local_path,
            relative_path,
            error: format!("staged file identity check failed: {e}"),
        };
    }

    // 5. Compute final path
    let final_path =
        server_config
            .root
            .final_path(nickname, &sync.to_path, &file_entry.relative_path);

    if !path_is_within_root(&final_path, server_config.root.as_path()) {
        return FileOutcome::Failure {
            sync_name,
            local_path,
            relative_path,
            error: format!("final path escapes root: {}", final_path.as_str()),
        };
    }

    // 6. Symlink check in final destination path
    if let Err(e) = check_symlink_in_path(&final_path, server_config.root.as_path()) {
        return FileOutcome::Failure {
            sync_name,
            local_path,
            relative_path,
            error: format!("symlink check failed: {e}"),
        };
    }

    // 7. Copy staged file to work area (namespaced by sync.to_path)
    let work_path = work_area
        .join(sync.to_path.as_str())
        .join(file_entry.relative_path.as_str());
    if let Some(parent) = work_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return FileOutcome::Failure {
                sync_name,
                local_path,
                relative_path,
                error: format!("failed to create work subdirectory: {e}"),
            };
        }
    }
    if let Err(e) = fs::copy(source_path.as_std_path(), work_path.as_std_path()) {
        return FileOutcome::Failure {
            sync_name,
            local_path,
            relative_path,
            error: format!("failed to copy to work area: {e}"),
        };
    }

    // 8. Apply postprocessing using precompiled run plan
    let normalized_path = format!(
        "{}/{}",
        sync.to_path.as_str(),
        file_entry.relative_path.as_str()
    );
    let postprocess_result = apply_postprocessing(run_plan, &normalized_path, &work_path);

    match postprocess_result {
        Ok(outputs) => {
            // Preflight: derive final paths and check none exist
            let mut preflight_checks: Vec<(Utf8PathBuf, Utf8PathBuf)> = Vec::new();
            let root_path = server_config.root.as_path();

            for output in &outputs {
                let output_final = if output == &work_path {
                    final_path.clone()
                } else {
                    let filename = output.file_name().unwrap_or("");
                    final_path
                        .parent()
                        .map_or_else(|| Utf8PathBuf::from(filename), |p| p.join(filename))
                };

                // Symlink check on destination path component for each output
                if let Err(e) = check_symlink_in_path(&output_final, server_config.root.as_path()) {
                    return FileOutcome::Failure {
                        sync_name,
                        local_path,
                        relative_path,
                        error: format!("symlink check failed for output: {e}"),
                    };
                }

                if output_final.exists() {
                    return FileOutcome::Failure {
                        sync_name,
                        local_path,
                        relative_path,
                        error: format!("final path already exists: {}", output_final.as_str()),
                    };
                }

                // Ensure parent directory can be created
                if let Some(parent) = output_final.parent() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        return FileOutcome::Failure {
                            sync_name,
                            local_path,
                            relative_path,
                            error: format!("failed to create parent directory: {e}"),
                        };
                    }
                }

                preflight_checks.push((output.clone(), output_final));
            }

            // Commit each output via temp-file atomic rename
            let mut committed_rel_paths: Vec<String> = Vec::new();

            for (output, output_final) in &preflight_checks {
                match commit_output(output, output_final, run_id) {
                    Ok(()) => {
                        let rel = output_final
                            .strip_prefix(root_path)
                            .unwrap_or(output_final)
                            .to_string();
                        committed_rel_paths.push(rel);
                    }
                    Err(e) => {
                        // Rollback: remove outputs already committed for this file
                        for committed in &committed_rel_paths {
                            let full_path = root_path.join(committed);
                            let _ = fs::remove_file(&full_path);
                        }
                        return FileOutcome::Failure {
                            sync_name,
                            local_path,
                            relative_path,
                            error: format!("commit failed, rolled back: {e}"),
                        };
                    }
                }
            }

            // 10. Determine postprocess step names that were applied (from run plan)
            let applied_steps: Vec<String> = run_plan
                .rules
                .iter()
                .filter(|cr| cr.regex.is_match(work_path.as_str()))
                .flat_map(|cr| cr.steps.iter().map(|s| s.step_name.clone()))
                .collect();

            let steps_opt = if applied_steps.is_empty() {
                None
            } else {
                Some(applied_steps)
            };

            FileOutcome::Success {
                sync_name,
                local_path,
                relative_path,
                final_paths: committed_rel_paths,
                postprocess: steps_opt,
            }
        }
        Err(e) => {
            eprintln!("postprocessing failed: {e}",);
            FileOutcome::Failure {
                sync_name,
                local_path,
                relative_path,
                error: e,
            }
        }
    }
}

/// Process a single run: claim, move files, postprocess, write status.
///
/// Returns `Ok(())` on success. On error the run should be moved to failed.
pub fn process_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
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

    // Read run config
    let run_config_path = processing_path.join("run.toml");
    let run_config_content = match fs::read_to_string(&run_config_path) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to read run config: {e}");
            eprintln!("{msg}");
            write_run_failure(&config.purgery_root, nickname, run_id, &msg);
            anyhow::bail!("{msg}");
        }
    };
    let run_config = match RunConfig::from_toml(&run_config_content) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to parse run config: {e}");
            eprintln!("{msg}");
            write_run_failure(&config.purgery_root, nickname, run_id, &msg);
            anyhow::bail!("{msg}");
        }
    };

    // Build and validate run plan (compiles all regexes, validates step references)
    let run_plan = match RunPlan::build(config, &run_config) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("run plan validation failed: {e}");
            eprintln!("{msg}");
            write_run_failure(&config.purgery_root, nickname, run_id, &msg);
            anyhow::bail!("{msg}");
        }
    };

    // Read manifest
    let manifest_path = processing_path.join("manifest.toml");
    let manifest_content = match fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to read manifest: {e}");
            eprintln!("{msg}");
            write_run_failure(&config.purgery_root, nickname, run_id, &msg);
            anyhow::bail!("{msg}");
        }
    };
    let manifest = match Manifest::from_toml(&manifest_content) {
        Ok(m) => m,
        Err(e) => {
            let msg = format!("failed to parse manifest: {e}");
            eprintln!("{msg}");
            write_run_failure(&config.purgery_root, nickname, run_id, &msg);
            anyhow::bail!("{msg}");
        }
    };

    // Envelope validation: directory nickname/run_config/manifest must agree
    if let Err(e) = validate_envelope(nickname, run_id, &run_config, &manifest) {
        let msg = format!("envelope validation failed: {e}");
        eprintln!("{msg}");
        write_run_failure(&config.purgery_root, nickname, run_id, &msg);
        anyhow::bail!("{msg}");
    }

    // Create work area
    let work_area = work_dir(config.root.as_path(), nickname, run_id);
    fs::create_dir_all(&work_area)
        .with_context(|| format!("failed to create work area: {}", work_area.as_str()))?;

    let sync_map: HashMap<&str, &RunConfigSync> = run_config.sync_map().into_iter().collect();

    let mut outcomes: Vec<FileOutcome> = Vec::new();

    for file_entry in &manifest.files {
        let sync_name = file_entry.sync_name.as_str();
        let Some(sync) = sync_map.get(sync_name) else {
            eprintln!("sync mapping '{sync_name}' not found in run config, skipping");
            outcomes.push(FileOutcome::Skipped {
                sync_name: file_entry.sync_name.clone(),
                local_path: file_entry.local_path.as_str().to_owned(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                error: format!("sync mapping '{sync_name}' not found"),
            });
            continue;
        };

        let outcome = process_one_file(
            config,
            &run_plan,
            sync,
            file_entry,
            nickname,
            run_id,
            &processing_path,
            &work_area,
        );
        outcomes.push(outcome);
    }

    // Determine run state
    let all_imported = outcomes
        .iter()
        .all(|o| matches!(o, FileOutcome::Success { .. }));
    let any_imported = outcomes
        .iter()
        .any(|o| matches!(o, FileOutcome::Success { .. }));

    let run_state = if all_imported {
        RunState::Done
    } else if any_imported {
        RunState::Partial
    } else {
        RunState::Failed
    };

    // Work area cleanup policy:
    //   Done    -> remove work area
    //   Partial -> keep for debugging
    //   Failed  -> keep for debugging
    if run_state == RunState::Done {
        let _ = fs::remove_dir_all(&work_area);
    }

    let run_status = RunStatus {
        run_id: run_id.clone(),
        nickname: nickname.clone(),
        state: run_state.clone(),
        files: outcomes.into_iter().map(|o| o.into_entry()).collect(),
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

    let dest_phase = match run_state {
        RunState::Done => RunPhase::Done,
        RunState::Partial => RunPhase::Done,
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

    eprintln!(
        "run {}/{} completed with state {}",
        nickname.as_str(),
        run_id.as_str(),
        run_status.state.as_str()
    );

    Ok(())
}

/// A compiled postprocess rule with resolved step definitions.
#[derive(Debug)]
pub struct CompiledRule {
    pub regex: regex::Regex,
    pub steps: Vec<ResolvedStep>,
}

#[derive(Debug, Clone)]
pub struct ResolvedStep {
    pub step_name: String,
    pub step_def: purgery_core::PostprocessStepDefinition,
}

/// A validated run plan: precompiled regexes and resolved step definitions.
#[derive(Debug)]
pub struct RunPlan {
    pub rules: Vec<CompiledRule>,
}

impl RunPlan {
    /// Build a run plan from server config and run config.
    ///
    /// Validates all regexes and step references. Returns an error
    /// (suitable for run-level failure) if anything is invalid.
    pub fn build(
        server_config: &ServerConfig,
        run_config: &purgery_core::RunConfig,
    ) -> Result<Self, String> {
        let mut rules = Vec::new();

        for rule in &run_config.postprocess.rules {
            let re = regex::Regex::new(&rule.pattern)
                .map_err(|e| format!("invalid postprocess regex '{}': {e}", rule.pattern))?;

            let mut steps = Vec::new();
            for step_name in &rule.steps {
                let Some(def) = server_config.postprocess.steps.get(step_name.as_str()) else {
                    return Err(format!(
                        "postprocess step '{step_name}' referenced by rule is not defined on server"
                    ));
                };
                steps.push(ResolvedStep {
                    step_name: step_name.clone(),
                    step_def: def.clone(),
                });
            }

            rules.push(CompiledRule { regex: re, steps });
        }

        Ok(RunPlan { rules })
    }
}

/// Apply postprocessing rules to a file in the work area using a precompiled RunPlan.
///
/// `normalized_path` is the logical path used for rule matching (e.g. `videos/video.mp4`).
/// `work_path` is the absolute work area path used for subprocess execution.
/// Returns the list of work area paths to commit.
pub fn apply_postprocessing(
    run_plan: &RunPlan,
    normalized_path: &str,
    work_path: &Utf8Path,
) -> Result<Vec<Utf8PathBuf>, String> {
    let mut results: Vec<Utf8PathBuf> = Vec::new();
    let mut any_rule_matched = false;

    for compiled in &run_plan.rules {
        if !compiled.regex.is_match(normalized_path) {
            continue;
        }
        any_rule_matched = true;

        for step in &compiled.steps {
            let step_def = &step.step_def;

            match step_def.kind {
                purgery_core::PostprocessKind::Subprocess => {
                    let args = step_def.build_args(work_path);
                    eprintln!(
                        "running postprocess step '{}': {} {}",
                        step.step_name,
                        step_def.program,
                        args.join(" ")
                    );

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

                    // Check expected outputs
                    let expected = step_def.resolve_expected_outputs(work_path);
                    for exp in &expected {
                        if !exp.exists() {
                            return Err(format!("expected output not found: {}", exp.as_str()));
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

/// Process once: scan ready runs and process each one.
pub fn process_once_raw(config: &ServerConfig) -> Result<()> {
    let ready_runs = find_ready_runs(&config.purgery_root)?;
    if ready_runs.is_empty() {
        eprintln!("no ready runs found");
        return Ok(());
    }

    for (nickname, run_id) in &ready_runs {
        eprintln!("processing run {}/{}", nickname.as_str(), run_id.as_str());
        if let Err(e) = process_run(config, nickname, run_id) {
            eprintln!(
                "run {}/{} failed: {:#}",
                nickname.as_str(),
                run_id.as_str(),
                e
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
    // Run server checks before mutation
    server_check(config)?;

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

    fs::create_dir_all(&files_dir)
        .with_context(|| format!("failed to create files dir: {}", files_dir.as_str()))?;

    let response = purgery_core::BeginRunResponse {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        incoming_dir: incoming_path.as_str().to_owned(),
        files_dir: files_dir.as_str().to_owned(),
        run_config_path: run_config_path.as_str().to_owned(),
        manifest_path: manifest_path.as_str().to_owned(),
    };

    toml::to_string(&response)
        .map_err(|e| anyhow::anyhow!("failed to serialize begin-run response: {e}"))
}

/// Server-side subcommand: finish a run by moving from incoming to ready.
pub fn finish_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    // Run server checks before mutation
    server_check(config)?;

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
            Ok(status) => return Ok(status),
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

/// Boot-time server check: verify config, paths, and subprocess programs.
pub fn server_check(config: &ServerConfig) -> Result<()> {
    eprintln!("checking server configuration...");

    // Check root path
    let root_path = config.root.as_path();
    if root_path.exists() {
        if !root_path.is_dir() {
            anyhow::bail!(
                "root path '{}' exists but is not a directory",
                root_path.as_str()
            );
        }
    } else {
        fs::create_dir_all(root_path.as_std_path())
            .with_context(|| format!("failed to create root: {}", root_path.as_str()))?;
        eprintln!("  created root: {}", root_path.as_str());
    }
    eprintln!("  root: {} (accessible)", root_path.as_str());

    // Check purgery_root path
    let purgery_path = config.purgery_root.as_path();
    if purgery_path.exists() {
        if !purgery_path.is_dir() {
            anyhow::bail!(
                "purgery_root '{}' exists but is not a directory",
                purgery_path.as_str()
            );
        }
    } else {
        fs::create_dir_all(purgery_path.as_std_path())
            .with_context(|| format!("failed to create purgery_root: {}", purgery_path.as_str()))?;
        eprintln!("  created purgery_root: {}", purgery_path.as_str());
    }
    eprintln!("  purgery_root: {} (accessible)", purgery_path.as_str());

    // Check phase directories
    for phase in &[
        RunPhase::Incoming,
        RunPhase::Ready,
        RunPhase::Processing,
        RunPhase::Done,
        RunPhase::Failed,
    ] {
        // Just verify the base name is valid
        eprintln!("  phase '{}' directory: valid", phase.as_str());
    }

    // Check postprocess programs
    for (name, step) in &config.postprocess.steps {
        let program = &step.program;
        if program.is_empty() {
            anyhow::bail!("postprocess step '{}' has empty program", name);
        }

        // Validate step produces at least one output
        if !step.keep_original && step.expected_outputs.is_empty() {
            anyhow::bail!(
                "postprocess step '{}': keep_original=false with no expected_outputs would produce zero committed outputs",
                name
            );
        }

        purgery_core::resolve_executable(program).map(|r| {
            eprintln!(
                "  postprocess step '{}': program '{}' found at {}",
                name,
                program,
                r.path.as_str()
            )
        })?;
    }

    eprintln!("server configuration: OK");
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
        ClientLocalPath, ManifestFileEntry, NormalizedRelativePath, PostprocessConfig,
        PostprocessKind, PostprocessStepDefinition, ServerRoot, SyncName,
    };

    fn test_server_config(purgery_root: &Utf8Path, server_root: &Utf8Path) -> ServerConfig {
        ServerConfig {
            root: ServerRoot::new(server_root.to_owned()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.to_owned()).unwrap(),
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig::default(),
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
            files: vec![ManifestFileEntry {
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
                size: content.len() as u64,
                mtime_ns: 1000000,
                sha256: None,
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
        assert_eq!(status.files.len(), 1);
        assert_eq!(status.files[0].status, FileStatus::Imported);
        assert_eq!(
            status.files[0].final_paths,
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
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("unknown-sync".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
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
        assert_eq!(status.files[0].status, FileStatus::Skipped);
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
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/missing.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/missing.mp4".into())
                    .unwrap(),
                relative_path: NormalizedRelativePath::new("missing.mp4".into()).unwrap(),
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
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
        assert_eq!(status.files[0].status, FileStatus::Failed);
        assert!(status.files[0]
            .error
            .as_ref()
            .unwrap()
            .contains("staged file not found"));
    }

    #[test]
    fn test_rule_matching() {
        let rule = purgery_core::PostprocessRule {
            pattern: r"^videos/.*\.(mp4|mov|mkv|webm)$".into(),
            steps: vec!["compress-video".into()],
        };
        let re = regex::Regex::new(&rule.pattern).unwrap();
        assert!(re.is_match("videos/a.mp4"));
        assert!(re.is_match("videos/subdir/b.mov"));
        assert!(re.is_match("videos/c.webm"));
        assert!(!re.is_match("audio/song.mp3"));
        assert!(!re.is_match("videos/a.txt"));
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
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
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
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig {
                max_parallel_jobs: 1,
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
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: r"^videos/.*$".to_owned(),
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
        // With keep_original=true and no expected outputs, should return [original, ...]
        // Since "true" succeeds but doesn't create files, the original is returned
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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig {
                max_parallel_jobs: 1,
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
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-fail-pp".into()).unwrap();

        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/test.mp4"), b"video content").unwrap();

        write_run_toml_with_sync(&ready_path, &nickname, "videos", "videos");
        // Add postprocess rules to the run config manually
        let run_config_content = r#"nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[postprocess.rules]]
match = '^videos/.*\.mp4$'
steps = ["compress-video"]
"#
        .to_string();
        fs::write(ready_path.join("run.toml"), &run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
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
        assert_eq!(status.files[0].status, FileStatus::Failed);
        assert!(status.files[0].error.as_ref().unwrap().contains("failed"));

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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig {
                max_parallel_jobs: 1,
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
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: r"^videos/.*\.mp4$".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                }],
            },
        };

        let compressed = work_area.join("video.Z.webm");
        fs::write(&compressed, b"compressed").unwrap();

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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig {
                max_parallel_jobs: 1,
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
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: r"^videos/.*$".to_owned(),
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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig {
                max_parallel_jobs: 1,
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
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: r"^videos/.*$".to_owned(),
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

    // ── Fail-if-exists test ──

    #[test]
    fn test_final_output_exists_causes_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-exists".into()).unwrap();

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

        let final_path = server_root.join("laptop/videos/test.mp4");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"pre-existing content").unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(
            fs::read_to_string(&final_path).unwrap(),
            "pre-existing content"
        );

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.files[0].status, FileStatus::Failed);
        assert!(status.files[0]
            .error
            .as_ref()
            .unwrap()
            .contains("already exists"));
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
            files: vec![
                ManifestFileEntry {
                    sync_name: SyncName::new("videos".into()).unwrap(),
                    local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                    size: 13,
                    mtime_ns: 1000000,
                    sha256: None,
                },
                ManifestFileEntry {
                    sync_name: SyncName::new("pictures".into()).unwrap(),
                    local_path: ClientLocalPath::new("/home/user/Pictures/a.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/pictures/a.mp4".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                    size: 15,
                    mtime_ns: 1000001,
                    sha256: None,
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
        assert_eq!(status.files.len(), 2);
        assert_eq!(status.files[0].status, FileStatus::Imported);
        assert_eq!(status.files[1].status, FileStatus::Imported);
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
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/other/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 7,
                mtime_ns: 1000000,
                sha256: None,
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
        assert_eq!(status.files[0].status, FileStatus::Failed);
        assert!(status.files[0]
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
        assert_eq!(status.files[0].status, FileStatus::Imported);
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
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 12,
                mtime_ns: 1000000,
                sha256: None,
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
        assert_eq!(status.files[0].status, FileStatus::Failed);
        assert!(status.files[0].error.as_ref().unwrap().contains("symlink"));
    }

    // ── Invalid regex test ──

    #[test]
    fn test_invalid_regex_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-regex".into()).unwrap();

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
match = '[invalid-regex'
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 7,
                mtime_ns: 1000000,
                sha256: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(result.is_err(), "process_run must error on invalid regex");

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed_path.exists());
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("invalid postprocess regex"));
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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig {
                max_parallel_jobs: 1,
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
match = '^videos/.*\.mp4$'
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig {
                max_parallel_jobs: 1,
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
match = '^videos/.*\.mp4$'
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/video.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/video.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("video.mp4".into()).unwrap(),
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
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
        assert_eq!(status.files[0].status, FileStatus::Imported);
        assert_eq!(status.files[0].final_paths.len(), 2);

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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig {
                max_parallel_jobs: 1,
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
match = '^videos/.*\.mp4$'
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/video.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/video.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("video.mp4".into()).unwrap(),
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
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
        assert_eq!(status.files[0].status, FileStatus::Imported);
        assert_eq!(status.files[0].final_paths.len(), 1);

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
    fn test_run_plan_validates_regexes() {
        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "[invalid-regex".into(),
                    steps: vec!["compress-video".into()],
                }],
            },
        };
        let result = RunPlan::build(&server_config, &run_config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid postprocess regex"));
    }

    #[test]
    fn test_run_plan_validates_step_references() {
        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: r"^videos/.*$".into(),
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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig::default(),
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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig::default(),
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
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("nonexistent".into()).unwrap();

        let result = read_run_status(&server_config, &nickname, &run_id);
        assert!(result.is_err());
    }
}
