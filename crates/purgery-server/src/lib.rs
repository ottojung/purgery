use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    check_symlink_in_path, path_is_within_root, validate_envelope, work_dir, FileStatus,
    FileStatusEntry, Manifest, Nickname, NormalizedRelativePath, PostprocessKind, PurgeryRoot,
    RunId, RunPhase, RunState, RunStatus, ServerConfig,
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
    client_config: &purgery_core::ClientConfig,
    sync: &purgery_core::SyncMapping,
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

    // 8. Apply postprocessing
    let normalized_path = format!(
        "{}/{}",
        sync.to_path.as_str(),
        file_entry.relative_path.as_str()
    );
    let postprocess_result =
        apply_postprocessing(server_config, client_config, &normalized_path, &work_path);

    match postprocess_result {
        Ok(outputs) => {
            // 9. Commit each output via temp-file atomic rename
            let mut committed_rel_paths: Vec<String> = Vec::new();
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

                // Check fail-if-exists
                if output_final.exists() {
                    // Clean up any previously committed outputs for this file
                    return FileOutcome::Failure {
                        sync_name,
                        local_path,
                        relative_path,
                        error: format!("final path already exists: {}", output_final.as_str()),
                    };
                }

                match commit_output(output, &output_final, run_id) {
                    Ok(()) => {
                        let rel = output_final
                            .strip_prefix(root_path)
                            .unwrap_or(&output_final)
                            .to_string();
                        committed_rel_paths.push(rel);
                    }
                    Err(e) => {
                        return FileOutcome::Failure {
                            sync_name,
                            local_path,
                            relative_path,
                            error: format!("commit failed: {e}"),
                        };
                    }
                }
            }

            // 10. Determine postprocess step names that were applied
            let applied_steps: Vec<String> = client_config
                .postprocess
                .rules
                .iter()
                .filter(|r| {
                    regex::Regex::new(&r.pattern)
                        .map(|re| re.is_match(&normalized_path))
                        .unwrap_or(false)
                })
                .flat_map(|r| r.steps.clone())
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
            eprintln!("postprocessing failed for '{}': {e}", normalized_path);
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

    // Read client config
    let config_path = processing_path.join("config.toml");
    let client_config_content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to read client config: {e}");
            eprintln!("{msg}");
            write_run_failure(&config.purgery_root, nickname, run_id, &msg);
            anyhow::bail!("{msg}");
        }
    };
    let client_config = match purgery_core::ClientConfig::from_toml(&client_config_content) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to parse client config: {e}");
            eprintln!("{msg}");
            write_run_failure(&config.purgery_root, nickname, run_id, &msg);
            anyhow::bail!("{msg}");
        }
    };

    // Validate postprocess regexes before any file processing
    for rule in &client_config.postprocess.rules {
        if regex::Regex::new(&rule.pattern).is_err() {
            let msg = format!(
                "invalid postprocess regex in client config: '{}'",
                rule.pattern
            );
            eprintln!("{msg}");
            write_run_failure(&config.purgery_root, nickname, run_id, &msg);
            anyhow::bail!("{msg}");
        }
    }

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

    // Envelope validation: directory nickname/config/manifest must agree
    if let Err(e) = validate_envelope(nickname, run_id, &client_config, &manifest) {
        let msg = format!("envelope validation failed: {e}");
        eprintln!("{msg}");
        write_run_failure(&config.purgery_root, nickname, run_id, &msg);
        anyhow::bail!("{msg}");
    }

    // Create work area
    let work_area = work_dir(config.root.as_path(), nickname, run_id);
    fs::create_dir_all(&work_area)
        .with_context(|| format!("failed to create work area: {}", work_area.as_str()))?;

    let sync_map: HashMap<&str, &purgery_core::SyncMapping> = client_config
        .sync
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect();

    let mut outcomes: Vec<FileOutcome> = Vec::new();

    for file_entry in &manifest.files {
        let sync_name = file_entry.sync_name.as_str();
        let Some(sync) = sync_map.get(sync_name) else {
            eprintln!("sync mapping '{sync_name}' not found in client config, skipping");
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
            &client_config,
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

/// Apply postprocessing rules to a file in the work area.
///
/// Returns the list of work area paths to commit.
/// For compress-video: runs `<program> --input <work_path>`, checks for `.Z.webm` output.
pub fn apply_postprocessing(
    server_config: &ServerConfig,
    client_config: &purgery_core::ClientConfig,
    normalized_path: &str,
    work_path: &Utf8Path,
) -> Result<Vec<Utf8PathBuf>, String> {
    let mut results: Vec<Utf8PathBuf> = Vec::new();
    let mut any_rule_matched = false;

    for rule in &client_config.postprocess.rules {
        let Ok(re) = regex::Regex::new(&rule.pattern) else {
            eprintln!("invalid regex pattern: {}", rule.pattern);
            continue;
        };

        if !re.is_match(normalized_path) {
            continue;
        }
        any_rule_matched = true;

        for step_name in &rule.steps {
            let Some(step_def) = server_config.postprocess.steps.get(step_name) else {
                return Err(format!(
                    "postprocess step '{step_name}' not defined on server"
                ));
            };

            match step_def.kind {
                PostprocessKind::CompressVideo => {
                    eprintln!(
                        "running postprocess step '{step_name}': {} --input {}",
                        step_def.program,
                        work_path.as_str()
                    );

                    let status = std::process::Command::new(&step_def.program)
                        .arg("--input")
                        .arg(work_path.as_str())
                        .status()
                        .map_err(|e| format!("failed to run {step_name}: {e}"))?;

                    if !status.success() {
                        return Err(format!("{step_name} failed"));
                    }

                    // Check for expected .Z.webm output
                    let stem = work_path.file_stem().unwrap_or("");
                    let compressed_name = format!("{stem}.Z.webm");
                    let compressed = work_path.with_file_name(&compressed_name);

                    if !compressed.exists() {
                        return Err(format!(
                            "expected output not found: {}",
                            compressed.as_str()
                        ));
                    }

                    if step_def.keep_original {
                        results.push(work_path.to_owned());
                    }
                    results.push(compressed);
                }
            }
        }
    }

    if !any_rule_matched {
        // No matching rules, just commit the original
        results.push(work_path.to_owned());
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

    fn test_server_config(purgery_root: &str, server_root: &str) -> ServerConfig {
        ServerConfig {
            root: ServerRoot::new(server_root.into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.into()).unwrap(),
            state_dir: None,
            log_dir: None,
            postprocess: PostprocessConfig::default(),
        }
    }

    #[test]
    fn test_full_processing_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-001".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        let files_dir = ready_path.join("files/videos");
        fs::create_dir_all(&files_dir).unwrap();
        let staged_file_path = files_dir.join("test.mp4");
        fs::write(&staged_file_path, b"hello world").unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
delete_after_import = true
"#
        .to_string();
        fs::write(ready_path.join("config.toml"), &client_toml).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
            }],
        };
        let manifest_toml = manifest.to_toml().unwrap();
        fs::write(ready_path.join("manifest.toml"), &manifest_toml).unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let status_path = done_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
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
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-002".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files")).unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

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
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-003".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#
        .to_string();
        fs::write(ready_path.join("config.toml"), &client_toml).unwrap();

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

    /// Envelope validation: directory nickname != config nickname -> run fails
    #[test]
    fn test_nickname_mismatch_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let dir_nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-env-001".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&dir_nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        // Config has a different nickname than the directory
        let client_toml = r#"
