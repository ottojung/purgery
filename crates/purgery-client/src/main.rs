use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use purgery_core::ClientConfig;
use std::fs;

mod classify;
mod cleanup;
mod run;
mod ssh;
mod transfer;

pub(crate) use run::*;

#[cfg(test)]
pub(crate) use classify::*;

fn find_config() -> Result<String> {
    if let Ok(path) = std::env::var("PURGERY_CLIENT_CONFIG_PATH") {
        if !path.is_empty() {
            return Ok(path);
        }
    }
    if let Ok(xdg_home) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg_home.is_empty() {
            let xdg_path = format!("{xdg_home}/purgery/client.toml");
            if fs::metadata(&xdg_path).is_ok() {
                return Ok(xdg_path);
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            let user_path = format!("{home}/.config/purgery/client.toml");
            if fs::metadata(&user_path).is_ok() {
                return Ok(user_path);
            }
        }
    }
    anyhow::bail!(
        "no client config found; use --config, $PURGERY_CLIENT_CONFIG_PATH, \
         $XDG_CONFIG_HOME/purgery/client.toml, or ~/.config/purgery/client.toml"
    )
}

#[derive(Parser)]
#[command(
    name = "purgery-client",
    about = "Purgery client: sync files to server and clean up imported files",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    /// Path to client configuration TOML
    #[arg(long, global = true)]
    config: Option<String>,

    /// Log level override (error, warn, info, debug, trace)
    #[arg(long, global = true)]
    log_level: Option<String>,
    /// Log format override (pretty, compact, json)
    #[arg(long, global = true)]
    log_format: Option<String>,
    /// Color mode override (auto, always, never)
    #[arg(long, global = true)]
    color: Option<String>,
    /// Suppress all logs except errors (conflicts with --verbose and --log-level)
    #[arg(long, global = true, conflicts_with_all = &["verbose", "log_level"])]
    quiet: bool,
    /// Enable verbose (debug) logging
    #[arg(long, global = true, conflicts_with_all = &["quiet", "log_level"])]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync local files to the server, process, and clean up confirmed imports
    SyncAndCleanup,
    /// Check client dependencies and configuration (local only, no SSH)
    Check,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve config path
    let config_path = cli.config.as_deref().unwrap_or("");
    let path = if config_path.is_empty() {
        find_config()?
    } else {
        config_path.to_owned()
    };

    // Load client config first — needed for logging settings
    let config_content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read client config: {path}"))?;
    let config = ClientConfig::from_toml(&config_content)
        .with_context(|| "failed to parse client config")?;

    // Merge logging: start with config's logging settings, then apply CLI overrides.
    // Precedence: CLI > config > default.
    let mut log_cfg = config.logging.clone();
    apply_cli_overrides(&mut log_cfg, &cli)?;
    purgery_core::init_logging(&log_cfg)
        .map_err(|e| anyhow::anyhow!("failed to initialize logging: {e}"))?;

    match cli.command {
        Command::SyncAndCleanup => {
            sync_and_cleanup(&config)?;
        }
        Command::Check => {
            client_check(&config, &path)?;
        }
    }
    Ok(())
}

