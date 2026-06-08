use anyhow::{Context, Result};
use purgery_core::{
    path_is_within_root, shell_escape, validate_envelope, FileStatus, FileStatusEntry, Manifest,
    Nickname, PurgeryRoot, RunId, RunPhase, RunState, RunStatus, ServerConfig,
};
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

    let sync_map: std::collections::HashMap<&str, &purgery_core::SyncMapping> = client_config
        .sync
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect();

    let root = &config.root;
    let mut status_entries = Vec::new();
    let mut all_imported = true;

    for file_entry in &manifest.files {
        let sync_name = &file_entry.sync_name;
        let Some(sync) = sync_map.get(sync_name.as_str()) else {
            eprintln!(
                "sync mapping '{}' not found in client config, skipping",
                sync_name
            );
            status_entries.push(FileStatusEntry {
                sync_name: sync_name.clone(),
                local_path: file_entry.local_path.clone(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                status: FileStatus::Skipped,
                final_path: None,
                postprocess: None,
                error: Some(format!("sync mapping '{}' not found", sync_name)),
            });
            all_imported = false;
            continue;
        };

        // Compute final path via validated API
        let final_path = root.final_path(nickname, &sync.to_path, &file_entry.relative_path);

        // Verify path stays within the server root
        if !path_is_within_root(&final_path, root.as_path()) {
            status_entries.push(FileStatusEntry {
                sync_name: sync_name.clone(),
                local_path: file_entry.local_path.clone(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                status: FileStatus::Failed,
                final_path: None,
                postprocess: None,
                error: Some(format!("final path escapes root: {}", final_path.as_str())),
            });
            all_imported = false;
            continue;
        }

        let staged_path = file_entry.staged_path.as_str();
        let source_path = processing_path.join(staged_path);

        if !source_path.exists() {
            status_entries.push(FileStatusEntry {
                sync_name: sync_name.clone(),
                local_path: file_entry.local_path.clone(),
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
        let source_utf8 =
            camino::Utf8PathBuf::from_path_buf(source_path.clone().into_std_path_buf())
                .unwrap_or_else(|p| camino::Utf8PathBuf::from(p.to_string_lossy().as_ref()));
        if let Err(e) = file_entry.verify_staged(&source_utf8) {
            let msg = format!("staged file identity check failed: {e}");
            eprintln!("{msg}");
            status_entries.push(FileStatusEntry {
                sync_name: sync_name.clone(),
                local_path: file_entry.local_path.clone(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                status: FileStatus::Failed,
                final_path: None,
                postprocess: None,
                error: Some(msg),
            });
            all_imported = false;
            continue;
        }

        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create parent directory: {}", parent.as_str())
            })?;
        }

        // Build temporary import path
        let rel_part = file_entry.relative_path.as_str().replace('/', "_");
        let tmp_name = format!(".purgery-importing.{rel_part}.tmp");
        let tmp_path = final_path
            .parent()
            .map(|p| p.join(&tmp_name))
            .unwrap_or_else(|| final_path.with_file_name(&tmp_name));

        // Move (or copy+delete) to temporary path
        if let Err(e) = fs::rename(&source_path, &tmp_path) {
            eprintln!("rename failed (may be cross-filesystem): {e}, falling back to copy");
            fs::copy(&source_path, &tmp_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.as_str(),
                    tmp_path.as_str()
                )
            })?;
            let _ = fs::remove_file(&source_path);
        }

        // Atomic rename into place
        fs::rename(&tmp_path, final_path.as_std_path()).with_context(|| {
            format!(
                "failed to rename {} to {}",
                tmp_path.as_str(),
                final_path.as_str()
            )
        })?;

        // Apply postprocessing
        let normalized_path = format!(
            "{}/{}",
            sync.to_path.as_str(),
            file_entry.relative_path.as_str()
        );
        let applied_steps =
            apply_postprocessing(config, &client_config, &normalized_path, &final_path);

        let has_failure = applied_steps.iter().any(|(_, success)| !success);
        let was_imported = !has_failure;

        if was_imported {
            let steps: Vec<String> = applied_steps.iter().map(|(name, _)| name.clone()).collect();
            let steps_opt = if steps.is_empty() { None } else { Some(steps) };
            status_entries.push(FileStatusEntry {
                sync_name: sync_name.clone(),
                local_path: file_entry.local_path.clone(),
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
        } else {
            let error_msg = applied_steps
                .iter()
                .filter(|(_, success)| !success)
                .map(|(name, _)| format!("{name} failed"))
                .collect::<Vec<_>>()
                .join("; ");
            status_entries.push(FileStatusEntry {
                sync_name: sync_name.clone(),
                local_path: file_entry.local_path.clone(),
                relative_path: file_entry.relative_path.as_str().to_owned(),
                status: FileStatus::Failed,
                final_path: None,
                postprocess: None,
                error: Some(error_msg),
            });
            all_imported = false;
        }
    }

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

/// Apply postprocessing rules to a file.
///
/// Returns a list of (step_name, success) pairs.
/// Uses `{path}` as the template placeholder in args.
pub fn apply_postprocessing(
    server_config: &ServerConfig,
    client_config: &purgery_core::ClientConfig,
    normalized_path: &str,
    final_path: impl AsRef<std::path::Path>,
) -> Vec<(String, bool)> {
    let final_path = final_path.as_ref();
    let final_path_str = final_path.to_string_lossy();
    let mut results = Vec::new();

    for rule in &client_config.postprocess.rules {
        let Ok(re) = regex::Regex::new(&rule.pattern) else {
            eprintln!("invalid regex pattern: {}", rule.pattern);
            continue;
        };

        if !re.is_match(normalized_path) {
            continue;
        }

        for step_name in &rule.steps {
            let Some(step_def) = server_config.postprocess.steps.get(step_name) else {
                eprintln!("postprocess step '{step_name}' not defined on server");
                results.push((step_name.clone(), false));
                continue;
            };

            // Substitute {path} in args with the actual file path
            let args: Vec<String> = step_def
                .args
                .iter()
                .map(|a| a.replace("{path}", &final_path_str))
                .collect();

            eprintln!(
                "running postprocess step '{step_name}': {} {:?}",
                step_def.program, args
            );

            let success = match std::process::Command::new(&step_def.program)
                .args(&args)
                .status()
            {
                Ok(status) if status.success() => {
                    eprintln!("postprocess step '{step_name}' succeeded");
                    true
                }
                Ok(status) => {
                    eprintln!(
                        "postprocess step '{step_name}' failed with exit code: {}",
                        status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".to_owned())
                    );
                    false
                }
                Err(e) => {
                    eprintln!("postprocess step '{step_name}' error: {e}");
                    false
                }
            };
            results.push((step_name.clone(), success));
        }
    }

    results
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
        cmd.push_str(&shell_escape(a));
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::{Utf8Path, Utf8PathBuf};
    use purgery_core::{ManifestFileEntry, NormalizedRelativePath, PostprocessConfig, ServerRoot};

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
                sync_name: "videos".into(),
                local_path: "/home/user/Videos/test.mp4".into(),
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
                sync_name: "unknown-sync".into(),
                local_path: "/tmp/test.mp4".into(),
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
                sync_name: "videos".into(),
                local_path: "/home/user/Videos/missing.mp4".into(),
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
                sync_name: "videos".into(),
                local_path: "/tmp/a.mp4".into(),
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
                sync_name: "videos".into(),
                local_path: "/tmp/a.mp4".into(),
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
                        purgery_core::PostprocessStepDefinition {
                            kind: "builtin".to_owned(),
                            program: "true".to_owned(),
                            args: vec!["--input".to_owned(), "{path}".to_owned()],
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
        let results = apply_postprocessing(
            &server_config,
            &client_config,
            "videos/some file.mp4",
            Utf8Path::new("/data/laptop/videos/some file.mp4"),
        );
        // Should produce one result with success=true (true always exits 0)
        assert_eq!(results.len(), 1);
        assert!(results[0].1, "postprocess with spaces should succeed");
    }
}