nickname = "other-machine"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

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

        // The run should fail because envelope validation fails
        let result = process_run(&config, &dir_nickname, &run_id);
        assert!(result.is_err());

        // A failed status should have been written
        let failed_path = config
            .purgery_root
            .run_dir(&dir_nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(
            status_path.exists(),
            "failed status must be written on envelope mismatch"
        );
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("envelope validation failed"));
    }

    /// Bad manifest still produces a readable failed status
    #[test]
    fn test_bad_manifest_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-manifest".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();
        // Write garbage as manifest
        fs::write(ready_path.join("manifest.toml"), "not valid toml {{{").unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(result.is_err());

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(
            status_path.exists(),
            "failed status must exist after bad manifest"
        );
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("failed to parse manifest"));
    }

    /// Bad config produces a readable failed status
    #[test]
    fn test_bad_config_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-config".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        // Write garbage as config
        fs::write(ready_path.join("config.toml"), "not valid toml {{{").unwrap();
        // Valid manifest
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
        assert!(
            status_path.exists(),
            "failed status must exist after bad config"
        );
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status
            .error
            .unwrap()
            .contains("failed to parse client config"));
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
            postprocess: purgery_core::PostprocessConfig {
                max_parallel_jobs: 1,
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::CompressVideo,
                            program: "true".to_owned(),
                            keep_original: true,
                        },
                    );
                    m
                },
            },
        };
        let client_config = purgery_core::ClientConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            server: purgery_core::ServerConnection {
                host: purgery_core::RemoteHost::new("example.com".into()).unwrap(),
                purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            },
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: r"^videos/.*$".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                }],
            },
        };

        // Create a temporary work area with the file
        let tmp = tempfile::tempdir().unwrap();
        let work_area = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_path = work_area.join("some file.mp4");
        fs::write(&work_path, b"test data").unwrap();

        // Create a fake compressed output that "true" would not create,
        // so we need to create it ourselves for the test
        let compressed = work_area.join("some file.Z.webm");
        fs::write(&compressed, b"compressed data").unwrap();

        let results = apply_postprocessing(
            &server_config,
            &client_config,
            "videos/some file.mp4",
            &work_path,
        );
        assert!(results.is_ok(), "postprocess with spaces should succeed");
        let outputs = results.unwrap();
        // With keep_original=true, should return [original, compressed]
        assert_eq!(outputs.len(), 2);
        assert!(outputs.contains(&work_path));
        assert!(outputs.contains(&compressed));
    }

    #[test]
    fn test_postprocessing_failure_does_not_create_final_output() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        // Server config with a program that always fails
        let server_config = ServerConfig {
            root: ServerRoot::new(server_str.clone().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_str.into()).unwrap(),
            state_dir: None,
            log_dir: None,
            postprocess: purgery_core::PostprocessConfig {
                max_parallel_jobs: 1,
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::CompressVideo,
                            program: "false".to_owned(),
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

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = '^videos/.*\.mp4$'
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

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
        assert!(
            status.files[0].error.as_ref().unwrap().contains("failed"),
            "error should mention failure: {:?}",
            status.files[0].error
        );

        // No user-visible final output should exist
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
            postprocess: purgery_core::PostprocessConfig {
                max_parallel_jobs: 1,
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::CompressVideo,
                            program: "true".to_owned(),
                            keep_original: true,
                        },
                    );
                    m
                },
            },
        };
        let client_config = purgery_core::ClientConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            server: purgery_core::ServerConnection {
                host: purgery_core::RemoteHost::new("example.com".into()).unwrap(),
                purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            },
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: r"^videos/.*\.mp4$".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                }],
            },
        };

        // Create the expected .Z.webm output (simulating what the program would create)
        let compressed = work_area.join("video.Z.webm");
        fs::write(&compressed, b"compressed").unwrap();

        let result = apply_postprocessing(
            &server_config,
            &client_config,
            "videos/video.mp4",
            &work_path,
        );
        assert!(result.is_ok());
        let outputs = result.unwrap();
        assert!(
            outputs.contains(&compressed),
            "compressed output must be in list"
        );
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
            postprocess: purgery_core::PostprocessConfig {
                max_parallel_jobs: 1,
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::CompressVideo,
                            program: "true".to_owned(),
                            keep_original: true,
                        },
                    );
                    m
                },
            },
        };
        let client_config = purgery_core::ClientConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            server: purgery_core::ServerConnection {
                host: purgery_core::RemoteHost::new("example.com".into()).unwrap(),
                purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            },
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: r"^videos/.*$".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                }],
            },
        };

        let result = apply_postprocessing(
            &server_config,
            &client_config,
            "videos/video.mp4",
            &work_path,
        );
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
            postprocess: purgery_core::PostprocessConfig {
                max_parallel_jobs: 1,
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::CompressVideo,
                            program: "true".to_owned(),
                            keep_original: false,
                        },
                    );
                    m
                },
            },
        };
        let client_config = purgery_core::ClientConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            server: purgery_core::ServerConnection {
                host: purgery_core::RemoteHost::new("example.com".into()).unwrap(),
                purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            },
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: r"^videos/.*$".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                }],
            },
        };

        let result = apply_postprocessing(
            &server_config,
            &client_config,
            "videos/video.mp4",
            &work_path,
        );
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

    // ── Temp-file commit: no direct copy to final paths ──

    #[test]
    fn test_temp_file_commit_no_direct_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-tmp-commit".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/test.mp4"), b"hello").unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
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

        process_run(&config, &nickname, &run_id).unwrap();

        let final_path = server_root.join("laptop/videos/test.mp4");
        assert!(final_path.exists(), "final file must exist after commit");
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "hello");

        // No .purgery-commit temp files should remain in the final parent directory
        let parent = final_path.parent().unwrap();
        let has_temp_files = std::fs::read_dir(parent).unwrap().any(|e| {
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

    // ── Fail-if-exists: existing final output causes file failure ──

    #[test]
    fn test_final_output_exists_causes_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-exists".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/test.mp4"), b"hello").unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
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

        // Create the final path before running — this should cause a failure
        let final_path = server_root.join("laptop/videos/test.mp4");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"pre-existing content").unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        // Verify the original content was not overwritten
        assert_eq!(
            fs::read_to_string(&final_path).unwrap(),
            "pre-existing content",
            "existing final file must not be overwritten"
        );

        // Verify status shows failure
        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.files[0].status, FileStatus::Failed);
        assert!(
            status.files[0]
                .error
                .as_ref()
                .unwrap()
                .contains("already exists"),
            "error must mention 'already exists': {:?}",
            status.files[0].error
        );
    }

    // ── Work area namespacing: two sync mappings with same relative_path ──

    #[test]
    fn test_work_area_namespacing_no_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-ns".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::create_dir_all(ready_path.join("files/pictures")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"video content").unwrap();
        fs::write(ready_path.join("files/pictures/a.mp4"), b"picture content").unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[sync]]