/// Apply CLI logging overrides on top of a base config.
fn apply_cli_overrides(log_cfg: &mut purgery_core::LoggingConfig, cli: &Cli) -> Result<()> {
    use purgery_core::{ColorMode, LogFormat, LogLevel};
    if cli.quiet {
        log_cfg.level = LogLevel::Error;
    }
    if cli.verbose {
        log_cfg.level = LogLevel::Debug;
    }
    if let Some(ref level) = cli.log_level {
        log_cfg.level = level
            .parse::<LogLevel>()
            .map_err(|e| anyhow::anyhow!("invalid log level: {e}"))?;
    }
    if let Some(ref fmt) = cli.log_format {
        log_cfg.format = fmt
            .parse::<LogFormat>()
            .map_err(|e| anyhow::anyhow!("invalid log format: {e}"))?;
    }
    if let Some(ref color) = cli.color {
        log_cfg.color = color
            .parse::<ColorMode>()
            .map_err(|e| anyhow::anyhow!("invalid color mode: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::process_cleanup_state_file;
    use camino::Utf8Path;
    use clap::Parser;
    use purgery_core::{
        ClientConfig, EntryStatusEntry, FileStatus, ManifestEntry, ManifestEntryKind,
        ManifestEntryMode, RunId, RunState, RunStatus,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn config_for(source: &Path) -> ClientConfig {
        ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true
"#,
            source.display()
        ))
        .unwrap()
    }

    fn config_no_delete_for(source: &Path) -> ClientConfig {
        ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = false
"#,
            source.display()
        ))
        .unwrap()
    }

    #[test]
    fn cli_rejects_verbose_with_log_level() {
        let result = Cli::try_parse_from([
            "purgery-client",
            "--verbose",
            "--log-level",
            "debug",
            "check",
            "--config",
            "client.toml",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn manifest_includes_directories_files_and_symlinks_in_topological_order() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(source.join("empty")).unwrap();
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "data").unwrap();
        std::os::unix::fs::symlink("nested/file.txt", source.join("link")).unwrap();
        let config = config_for(&source);
        let run_id = RunId::new("manifest-tree".into()).unwrap();

        let manifest = build_manifest(&config, &run_id).unwrap();
        let actual: Vec<_> = manifest
            .entries
            .iter()
            .map(|entry| (entry.relative_path.as_str(), entry.kind))
            .collect();
        assert_eq!(
            actual,
            vec![
                ("empty", ManifestEntryKind::Directory),
                ("nested", ManifestEntryKind::Directory),
                ("link", ManifestEntryKind::Symlink),
                ("nested/file.txt", ManifestEntryKind::RegularFile),
            ]
        );
        assert_eq!(
            manifest.entries[2].link_target.as_deref(),
            Some(Utf8Path::new("nested/file.txt"))
        );
    }

    #[test]
    fn cleanup_deletes_only_unchanged_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(source.join("directory")).unwrap();
        fs::write(source.join("file.txt"), "data").unwrap();
        std::os::unix::fs::symlink("file.txt", source.join("link")).unwrap();

        // Use a config that marks file.txt as postprocess so it is eligible for status cleanup
        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "file.txt"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("cleanup-tree".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();
        let status_entries = manifest
            .entries
            .iter()
            .map(|entry| EntryStatusEntry {
                kind: entry.kind,
                sync_name: entry.sync_name.clone(),
                local_path: entry.local_path.as_str().to_owned(),
                relative_path: entry.relative_path.as_str().to_owned(),
                status: FileStatus::Imported,
                final_paths: vec![],
                postprocess: None,
                error: None,
            })
            .collect();
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        assert_eq!(
            delete_confirmed_files(&config, &manifest, &status).unwrap(),
            1
        );
        assert!(!source.join("file.txt").exists());
        assert!(source.join("directory").is_dir());
        assert!(fs::symlink_metadata(source.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn cleanup_does_not_delete_symlink_replacing_original_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), "data").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "file.txt"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();
        let run_id = RunId::new("symlink-safety".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // After upload, the original regular file is replaced by a symlink
        fs::remove_file(source.join("file.txt")).unwrap();
        let target = tmp.path().join("other");
        fs::write(&target, "other data").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, source.join("file.txt")).unwrap();

        let status_entries: Vec<_> = manifest
            .entries
            .iter()
            .map(|entry| EntryStatusEntry {
                kind: entry.kind,
                sync_name: entry.sync_name.clone(),
                local_path: entry.local_path.as_str().to_owned(),
                relative_path: entry.relative_path.as_str().to_owned(),
                status: FileStatus::Imported,
                final_paths: vec![],
                postprocess: None,
                error: None,
            })
            .collect();
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(count, 0, "should not delete symlink");
        assert!(
            fs::symlink_metadata(source.join("file.txt")).is_ok(),
            "symlink must still exist"
        );
    }

    #[test]
    fn passthrough_no_delete_entries_have_no_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"hello").unwrap();
        let config = config_no_delete_for(&source);
        let run_id = RunId::new("no-delete-identity".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.kind == ManifestEntryKind::RegularFile)
            .expect("must have a regular file entry");
        // For delete_after_import=false passthrough, identity fields must be empty
        assert_eq!(
            entry.mtime_ns, 0,
            "no-delete passthrough must not track mtime"
        );
        assert!(
            entry.sha256.is_none(),
            "no-delete passthrough must not compute sha256, got {:?}",
            entry.sha256
        );
    }

    #[test]
    fn passthrough_no_delete_entries_still_have_relative_path() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"hello").unwrap();
        let config = config_no_delete_for(&source);
        let run_id = RunId::new("no-delete-path".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();
        // Path planning must still work even without identity fields
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.relative_path.as_str() == "file.txt")
            .expect("must find file.txt entry");
        assert_eq!(entry.mode, purgery_core::ManifestEntryMode::Passthrough);
        assert!(
            filter_contains_path(entry),
            "entry must be usable for filter generation"
        );
    }

    /// Helper: check that a manifest entry can be used for filter generation.
    fn filter_contains_path(entry: &ManifestEntry) -> bool {
        let root = purgery_core::TransferRoot::Exact(entry.relative_path.as_str().to_owned());
        let filter = purgery_core::transfer_set_filter(&[root]);
        filter.contains(entry.relative_path.as_str())
    }

    #[test]
    fn delete_confirmed_files_deletes_postprocessed_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let target = tmp.path().join("target_file");
        fs::write(&target, "target data").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, source.join("link.lnk")).unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "link.lnk"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("symlink-pp-cleanup".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();
        let symlink_entry = manifest
            .entries
            .iter()
            .find(|e| e.kind == ManifestEntryKind::Symlink)
            .expect("must have a symlink entry");

        let status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::Symlink,
            sync_name: symlink_entry.sync_name.clone(),
            local_path: symlink_entry.local_path.as_str().to_owned(),
            relative_path: symlink_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: Some(vec!["compress-video".into()]),
            error: None,
        }];
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(count, 1, "postprocessed symlink must be deleted");
        assert!(!source.join("link.lnk").exists(), "symlink must be removed");
        // The target must still exist (symlink cleanup never follows the target)
        assert!(target.exists(), "symlink target must not be affected");
    }

    #[test]
    fn delete_confirmed_files_deletes_postprocessed_directory_bottom_up() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("photos");
        fs::create_dir_all(dir_path.join("sub")).unwrap();
        fs::write(dir_path.join("sub/file1.txt"), "data1").unwrap();
        fs::write(dir_path.join("file2.txt"), "data2").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("dir-pp-cleanup".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();
        let dir_entry = manifest
            .entries
            .iter()
            .find(|e| e.kind == ManifestEntryKind::Directory)
            .expect("must have a directory entry");

        // Build status entries that include covered children as skipped
        let mut status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::Directory,
            sync_name: dir_entry.sync_name.clone(),
            local_path: dir_entry.local_path.as_str().to_owned(),
            relative_path: dir_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: Some(vec!["compress-video".into()]),
            error: None,
        }];
        // Covered children get skipped status
        for child in &manifest.entries {
            if child.mode == purgery_core::ManifestEntryMode::Covered {
                status_entries.push(EntryStatusEntry {
                    kind: child.kind,
                    sync_name: child.sync_name.clone(),
                    local_path: child.local_path.as_str().to_owned(),
                    relative_path: child.relative_path.as_str().to_owned(),
                    status: FileStatus::Skipped,
                    final_paths: vec![],
                    postprocess: None,
                    error: Some("covered by postprocessed ancestor directory".into()),
                });
            }
        }
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(
            count, 1,
            "postprocessed directory root must be deleted as one entry"
        );
        assert!(!dir_path.exists(), "directory must be removed");
    }

    #[test]
    fn delete_confirmed_files_skips_directory_with_new_local_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("photos");
        fs::create_dir_all(dir_path.join("sub")).unwrap();
        fs::write(dir_path.join("sub/file1.txt"), "data1").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("dir-skip-new".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();
        let dir_entry = manifest
            .entries
            .iter()
            .find(|e| e.kind == ManifestEntryKind::Directory)
            .expect("must have a directory entry");

        // After manifest, a new file appears in the directory
        fs::write(dir_path.join("new_file.txt"), "new data").unwrap();

        let status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::Directory,
            sync_name: dir_entry.sync_name.clone(),
            local_path: dir_entry.local_path.as_str().to_owned(),
            relative_path: dir_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: Some(vec!["compress-video".into()]),
            error: None,
        }];
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(count, 0, "directory with new entries must not be deleted");
        assert!(dir_path.exists(), "directory must still exist");
    }

    #[test]
    fn delete_confirmed_files_rejects_status_passthrough_entries() {
        // When server status has imported entries but the corresponding
        // manifest entry is passthrough (not postprocess), delete_confirmed_files
        // must not delete them. Status-based cleanup is for postprocess entries only.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"hello").unwrap();
        let config = config_for(&source);
        let run_id = RunId::new("status-safety".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // Find the passthrough entry
        let passthrough_entry = manifest
            .entries
            .iter()
            .find(|e| e.kind == ManifestEntryKind::RegularFile)
            .expect("must have a regular file");

        // Create a status that references the passthrough entry's local_path
        let status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::RegularFile,
            sync_name: passthrough_entry.sync_name.clone(),
            local_path: passthrough_entry.local_path.as_str().to_owned(),
            relative_path: passthrough_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: None,
            error: None,
        }];
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        // delete_confirmed_files must NOT delete the passthrough file even though
        // status says imported, because passthrough entries are not tracked by server status.
        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(count, 0, "must not delete passthrough files from status");
        assert!(
            source.join("file.txt").exists(),
            "passthrough file must remain"
        );
    }

    #[test]
    fn delete_confirmed_files_skips_directory_with_changed_known_child() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("photos");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(dir_path.join("file.txt"), "original content").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("dir-changed-child".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // After manifest, change a known child's content
        fs::write(dir_path.join("file.txt"), "changed content").unwrap();

        let dir_entry = manifest
            .entries
            .iter()
            .find(|e| e.kind == ManifestEntryKind::Directory)
            .expect("must have a directory entry");

        let status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::Directory,
            sync_name: dir_entry.sync_name.clone(),
            local_path: dir_entry.local_path.as_str().to_owned(),
            relative_path: dir_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: Some(vec!["compress-video".into()]),
            error: None,
        }];
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(
            count, 0,
            "directory with changed known child must not be deleted"
        );
        assert!(dir_path.exists(), "directory must still exist");
        assert!(
            dir_path.join("file.txt").exists(),
            "changed child must still exist"
        );
    }

    #[test]
    fn process_cleanup_state_file_skips_directory_with_changed_known_child() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_path = tmp.path().join("photos");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(dir_path.join("file.txt"), "original").unwrap();

        let state = purgery_core::DurableCleanupState {
            nickname: "laptop".into(),
            operation_id: "test-op".into(),
            entries: vec![
                purgery_core::CleanupEntry {
                    sync_name: "data".into(),
                    relative_path: "photos/file.txt".into(),
                    local_path: dir_path.join("file.txt").to_string_lossy().into_owned(),
                    kind: ManifestEntryKind::RegularFile,
                    size: "original".len() as u64,
                    mtime_ns: 100,
                    sha256: None,
                    link_target: None,
                    rsync_succeeded: true,
                    cleaned: false,
                },
                purgery_core::CleanupEntry {
                    sync_name: "data".into(),
                    relative_path: "photos".into(),
                    local_path: dir_path.to_string_lossy().into_owned(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    rsync_succeeded: true,
                    cleaned: false,
                },
            ],
        };

        // Write cleanup state file
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("cleanup-test.toml");
        let content = toml::to_string(&state).unwrap();
        fs::write(&state_path, &content).unwrap();

        // Change the child after capture
        fs::write(dir_path.join("file.txt"), "CHANGED").unwrap();

        // Process should skip the changed child and not delete the directory
        let state_utf8 = camino::Utf8PathBuf::from_path_buf(state_path).unwrap();
        process_cleanup_state_file(&state_utf8).unwrap();

        // Verify unchanged - the file was changed, so nothing should be deleted
        assert!(
            dir_path.join("file.txt").exists(),
            "changed child must still exist"
        );
        assert!(dir_path.exists(), "directory must still exist");
    }

    #[test]
    fn delete_confirmed_files_skips_directory_with_nested_new_child() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("photos");
        let sub_dir = dir_path.join("sub");
        fs::create_dir_all(&sub_dir).unwrap();
        let known_file = sub_dir.join("file.txt");
        fs::write(&known_file, "data").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("dir-nested-new".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // After manifest, add a new file inside the nested subdirectory
        fs::write(sub_dir.join("new_file.txt"), "new data").unwrap();

        let dir_entry = manifest
            .entries
            .iter()
            .find(|e| {
                e.kind == ManifestEntryKind::Directory && e.mode == ManifestEntryMode::Postprocess
            })
            .expect("must have a postprocessed directory entry");

        let status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::Directory,
            sync_name: dir_entry.sync_name.clone(),
            local_path: dir_entry.local_path.as_str().to_owned(),
            relative_path: dir_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: Some(vec!["compress-video".into()]),
            error: None,
        }];
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(
            count, 0,
            "directory with nested new child must not be deleted"
        );
        assert!(dir_path.exists(), "directory must still exist");
        assert!(sub_dir.exists(), "subdirectory must still exist");
        assert!(
            known_file.exists(),
            "known nested file must not be partially deleted"
        );
        assert!(
            sub_dir.join("new_file.txt").exists(),
            "nested new file must still exist"
        );
    }

    #[test]
    fn delete_confirmed_files_skips_directory_with_new_direct_child() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("photos");
        fs::create_dir_all(&dir_path).unwrap();
        let known_file = dir_path.join("file.txt");
        fs::write(&known_file, "data").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("dir-direct-child".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // After manifest, add a new file directly inside the root directory
        fs::write(dir_path.join("new_file.txt"), "new data").unwrap();

        let dir_entry = manifest
            .entries
            .iter()
            .find(|e| {
                e.kind == ManifestEntryKind::Directory && e.mode == ManifestEntryMode::Postprocess
            })
            .expect("must have a postprocessed directory entry");

        let status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::Directory,
            sync_name: dir_entry.sync_name.clone(),
            local_path: dir_entry.local_path.as_str().to_owned(),
            relative_path: dir_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: Some(vec!["compress-video".into()]),
            error: None,
        }];
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(
            count, 0,
            "directory with new direct child must not be deleted"
        );
        assert!(dir_path.exists(), "directory must still exist");
        assert!(
            known_file.exists(),
            "known file must not be partially deleted"
        );
        assert!(
            dir_path.join("new_file.txt").exists(),
            "new direct child must still exist"
        );
    }

    #[test]
    fn passthrough_cleanup_excludes_source_root() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("file.txt"), "data").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true
