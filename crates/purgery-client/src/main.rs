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

#[derive(Parser)]
#[command(
    name = "purgery-client",
    about = "Purgery client: sync files to server and clean up imported files",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
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
    SyncAndCleanup {
        /// Path to client configuration TOML
        #[arg(long)]
        config: String,
    },
    /// Check client dependencies and configuration (local only, no SSH)
    Check {
        /// Path to client configuration TOML
        #[arg(long)]
        config: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Extract config path from whichever subcommand is active
    let config_path = match &cli.command {
        Command::SyncAndCleanup { config } => config.as_str(),
        Command::Check { config } => config.as_str(),
    };

    // Load client config first — needed for logging settings
    let config_content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read client config: {config_path}"))?;
    let config = ClientConfig::from_toml(&config_content)
        .with_context(|| "failed to parse client config")?;

    // Merge logging: start with config's logging settings, then apply CLI overrides.
    // Precedence: CLI > config > default.
    let mut log_cfg = config.logging.clone();
    apply_cli_overrides(&mut log_cfg, &cli)?;
    purgery_core::init_logging(&log_cfg)
        .map_err(|e| anyhow::anyhow!("failed to initialize logging: {e}"))?;

    match cli.command {
        Command::SyncAndCleanup { .. } => {
            sync_and_cleanup(&config)?;
        }
        Command::Check { .. } => {
            client_check(&config, config_path)?;
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
    use camino::Utf8Path;
    use clap::Parser;
    use purgery_core::{
        ClientConfig, EntryStatusEntry, FileStatus, ManifestEntry, ManifestEntryKind, RunId,
        RunState, RunStatus,
    };
    use std::path::Path;

    fn config_for(source: &Path) -> ClientConfig {
        ClientConfig::from_toml(&format!(
            r#"
nickname = "laptop"

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
}