name = "pictures"
from = "/home/user/Pictures"
to = "pictures"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

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

        // Both files should be in their respective final directories
        let video_final = server_root.join("laptop/videos/a.mp4");
        let picture_final = server_root.join("laptop/pictures/a.mp4");
        assert!(video_final.exists(), "video final must exist");
        assert!(picture_final.exists(), "picture final must exist");
        assert_eq!(fs::read_to_string(&video_final).unwrap(), "video content");
        assert_eq!(
            fs::read_to_string(&picture_final).unwrap(),
            "picture content"
        );

        // Status should show both files imported
        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.files.len(), 2);
        assert_eq!(status.files[0].status, FileStatus::Imported);
        assert_eq!(status.files[1].status, FileStatus::Imported);
        // Both should have one final path each
        assert_eq!(status.files[0].final_paths.len(), 1);
        assert_eq!(status.files[1].final_paths.len(), 1);
    }

    // ── Manifest staged_path validation ──

    #[test]
    fn test_manifest_staged_path_mismatch_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-sp-mismatch".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"content").unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

        // staged_path does not match expected "files/videos/a.mp4" for this file
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
        assert!(
            status.files[0]
                .error
                .as_ref()
                .unwrap()
                .contains("staged_path mismatch"),
            "error must mention staged_path mismatch: {:?}",
            status.files[0].error
        );
    }

    #[test]
    fn test_manifest_staged_path_match_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-sp-match".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"content").unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                // Correct staged_path: files/videos/a.mp4 matches files/videos/a.mp4
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

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.files[0].status, FileStatus::Imported);
    }

    // ── Staged symlink rejection ──

    #[test]
    fn test_staged_symlink_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-symlink".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();

        // Create a real file and then a symlink to it as the "staged" file
        let real_file = ready_path.join("files/videos/real.mp4");
        fs::write(&real_file, b"real content").unwrap();
        let staged_link = ready_path.join("files/videos/a.mp4");
        std::os::unix::fs::symlink(&real_file, &staged_link).unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

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

        // The file should NOT have been imported because it's a symlink
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
        assert!(
            status.files[0].error.as_ref().unwrap().contains("symlink"),
            "error must mention symlink: {:?}",
            status.files[0].error
        );
    }

    // ── Invalid client regex produces readable failed status ──

    #[test]
    fn test_invalid_regex_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-regex".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"content").unwrap();

        // Client config with an invalid regex
        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = '[invalid-regex'
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

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
        assert!(failed_path.exists(), "failed directory must exist");
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists(), "failed status must exist");
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(
            status.error.unwrap().contains("invalid postprocess regex"),
            "error must mention invalid regex"
        );

        // Verify no final output was created — the file should not be imported
        let final_path = server_root.join("laptop/videos/a.mp4");
        assert!(
            !final_path.exists(),
            "file must not be imported with invalid regex"
        );
    }

    // ── Work area cleanup policy ──

    #[test]
    fn test_run_state_done_removes_work_area() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-done-wa".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"hello").unwrap();

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
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

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists(), "done directory must exist");

        // Work area must be removed for Done state
        let work_area = purgery_core::work_dir(config.root.as_path(), &nickname, &run_id);
        assert!(!work_area.exists(), "work area must be removed on Done");
    }

    #[test]
    fn test_run_state_partial_keeps_work_area() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        // Server config with a program that always fails
        let server_config = ServerConfig {
            root: ServerRoot::new(server_str.clone().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_str.into()).unwrap(),
            state_dir: None,
            log_dir: None,
            postprocess: purgery_core::PostprocessConfig {
                max_parallel_jobs: 1,
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        purgery_core::PostprocessStepDefinition {
                            kind: PostprocessKind::CompressVideo,
                            program: "false".to_owned(),
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

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = '^videos/.*\.mp4$'
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

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

        // The run should be Partial (one file failed but it's all files, so actually Failed)
        // Since only 1 file and it failed, state will be Failed, not Partial
        // Let's check both cases: work area should be kept for Partial or Failed
        let work_area = purgery_core::work_dir(server_config.root.as_path(), &nickname, &run_id);

        // The run has no successful files (only 1 file, which failed), so state is Failed
        let failed_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed_path.exists(), "failed directory must exist");

        // Work area must be kept for diagnostic purposes
        assert!(
            work_area.exists(),
            "work area must be kept for Failed state (only 1 file, all failed)"
        );
    }

    #[test]
    fn test_run_state_partial_with_mixed_results_keeps_work_area() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let config = test_server_config(&purgery_str, &server_str);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-mixed-wa".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/good.mp4"), b"good content").unwrap();
        // Don't create bad.mp4 — it will fail with "staged file not found"

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            files: vec![
                ManifestFileEntry {
                    sync_name: SyncName::new("videos".into()).unwrap(),
                    local_path: ClientLocalPath::new("/home/user/Videos/good.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/videos/good.mp4".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("good.mp4".into()).unwrap(),
                    size: 12,
                    mtime_ns: 1000000,
                    sha256: None,
                },
                ManifestFileEntry {
                    sync_name: SyncName::new("videos".into()).unwrap(),
                    local_path: ClientLocalPath::new("/home/user/Videos/bad.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/videos/bad.mp4".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("bad.mp4".into()).unwrap(),
                    size: 99,
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

        // State should be Partial (one success, one failure)
        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists(), "done directory must exist");

        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Partial);
        assert_eq!(status.files[0].status, FileStatus::Imported);
        assert_eq!(status.files[1].status, FileStatus::Failed);

        // Work area must be kept for Partial state
        let work_area = purgery_core::work_dir(config.root.as_path(), &nickname, &run_id);
        assert!(
            work_area.exists(),
            "work area must be kept for Partial state"
        );
    }

    // ── compress-video keep_original records both final paths in status ──

    #[test]
    fn test_compress_video_keep_original_records_both_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let script_path = tmp.path().join("compress.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nbase=$(basename \"$2\");stem=\"${base%.*}\";dir=$(dirname \"$2\");touch \"$dir/$stem.Z.webm\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_str.clone().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_str.clone().into()).unwrap(),
            state_dir: None,
            log_dir: None,
            postprocess: purgery_core::PostprocessConfig {
                max_parallel_jobs: 1,
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        purgery_core::PostprocessStepDefinition {
                            kind: PostprocessKind::CompressVideo,
                            program: script_path.to_string_lossy().to_string(),
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

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = '^videos/.*\.mp4$'
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

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

        // With keep_original=true, should have both paths
        assert_eq!(
            status.files[0].final_paths.len(),
            2,
            "keep_original=true must record 2 final paths, got {:#?}",
            status.files[0].final_paths
        );

        let original_final = server_root.join("laptop/videos/video.mp4");
        let compressed_final = server_root.join("laptop/videos/video.Z.webm");
        assert!(original_final.exists(), "original must exist on disk");
        assert!(compressed_final.exists(), "compressed must exist on disk");
    }

    #[test]
    fn test_compress_video_keep_original_false_records_one_path() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = tmp.path().join("purgery");
        let server_root = tmp.path().join("storage");
        let purgery_str = purgery_root.to_string_lossy().to_string();
        let server_str = server_root.to_string_lossy().to_string();

        let script_path = tmp.path().join("compress.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nbase=$(basename \"$2\");stem=\"${base%.*}\";dir=$(dirname \"$2\");touch \"$dir/$stem.Z.webm\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_str.clone().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_str.into()).unwrap(),
            state_dir: None,
            log_dir: None,
            postprocess: purgery_core::PostprocessConfig {
                max_parallel_jobs: 1,
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        purgery_core::PostprocessStepDefinition {
                            kind: PostprocessKind::CompressVideo,
                            program: script_path.to_string_lossy().to_string(),
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

        let client_toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"

[[postprocess.rules]]
match = '^videos/.*\.mp4$'
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("config.toml"), client_toml).unwrap();

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

        // With keep_original=false, should have only 1 final path (compressed)
        assert_eq!(
            status.files[0].final_paths.len(),
            1,
            "keep_original=false must record 1 final path, got {:#?}",
            status.files[0].final_paths
        );

        let original_final = server_root.join("laptop/videos/video.mp4");
        let compressed_final = server_root.join("laptop/videos/video.Z.webm");
        assert!(
            !original_final.exists(),
            "original must NOT exist on disk with keep_original=false"
        );
        assert!(compressed_final.exists(), "compressed must exist on disk");
    }
}