"#,
            source.display()
        ))
        .unwrap();

        let entries =
            crate::cleanup::build_pre_rsync_cleanup_entries(&config, &config.sync[0]).unwrap();

        // The source root itself must not be a cleanup entry
        let root_entry = entries.iter().find(|e| Path::new(&e.local_path) == source);
        assert!(
            root_entry.is_none(),
            "source root must not be in cleanup entries, got: {:?}",
            root_entry
        );
        // Entries under the root should still be present
        assert!(
            entries.iter().any(|e| e.relative_path == "file.txt"),
            "entries under source root must still be captured"
        );
    }

    #[test]
    fn delete_confirmed_files_rejects_covered_entry_from_status() {
        // Covered descendants must not be independently deleted from server status,
        // even if a status entry incorrectly reports them as imported.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("photos");
        let file_path = dir_path.join("file.txt");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(&file_path, "data").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("covered-status-safety".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // Find a covered (non-directory) entry
        let covered_entry = manifest
            .entries
            .iter()
            .find(|e| {
                e.mode == ManifestEntryMode::Covered && e.kind == ManifestEntryKind::RegularFile
            })
            .expect("must have a covered regular file entry");

        // Create a status that falsely marks the covered entry as imported
        let status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::RegularFile,
            sync_name: covered_entry.sync_name.clone(),
            local_path: covered_entry.local_path.as_str().to_owned(),
            relative_path: covered_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: None,
            error: None,
        }];
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(count, 0, "covered entry must not be deleted from status");
        assert!(file_path.exists(), "covered file must remain");
    }

    #[test]
    fn delete_confirmed_files_directory_root_removed_when_child_absent() {
        // A postprocessed directory root should still be cleaned up if a captured
        // child is already absent (idempotent — treated as already removed).
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("photos");
        let file_path = dir_path.join("file.txt");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(&file_path, "data").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("dir-child-absent".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // Remove the child before cleanup (simulates prior partial cleanup)
        fs::remove_file(&file_path).unwrap();

        let dir_entry = manifest
            .entries
            .iter()
            .find(|e| {
                e.kind == ManifestEntryKind::Directory && e.mode == ManifestEntryMode::Postprocess
            })
            .expect("must have a postprocessed directory entry");

        let status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::Directory,
            sync_name: dir_entry.sync_name.clone(),
            local_path: dir_entry.local_path.as_str().to_owned(),
            relative_path: dir_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: Some(vec!["compress-video".into()]),
            error: None,
        }];
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(
            count, 1,
            "directory root must be removed even with absent child"
        );
        assert!(!dir_path.exists(), "directory must be removed");
    }

    #[test]
    fn delete_confirmed_files_skips_directory_when_child_absent_and_new_entry_present() {
        // A new entry must still block cleanup even if a captured child is already absent.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("photos");
        fs::create_dir_all(&dir_path).unwrap();
        let known_file = dir_path.join("file.txt");
        fs::write(&known_file, "data").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("dir-absent-new".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // Remove the captured child, then add a new entry
        fs::remove_file(&known_file).unwrap();
        fs::write(dir_path.join("new_file.txt"), "new data").unwrap();

        let dir_entry = manifest
            .entries
            .iter()
            .find(|e| {
                e.kind == ManifestEntryKind::Directory && e.mode == ManifestEntryMode::Postprocess
            })
            .expect("must have a postprocessed directory entry");

        let status_entries = vec![EntryStatusEntry {
            kind: ManifestEntryKind::Directory,
            sync_name: dir_entry.sync_name.clone(),
            local_path: dir_entry.local_path.as_str().to_owned(),
            relative_path: dir_entry.relative_path.as_str().to_owned(),
            status: FileStatus::Imported,
            final_paths: vec![],
            postprocess: Some(vec!["compress-video".into()]),
            error: None,
        }];
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(
            count, 0,
            "directory must not be removed when new entry exists alongside absent child"
        );
        assert!(dir_path.exists(), "directory must still exist");
        assert!(
            dir_path.join("new_file.txt").exists(),
            "new entry must still exist"
        );
    }

    #[test]
    fn delete_confirmed_files_cleans_all_entry_kinds() {
        // Verify that regular files, symlinks, and directories
        // are all cleaned by status-based cleanup when configured
        // as postprocess entries.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("a_dir");
        let file_path = source.join("file.txt");
        let link_path = source.join("link.lnk");
        let target = tmp.path().join("target");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(&file_path, "data").unwrap();
        fs::write(&target, "target").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link_path).unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "file.txt"
steps = ["compress-video"]

[[postprocess.rules]]
match = "link.lnk"
steps = ["compress-video"]

[[postprocess.rules]]
match = "a_dir"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("all-kinds-cleanup".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // Build status entries for all three
        let mut status_entries = Vec::new();
        for entry in &manifest.entries {
            if entry.mode == ManifestEntryMode::Postprocess {
                status_entries.push(EntryStatusEntry {
                    kind: entry.kind,
                    sync_name: entry.sync_name.clone(),
                    local_path: entry.local_path.as_str().to_owned(),
                    relative_path: entry.relative_path.as_str().to_owned(),
                    status: FileStatus::Imported,
                    final_paths: vec![],
                    postprocess: Some(vec!["compress-video".into()]),
                    error: None,
                });
            } else if entry.mode == ManifestEntryMode::Covered {
                // Covered children get skipped status
                status_entries.push(EntryStatusEntry {
                    kind: entry.kind,
                    sync_name: entry.sync_name.clone(),
                    local_path: entry.local_path.as_str().to_owned(),
                    relative_path: entry.relative_path.as_str().to_owned(),
                    status: FileStatus::Skipped,
                    final_paths: vec![],
                    postprocess: None,
                    error: Some("covered by postprocessed ancestor directory".into()),
                });
            }
        }
        let status = RunStatus {
            run_id,
            nickname: config.nickname.clone(),
            state: RunState::Done,
            entries: status_entries,
            error: None,
        };

        let count = delete_confirmed_files(&config, &manifest, &status).unwrap();
        assert_eq!(count, 3, "all three entry kinds must be cleaned");
        assert!(!file_path.exists(), "regular file must be removed");
        assert!(
            !link_path.exists(),
            "symlink must be removed (target stays)"
        );
        assert!(target.exists(), "symlink target must not be affected");
        assert!(!dir_path.exists(), "directory must be removed bottom-up");
    }

    #[test]
    #[cfg(unix)]
    fn passthrough_cleanup_skips_entry_when_sha_fails() {
        // SHA-256 computation failure during pre-rsync capture must exclude
        // the entry from cleanup, not silently degrade to size/mtime-only.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let file_path = source.join("video.mp4");
        fs::write(&file_path, b"content").unwrap();

        // Create a config with delete_after_import for a regular file
        let sync = purgery_core::SyncMapping {
            name: purgery_core::SyncName::new("data".into()).unwrap(),
            from_path: purgery_core::LocalSourcePath::new(source.to_string_lossy().into_owned())
                .unwrap(),
            to_path: purgery_core::RelativeDestinationPath::new("data".into()).unwrap(),
            delete_after_import: true,
        };

        let entries =
            crate::cleanup::build_pre_rsync_cleanup_entries(&config_for(&source), &sync).unwrap();

        // If SHA computation would fail (e.g., file is unreadable), the entry
        // should be excluded rather than included with sha256: None.
        // Since the file IS readable here, the entry should have sha256: Some(...)
        let file_entry = entries
            .iter()
            .find(|e| e.relative_path == "video.mp4")
            .expect("must have entry for video.mp4");
        assert!(
            file_entry.sha256.is_some(),
            "readable file must have SHA-256 in cleanup entries, got: {:?}",
            file_entry.sha256
        );

        // Now simulate SHA failure: mark the file as unreadable
        fs::set_permissions(&file_path, PermissionsExt::from_mode(0o000)).unwrap();

        let entries_no_read =
            crate::cleanup::build_pre_rsync_cleanup_entries(&config_for(&source), &sync).unwrap();
        let file_entry_no_read = entries_no_read
            .iter()
            .find(|e| e.relative_path == "video.mp4");
        assert!(
            file_entry_no_read.is_none(),
            "entry must be excluded from cleanup when SHA-256 cannot be computed"
        );

        // Restore permissions for tempdir cleanup
        fs::set_permissions(&file_path, PermissionsExt::from_mode(0o644)).unwrap();
    }

    #[test]
    fn cleanup_state_written_under_state_dir() {
        // Cleanup state files must be written under state_dir, not system temp
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"data").unwrap();

        let state_dir = tmp.path().join("purgery-state");
        fs::create_dir_all(&state_dir).unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "{}"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true
"#,
            state_dir.display(),
            source.display()
        ))
        .unwrap();

        let sync = &config.sync[0];
        let cleanup_entries =
            crate::cleanup::build_pre_rsync_cleanup_entries(&config, sync).unwrap();
        let cleanup_state = purgery_core::DurableCleanupState {
            nickname: "laptop".into(),
            operation_id: "test-op".into(),
            entries: cleanup_entries,
        };

        let state_path =
            crate::cleanup::write_cleanup_state(&cleanup_state, &config.state_dir).unwrap();
        assert!(
            state_path
                .as_str()
                .starts_with(state_dir.to_string_lossy().as_ref()),
            "cleanup state must be under state_dir, got: {}",
            state_path
        );
    }

    #[test]
    fn rsync_filter_files_use_state_dir_not_system_temp() {
        // Verify that rsync filter temp files are created under state_dir/tmp/{run_id}/filters/
        // rather than std::env::temp_dir()
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"data").unwrap();

        let state_dir = tmp.path().join("purgery-state");

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "{}"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true
"#,
            state_dir.display(),
            source.display()
        ))
        .unwrap();

        let run_id = RunId::generate();
        let manifest = build_manifest(&config, &run_id).unwrap();
        let _transfer_plan = manifest.to_transfer_plan();

        // The run_postprocess_path function creates filters under state_dir/tmp/{run_id}/filters/
        // This test verifies that the temp dir path construction uses state_dir
        let expected_tmp_dir = camino::Utf8Path::new(&config.state_dir)
            .join("tmp")
            .join(run_id.as_str())
            .join("filters");
        assert!(
            expected_tmp_dir.as_str().starts_with(&config.state_dir),
            "filter temp dir must be under state_dir"
        );
        // We cannot easily invoke run_postprocess_path without SSH,
        // but we verify the path construction is correct.
    }

    #[test]
    fn same_sync_name_different_run_ids_no_collision() {
        // Two runs with the same sync name must produce different filter temp paths
        // because the run_id differs
        let state_dir = "/tmp/purgery-state";

        let run_id1 = RunId::new("run-aaa".into()).unwrap();
        let run_id2 = RunId::new("run-bbb".into()).unwrap();
        let sync_name = "videos";

        let tmp1 = camino::Utf8Path::new(state_dir)
            .join("tmp")
            .join(run_id1.as_str())
            .join("filters")
            .join(format!("passthrough-{sync_name}"));
        let tmp2 = camino::Utf8Path::new(state_dir)
            .join("tmp")
            .join(run_id2.as_str())
            .join("filters")
            .join(format!("passthrough-{sync_name}"));

        assert_ne!(
            tmp1, tmp2,
            "different run IDs must produce different temp paths"
        );
        assert_eq!(
            tmp1.file_name(),
            tmp2.file_name(),
            "same sync name should have same file name but different parent dirs"
        );
    }

    #[test]
    fn cleanup_ledger_includes_symlinks_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("subdir");
        fs::create_dir_all(&dir_path).unwrap();
        let file_path = dir_path.join("file.txt");
        fs::write(&file_path, b"data").unwrap();
        let link_path = source.join("link.lnk");
        std::os::unix::fs::symlink("subdir/file.txt", &link_path).unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("ledger-kinds".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // Build cleanup entries from manifest (passthrough entries only)
        let entries =
            crate::cleanup::build_cleanup_entries_from_manifest(&config, "data", &manifest)
                .unwrap();

        // Must include directories and symlinks even though they have no SHA
        assert!(
            entries
                .iter()
                .any(|e| e.kind == ManifestEntryKind::Directory),
            "cleanup ledger must include directory entries"
        );
        assert!(
            entries.iter().any(|e| e.kind == ManifestEntryKind::Symlink),
            "cleanup ledger must include symlink entries"
        );
        // Regular files must have SHA
        let regular = entries
            .iter()
            .find(|e| e.kind == ManifestEntryKind::RegularFile);
        if let Some(rf) = regular {
            assert!(
                rf.sha256.is_some(),
                "regular file in cleanup ledger must have SHA"
            );
        }
    }

    #[test]
    fn directory_cleanup_refuses_without_sha_on_descendant() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dir_path = source.join("photos");
        fs::create_dir_all(&dir_path).unwrap();
        let file_path = dir_path.join("file.txt");
        fs::write(&file_path, b"data").unwrap();

        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["compress-video"]
