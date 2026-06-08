use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use purgery_core::{
    check_symlink_in_path, path_is_within_root, validate_envelope, work_dir, FileStatus,
    FileStatusEntry, Manifest, Nickname, PostprocessKind, PurgeryRoot, RunId, RunPhase, RunState,
    RunStatus, ServerConfig,
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

    let root = &config.root;
    let mut status_entries = Vec::new();
    let mut all_imported = true;

    for file_entry in &manifest.files {
        let sync_name = file_entry.sync_name.as_str();
        let Some(sync) = sync_map.get(sync_name) else {
            eprintln!("sync mapping '{sync_name}' not found in client config, skipping");
            status_entries.push(FileStatusEntry {
                sync_name: file_entry.sync_name.clone(),
                local_path: file_entry.local_path.as_str().to_owned(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                status: FileStatus::Skipped,
                final_path: None,
                postprocess: None,
                error: Some(format!("sync mapping '{sync_name}' not found")),
            });
            all_imported = false;
            continue;
        };

        // Compute final path via validated API
        let final_path = root.final_path(nickname, &sync.to_path, &file_entry.relative_path);

        // Verify path stays within the server root
        if !path_is_within_root(&final_path, root.as_path()) {
            status_entries.push(FileStatusEntry {
                sync_name: file_entry.sync_name.clone(),
                local_path: file_entry.local_path.as_str().to_owned(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                status: FileStatus::Failed,
                final_path: None,
                postprocess: None,
                error: Some(format!("final path escapes root: {}", final_path.as_str())),
            });
            all_imported = false;
            continue;
        }

        // Symlink check before committing
        if let Err(e) = check_symlink_in_path(&final_path, root.as_path()) {
            status_entries.push(FileStatusEntry {
                sync_name: file_entry.sync_name.clone(),
                local_path: file_entry.local_path.as_str().to_owned(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                status: FileStatus::Failed,
                final_path: None,
                postprocess: None,
                error: Some(format!("symlink check failed: {e}")),
            });
            all_imported = false;
            continue;
        }

        let staged_path = file_entry.staged_path.as_str();
        let source_path = processing_path.join(staged_path);

        if !source_path.exists() {
            status_entries.push(FileStatusEntry {
                sync_name: file_entry.sync_name.clone(),
                local_path: file_entry.local_path.as_str().to_owned(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                status: FileStatus::Failed,
                final_path: None,
                postprocess: None,
                error: Some(format!("staged file not found: {}", source_path.as_str())),
            });
            all_imported = false;
            continue;
        }

        // Server-side file identity verification
        let source_utf8 = Utf8PathBuf::from_path_buf(source_path.clone().into_std_path_buf())
            .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref()));
        if let Err(e) = file_entry.verify_staged(&source_utf8) {
            let msg = format!("staged file identity check failed: {e}");
            eprintln!("{msg}");
            status_entries.push(FileStatusEntry {
                sync_name: file_entry.sync_name.clone(),
                local_path: file_entry.local_path.as_str().to_owned(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                status: FileStatus::Failed,
                final_path: None,
                postprocess: None,
                error: Some(msg),
            });
            all_imported = false;
            continue;
        }

        // Copy staged file to work area
        let work_path = work_area.join(file_entry.relative_path.as_str());
        if let Some(parent) = work_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create work subdirectory: {}", parent.as_str())
            })?;
        }
        fs::copy(source_path.as_std_path(), work_path.as_std_path()).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source_path.as_str(),
                work_path.as_str()
            )
        })?;

        // Apply postprocessing
        let normalized_path = format!(
            "{}/{}",
            sync.to_path.as_str(),
            file_entry.relative_path.as_str()
        );
        let postprocess_result =
            apply_postprocessing(config, &client_config, &normalized_path, &work_path);

        match postprocess_result {
            Ok(outputs) => {
                // Create final parent directory
                if let Some(parent) = final_path.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create parent directory: {}", parent.as_str())
                    })?;
                }

                // Commit each output to its final path
                let mut committed_paths = Vec::new();
                for output in &outputs {
                    let output_final = if output == &work_path {
                        final_path.clone()
                    } else {
                        let filename = output.file_name().unwrap_or("");
                        final_path
                            .parent()
                            .map_or_else(|| Utf8PathBuf::from(filename), |p| p.join(filename))
                    };

                    if let Some(parent) = output_final.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("failed to create parent: {}", parent.as_str())
                        })?;
                    }
                    fs::copy(output.as_std_path(), output_final.as_std_path()).with_context(
                        || {
                            format!(
                                "failed to copy {} to {}",
                                output.as_str(),
                                output_final.as_str()
                            )
                        },
                    )?;
                    committed_paths.push(output_final);
                }

                // Determine postprocess step names that were applied
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

                status_entries.push(FileStatusEntry {
                    sync_name: file_entry.sync_name.clone(),
                    local_path: file_entry.local_path.as_str().to_owned(),
                    relative_path: file_entry.relative_path.as_str().to_owned(),
                    status: FileStatus::Imported,
                    final_path: Some(
                        final_path
                            .strip_prefix(root.as_path())
                            .unwrap_or(&final_path)
                            .to_string(),
                    ),
                    postprocess: steps_opt,
                    error: None,
                });
            }
            Err(e) => {
                eprintln!("postprocessing failed for '{}': {e}", normalized_path);
                status_entries.push(FileStatusEntry {
                    sync_name: file_entry.sync_name.clone(),
                    local_path: file_entry.local_path.as_str().to_owned(),
                    relative_path: file_entry.relative_path.as_str().to_owned(),
                    status: FileStatus::Failed,
                    final_path: None,
                    postprocess: None,
                    error: Some(e),
                });
                all_imported = false;
            }
        }
    }

    // Clean up work area
    let _ = fs::remove_dir_all(&work_area);

    let run_state = if all_imported {
        RunState::Done
    } else {
        RunState::Partial
    };

    let run_status = RunStatus {
        run_id: run_id.clone(),
        nickname: nickname.clone(),
        state: run_state,
        files: status_entries,
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

    let done_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Done);
    if let Some(parent) = done_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create done parent: {}", parent.as_str()))?;
    }
    fs::rename(&processing_path, &done_path).with_context(|| {
        format!(
            "failed to move run to done: {} -> {}",
            processing_path.as_str(),
            done_path.as_str()
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

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Partial);
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

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Partial);
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

        let done_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Partial);
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
}