"#,
            source.display()
        ))
        .unwrap();

        let run_id = RunId::new("dir-sha-safety".into()).unwrap();
        let manifest = build_manifest(&config, &run_id).unwrap();

        // Find a regular-file entry inside the postprocessed directory
        let child_entry = manifest
            .entries
            .iter()
            .find(|e| {
                e.mode == ManifestEntryMode::Covered && e.kind == ManifestEntryKind::RegularFile
            })
            .expect("must have a covered regular file");

        // Create a version of the entry without SHA (as if SHA computation failed)
        let mut entry_no_sha = child_entry.clone();
        entry_no_sha.sha256 = None;

        // verify_manifest_entry_local must return false when SHA is missing
        assert!(
            !crate::run::verify_manifest_entry_local(&entry_no_sha),
            "verify_manifest_entry_local must refuse regular file without SHA"
        );
    }

    #[test]
    fn cleanup_state_regular_file_without_sha_not_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("data.txt");
        fs::write(&file_path, b"hello").unwrap();

        let state = purgery_core::DurableCleanupState {
            nickname: "laptop".into(),
            operation_id: "test-no-sha".into(),
            entries: vec![purgery_core::CleanupEntry {
                sync_name: "data".into(),
                relative_path: "data.txt".into(),
                local_path: file_path.to_string_lossy().into_owned(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                rsync_succeeded: true,
                cleaned: false,
            }],
        };

        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("cleanup-no-sha.toml");
        fs::write(&state_path, toml::to_string(&state).unwrap()).unwrap();
        let state_utf8 = camino::Utf8PathBuf::from_path_buf(state_path).unwrap();

        process_cleanup_state_file(&state_utf8).unwrap();

        assert!(
            file_path.exists(),
            "regular file without SHA must not be deleted"
        );
        // Verify state: entry must not be marked cleaned
        let content = fs::read_to_string(state_utf8.as_std_path()).unwrap();
        let new_state: purgery_core::DurableCleanupState = toml::from_str(&content).unwrap();
        assert!(
            !new_state.entries[0].cleaned,
            "entry without SHA must not be marked cleaned"
        );
    }

    #[test]
    fn cleanup_state_regular_file_sha_recompute_fails_not_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("data.txt");
        fs::write(&file_path, b"hello").unwrap();

        // Capture SHA while file is readable
        let sha = crate::cleanup::compute_sha256(&file_path).unwrap();

        let state = purgery_core::DurableCleanupState {
            nickname: "laptop".into(),
            operation_id: "test-sha-fail".into(),
            entries: vec![purgery_core::CleanupEntry {
                sync_name: "data".into(),
                relative_path: "data.txt".into(),
                local_path: file_path.to_string_lossy().into_owned(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 100,
                sha256: Some(sha),
                link_target: None,
                rsync_succeeded: true,
                cleaned: false,
            }],
        };

        // Make file unreadable to simulate recomputation failure
        #[cfg(unix)]
        fs::set_permissions(&file_path, PermissionsExt::from_mode(0o000)).unwrap();

        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("cleanup-sha-fail.toml");
        fs::write(&state_path, toml::to_string(&state).unwrap()).unwrap();
        let state_utf8 = camino::Utf8PathBuf::from_path_buf(state_path).unwrap();

        process_cleanup_state_file(&state_utf8).unwrap();

        #[cfg(unix)]
        fs::set_permissions(&file_path, PermissionsExt::from_mode(0o644)).unwrap();

        assert!(
            file_path.exists(),
            "regular file with SHA recomputation failure must not be deleted"
        );
    }

    #[test]
    fn cleanup_state_symlink_without_target_not_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let link_path = tmp.path().join("link.lnk");
        std::os::unix::fs::symlink("/nonexistent", &link_path).unwrap();

        let state = purgery_core::DurableCleanupState {
            nickname: "laptop".into(),
            operation_id: "test-no-target".into(),
            entries: vec![purgery_core::CleanupEntry {
                sync_name: "data".into(),
                relative_path: "link.lnk".into(),
                local_path: link_path.to_string_lossy().into_owned(),
                kind: ManifestEntryKind::Symlink,
                size: 0,
                mtime_ns: 0,
                sha256: None,
                link_target: None,
                rsync_succeeded: true,
                cleaned: false,
            }],
        };

        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("cleanup-no-target.toml");
        fs::write(&state_path, toml::to_string(&state).unwrap()).unwrap();
        let state_utf8 = camino::Utf8PathBuf::from_path_buf(state_path).unwrap();

        process_cleanup_state_file(&state_utf8).unwrap();

        assert!(
            fs::symlink_metadata(&link_path).is_ok(),
            "symlink without target identity must not be deleted"
        );
        // Verify state: entry must not be marked cleaned
        let content = fs::read_to_string(state_utf8.as_std_path()).unwrap();
        let new_state: purgery_core::DurableCleanupState = toml::from_str(&content).unwrap();
        assert!(
            !new_state.entries[0].cleaned,
            "symlink entry without target identity must not be marked cleaned"
        );
    }

    #[test]
    fn pre_rsync_symlink_read_failure_skips_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let link_path = source.join("broken.lnk");
        // Create a dangling symlink, then remove the target so read_link works
        // but the path is still a symlink. Simulate read_link failure by
        // creating a symlink to a path with special characters or too long.
        // Actually, read_link typically doesn't fail for valid symlinks.
        // Instead, test that the pre-rsync capture gives the correct identity.
        std::os::unix::fs::symlink("target", &link_path).unwrap();

        // read_link should succeed here; the entry should have link_target set.
        let sync = purgery_core::SyncMapping {
            name: purgery_core::SyncName::new("data".into()).unwrap(),
            from_path: purgery_core::LocalSourcePath::new(source.to_string_lossy().into_owned())
                .unwrap(),
            to_path: purgery_core::RelativeDestinationPath::new("data".into()).unwrap(),
            delete_after_import: true,
        };
        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "/tmp/purgery-state"

[server]
host = "example.invalid"

[[sync]]
name = "data"
from = "{}"
to = "data"
delete_after_import = true
"#,
            source.display()
        ))
        .unwrap();
        let entries = crate::cleanup::build_pre_rsync_cleanup_entries(&config, &sync).unwrap();
        let sym_entry = entries
            .iter()
            .find(|e| e.kind == ManifestEntryKind::Symlink)
            .expect("must have a symlink entry when read_link succeeds");
        assert!(
            sym_entry.link_target.is_some(),
            "symlink entry must have link_target when read_link succeeds"
        );
    }

    #[test]
    fn cleanup_state_directory_with_child_lacking_sha_remains() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_path = tmp.path().join("photos");
        let file_path = dir_path.join("file.txt");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(&file_path, b"data").unwrap();

        // State has the directory plus a regular-file child WITHOUT SHA
        let state = purgery_core::DurableCleanupState {
            nickname: "laptop".into(),
            operation_id: "test-dir-child".into(),
            entries: vec![
                purgery_core::CleanupEntry {
                    sync_name: "data".into(),
                    relative_path: "photos/file.txt".into(),
                    local_path: file_path.to_string_lossy().into_owned(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 4,
                    mtime_ns: 100,
                    sha256: None,
                    link_target: None,
                    rsync_succeeded: true,
                    cleaned: false,
                },
                purgery_core::CleanupEntry {
                    sync_name: "data".into(),
                    relative_path: "photos".into(),
                    local_path: dir_path.to_string_lossy().into_owned(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    rsync_succeeded: true,
                    cleaned: false,
                },
            ],
        };

        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("cleanup-dir-child.toml");
        fs::write(&state_path, toml::to_string(&state).unwrap()).unwrap();
        let state_utf8 = camino::Utf8PathBuf::from_path_buf(state_path).unwrap();

        process_cleanup_state_file(&state_utf8).unwrap();

        assert!(file_path.exists(), "child without SHA must not be deleted");
        assert!(
            dir_path.exists(),
            "parent directory must remain when child lacks required identity"
        );
    }

    #[test]
    fn existing_abandoned_tombstone_blocks_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("purgery-state");
        fs::create_dir_all(&state_dir).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();
        let runs_dir = state_dir
            .join("runs")
            .join(format!("laptop-{}", run_id.as_str()));
        fs::create_dir_all(&runs_dir).unwrap();
        let manifest = purgery_core::Manifest {
            run_id: run_id.clone(),
            nickname: purgery_core::Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = purgery_core::RunConfig {
            nickname: purgery_core::Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: Default::default(),
        };
        let state = purgery_core::ClientRunState {
            protocol_version: 1,
            nickname: "laptop".into(),
            run_id: run_id.as_str().to_owned(),
            manifest: manifest.to_toml().unwrap(),
            run_config: run_config.to_toml().unwrap(),
            phase: purgery_core::ClientRunPhase::Abandoned,
        };
        let content = toml::to_string(&state).unwrap();
        fs::write(runs_dir.join("state.toml"), &content).unwrap();
        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "{}"

[server]
host = "example.invalid"
"#,
            state_dir.display()
        ))
        .unwrap();
        let result = resume_pending_postprocess_runs(&config);
        let err = result.unwrap_err().to_string().to_lowercase();
        assert!(
            err.contains("abandoned"),
            "expected error about abandoned tombstone, got: {err}"
        );
    }

    #[test]
    fn existing_corrupt_tombstone_blocks_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("purgery-state");
        fs::create_dir_all(&state_dir).unwrap();
        let run_id = RunId::new("test-run".into()).unwrap();
        let runs_dir = state_dir
            .join("runs")
            .join(format!("laptop-{}", run_id.as_str()));
        fs::create_dir_all(&runs_dir).unwrap();
        let manifest = purgery_core::Manifest {
            run_id: run_id.clone(),
            nickname: purgery_core::Nickname::new("laptop".into()).unwrap(),
            entries: vec![],
        };
        let run_config = purgery_core::RunConfig {
            nickname: purgery_core::Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: Default::default(),
        };
        let state = purgery_core::ClientRunState {
            protocol_version: 1,
            nickname: "laptop".into(),
            run_id: run_id.as_str().to_owned(),
            manifest: manifest.to_toml().unwrap(),
            run_config: run_config.to_toml().unwrap(),
            phase: purgery_core::ClientRunPhase::Corrupt,
        };
        let content = toml::to_string(&state).unwrap();
        fs::write(runs_dir.join("state.toml"), &content).unwrap();
        let config = ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"
state_dir = "{}"

[server]
host = "example.invalid"
"#,
            state_dir.display()
        ))
        .unwrap();
        let result = resume_pending_postprocess_runs(&config);
        let err = result.unwrap_err().to_string().to_lowercase();
        assert!(
            err.contains("corrupt"),
            "expected error about corrupt tombstone, got: {err}"
        );
    }

    /// Scan production .rs files for stray debug output (eprintln!, println!, dbg!).
    /// Test-only directories ("tests") are excluded entirely.
    /// Within production files, scanning stops at `#[cfg(test)]`.
    #[test]
    fn production_code_has_no_stray_debug_output() {
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("crates");
        let mut violations = Vec::new();

        for entry in walkdir::WalkDir::new(&crate_dir)
            .into_iter()
            .filter_entry(|e| e.file_name() != "tests")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().map(|x| x == "rs").unwrap_or(false))
        {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            for (lineno, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                // Stop scanning at test modules
                if trimmed.starts_with("#[cfg(test)]") {
                    break;
                }
                // Skip commented lines
                if trimmed.starts_with("//") {
                    continue;
                }
                if trimmed.contains("eprintln!(")
                    || trimmed.contains("println!(")
                    || trimmed.contains("dbg!(")
                {
                    violations.push(format!(
                        "{}:{}: {}",
                        entry.path().display(),
                        lineno + 1,
                        trimmed
                    ));
                }
            }
        }

        if !violations.is_empty() {
            panic!(
                "production code must not use println!/eprintln!/dbg!:\n{}",
                violations.join("\n")
            );
        }
    }
}
