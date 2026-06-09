use anyhow::{Context, Result};
use camino::Utf8Path;
use clap::{Parser, Subcommand};
use purgery_core::{
    build_rsync_args, resolve_executable, shell_escape, BeginRunResponse, ClientConfig,
    ClientLocalPath, Manifest, ManifestEntry, ManifestEntryKind, NormalizedRelativePath, RunConfig,
    RunConfigSync, RunId, RunStatus,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};
use tracing::{debug, info, span, warn, Level};
use walkdir::WalkDir;

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

/// Run a command via SSH.
fn ssh_run(host: &str, cmd: &str) -> Result<String> {
    let output = std::process::Command::new("ssh")
        .arg(host)
        .arg(cmd)
        .output()
        .with_context(|| format!("failed to execute SSH command on {host}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("SSH command on {host} failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run the server command over SSH and return stdout.
fn server_cmd(host: &str, server_command: &str, args: &[&str]) -> Result<String> {
    let full_cmd = {
        let mut cmd = server_command.to_owned();
        for a in args {
            cmd.push(' ');
            cmd.push_str(&shell_escape(a));
        }
        cmd
    };
    ssh_run(host, &full_cmd)
}

/// Read a remote file via SSH.
#[allow(dead_code)]
fn read_remote_file(host: &str, path: &str) -> Result<String> {
    ssh_run(host, &format!("cat {}", shell_escape(path)))
}

/// Write content to a remote file via SSH.
fn write_remote_file(host: &str, path: &str, content: &str) -> Result<()> {
    let remote_cmd = format!("cat > {}", purgery_core::shell_escape(path));
    let mut child = std::process::Command::new("ssh")
        .arg(host)
        .arg(&remote_cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn SSH write to {host}:{path}"))?;

    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content.as_bytes())
            .with_context(|| "failed to write content to SSH stdin")?;
    }

    let output = child
        .wait_with_output()
        .with_context(|| "failed to wait for SSH write")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to write remote file {path} on {host}: {stderr}");
    }

    Ok(())
}

/// Build a RunConfig from a ClientConfig — strips server-local fields.
fn build_run_config(config: &ClientConfig) -> RunConfig {
    let sync: Vec<RunConfigSync> = config
        .sync
        .iter()
        .map(|s| RunConfigSync {
            name: s.name.clone(),
            to_path: s.to_path.clone(),
        })
        .collect();

    RunConfig {
        nickname: config.nickname.clone(),
        sync,
        postprocess: config.postprocess.clone(),
    }
}

/// Client boot-time check: verify local executables and config only.
/// Does NOT SSH into the server or mutate anything.
fn client_check(config: &ClientConfig, _config_path: &str) -> Result<()> {
    info!("checking client configuration");

    resolve_executable("ssh").map(|r| {
        info!(path = %r.path.as_str(), "ssh: found");
    })?;

    resolve_executable("rsync").map(|r| {
        info!(path = %r.path.as_str(), "rsync: found");
    })?;

    if config.server.host.as_str().is_empty() {
        anyhow::bail!("server host is empty");
    }
    if config.server.command.is_empty() {
        anyhow::bail!("server command is empty");
    }

    info!("client configuration: OK");
    Ok(())
}

fn sync_and_cleanup(config: &ClientConfig) -> Result<()> {
    // 0. Run local checks before any remote operations
    client_check(config, "")?;

    // 1. Generate a unique run ID and build manifest BEFORE begin-run
    let run_id = RunId::generate();
    let host = config.server.host.as_str();
    let server_command = &config.server.command;

    let _span = span!(
        Level::INFO,
        "client run",
        nickname = %config.nickname.as_str(),
        run_id = %run_id.as_str()
    )
    .entered();

    let manifest = build_manifest(config, &run_id)?;
    info!(entries = manifest.entries.len(), "manifest built");

    // 2. Begin run on server — get server-derived paths
    let begin_out = server_cmd(
        host,
        server_command,
        &[
            "begin-run",
            "--nickname",
            config.nickname.as_str(),
            "--run-id",
            run_id.as_str(),
        ],
    )?;
    let begin_resp: BeginRunResponse =
        toml::from_str(&begin_out).with_context(|| "failed to parse begin-run response")?;

    // Validate begin-run response envelope
    if begin_resp.protocol_version != 1 {
        anyhow::bail!(
            "unsupported begin-run protocol version: {}",
            begin_resp.protocol_version
        );
    }
    if begin_resp.nickname != config.nickname.as_str() {
        anyhow::bail!(
            "begin-run response nickname '{}' does not match config nickname '{}'",
            begin_resp.nickname,
            config.nickname.as_str()
        );
    }
    if begin_resp.run_id != run_id.as_str() {
        anyhow::bail!(
            "begin-run response run_id '{}' does not match generated run_id '{}'",
            begin_resp.run_id,
            run_id.as_str()
        );
    }
    let incoming_path = Utf8Path::new(&begin_resp.incoming_dir);
    if !incoming_path.is_absolute() {
        anyhow::bail!(
            "begin-run response incoming_dir is not absolute: {}",
            begin_resp.incoming_dir
        );
    }
    let files_path = Utf8Path::new(&begin_resp.files_dir);
    if !files_path.is_absolute() {
        anyhow::bail!(
            "begin-run response files_dir is not absolute: {}",
            begin_resp.files_dir
        );
    }
    if !files_path.starts_with(incoming_path) {
        anyhow::bail!(
            "begin-run response files_dir '{}' is not under incoming_dir '{}'",
            begin_resp.files_dir,
            begin_resp.incoming_dir
        );
    }
    let run_config_path = Utf8Path::new(&begin_resp.run_config_path);
    if !run_config_path.is_absolute() {
        anyhow::bail!(
            "begin-run response run_config_path is not absolute: {}",
            begin_resp.run_config_path
        );
    }
    if !run_config_path.starts_with(incoming_path) {
        anyhow::bail!(
            "begin-run response run_config_path '{}' is not under incoming_dir '{}'",
            begin_resp.run_config_path,
            begin_resp.incoming_dir
        );
    }
    let manifest_path = Utf8Path::new(&begin_resp.manifest_path);
    if !manifest_path.is_absolute() {
        anyhow::bail!(
            "begin-run response manifest_path is not absolute: {}",
            begin_resp.manifest_path
        );
    }
    if !manifest_path.starts_with(incoming_path) {
        anyhow::bail!(
            "begin-run response manifest_path '{}' is not under incoming_dir '{}'",
            begin_resp.manifest_path,
            begin_resp.incoming_dir
        );
    }

    debug!(incoming_dir = %begin_resp.incoming_dir, "begin-run accepted");

    // 3. Write run.toml and manifest.toml to server
    let run_config = build_run_config(config);
    let run_config_toml = run_config
        .to_toml()
        .with_context(|| "failed to serialize run config")?;
    write_remote_file(host, &begin_resp.run_config_path, &run_config_toml)?;
    let manifest_toml = manifest.to_toml()?;
    write_remote_file(host, &begin_resp.manifest_path, &manifest_toml)?;

    // 4. Start heartbeat guard thread for periodic heartbeats during long rsync
    let heartbeat_interval = Duration::from_secs(begin_resp.heartbeat_interval_secs);
    let stop_hb = Arc::new(AtomicBool::new(false));
    let hb_error = Arc::new(Mutex::new(None::<String>));
    let stop_hb_clone = stop_hb.clone();
    let hb_error_clone = hb_error.clone();
    let hb_host = host.to_owned();
    let hb_cmd = server_command.to_owned();
    let hb_nick = config.nickname.as_str().to_owned();
    let hb_rid = run_id.as_str().to_owned();

    let hb_handle = thread::spawn(move || loop {
        if stop_hb_clone.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(heartbeat_interval);
        if stop_hb_clone.load(Ordering::Relaxed) {
            break;
        }
        if let Err(e) = server_cmd(
            &hb_host,
            &hb_cmd,
            &["heartbeat-run", "--nickname", &hb_nick, "--run-id", &hb_rid],
        ) {
            let mut err = hb_error_clone.lock().unwrap();
            *err = Some(format!("heartbeat failed: {e:#}"));
            break;
        }
    });

    // 5. Rsync files per sync mapping — split into passthrough and purgatory calls
    let sync_result = (|| -> Result<()> {
        for sync in &config.sync {
            let sync_name = sync.name.as_str();
            let from_path = sync.from_path.as_str();
            let to_path = sync.to_path.as_str();

            // Collect match patterns for this sync group
            let match_patterns: Vec<String> = config
                .postprocess
                .rules
                .iter()
                .map(|r| r.pattern.clone())
                .collect();

            let purgatory_filter = purgery_core::purgatory_filter_content(&match_patterns);
            let passthrough_filter = purgery_core::passthrough_filter_content(&match_patterns);

            // Write filter files
            let tmp_dir = std::env::temp_dir().join("purgery-filters");
            fs::create_dir_all(&tmp_dir).ok();
            let purgatory_file = tmp_dir.join(format!("purgatory-{sync_name}"));
            let passthrough_file = tmp_dir.join(format!("passthrough-{sync_name}"));
            fs::write(&purgatory_file, &purgatory_filter)
                .with_context(|| "failed to write purgatory filter")?;
            fs::write(&passthrough_file, &passthrough_filter)
                .with_context(|| "failed to write passthrough filter")?;

            // Purgatory rsync: transfer selected entries to staging
            let remote_staging_dir = format!("{}/{to_path}/", begin_resp.files_dir);
            info!(
                sync = sync_name,
                from = from_path,
                to = to_path,
                mode = "purgatory",
                "upload started"
            );
            let staging_dest = format!("{}:{}", host, remote_staging_dir);
            let mut args = build_rsync_args(from_path, &staging_dest);
            // Insert filter args before source/destination
            let filter_arg = format!("--include-from={}", purgatory_file.to_string_lossy());
            args.insert(5, filter_arg);
            // Remove the --protect-args that build_rsync_args adds, or keep both
            let status = std::process::Command::new("rsync")
                .args(&args)
                .status()
                .with_context(|| format!("failed to execute purgatory rsync for {from_path}"))?;
            if !status.success() {
                anyhow::bail!("purgatory rsync failed for sync mapping '{sync_name}'");
            }
            info!(sync = sync_name, mode = "purgatory", "upload finished");

            // Check for heartbeat failure after each mapping
            if let Some(err) = hb_error.lock().unwrap().take() {
                anyhow::bail!("{err}");
            }

            // Passthrough rsync: transfer non-selected entries to staging
            // NOTE: In a full implementation, the server would provide a
            // passthrough destination path. For now, upload to the incoming
            // dir with a passthrough filter applied, keeping the existing
            // architecture.
            // But this requires knowing the server root path, which the client
            // should not construct. Instead, we use a scheme where passthrough
            // entries are uploaded to the server's final destination directly
            // via SSH. For now, we upload everything to incoming directory
            // (current behavior) but with a passthrough filter applied.
            //
            // NOTE: In a full implementation, the server would provide a
            // passthrough destination path in the begin-run response.
            // For now, we simulate by uploading to the incoming dir with
            // the passthrough filter, keeping the existing architecture.
            let remote_files_dir = format!("{}/{to_path}/", begin_resp.files_dir);
            info!(
                sync = sync_name,
                from = from_path,
                to = to_path,
                mode = "passthrough",
                "upload started"
            );
            let rsync_dest = format!("{}:{}", host, remote_files_dir);
            let mut args = build_rsync_args(from_path, &rsync_dest);
            let filter_arg = format!("--exclude-from={}", purgatory_file.to_string_lossy());
            args.insert(5, filter_arg);
            let status = std::process::Command::new("rsync")
                .args(&args)
                .status()
                .with_context(|| format!("failed to execute passthrough rsync for {from_path}"))?;
            if !status.success() {
                anyhow::bail!("passthrough rsync failed for sync mapping '{sync_name}'");
            }
            info!(sync = sync_name, mode = "passthrough", "upload finished");

            // Early cleanup: delete passthrough regular files after successful transfer
            if sync.delete_after_import {
                // The manifest entries for this sync that have no postprocess_steps
                // are passthrough. We can clean them up immediately.
                let passthrough_entries: Vec<&ManifestEntry> = manifest
                    .entries
                    .iter()
                    .filter(|e| {
                        e.sync_name.as_str() == sync_name
                            && e.kind == ManifestEntryKind::RegularFile
                            && e.postprocess_steps.is_none()
                    })
                    .collect();
                let mut early_count = 0usize;
                for entry in &passthrough_entries {
                    let local_path = Path::new(entry.local_path.as_str());
                    // Check identity via symlink_metadata (no symlink replacements)
                    let symmeta = match fs::symlink_metadata(local_path) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
                        continue;
                    }
                    if let Ok(meta) = fs::metadata(local_path) {
                        if meta.len() == entry.size {
                            if let Err(e) = fs::remove_file(local_path) {
                                warn!(path = %local_path.display(), error = %e, "failed to delete passthrough file");
                            } else {
                                early_count += 1;
                            }
                        }
                    }
                }
                if early_count > 0 {
                    info!(
                        sync = sync_name,
                        deleted = early_count,
                        "passthrough early cleanup"
                    );
                }
            }

            // Check for heartbeat failure
            if let Some(err) = hb_error.lock().unwrap().take() {
                anyhow::bail!("{err}");
            }
        }

        // 6. Finish run: move from incoming to ready.
        // Check heartbeat right before the call — no output or logic between
        // the check and the command, so there is no window for finish-run to
        // proceed after the lease has already been lost.
        if let Some(err) = hb_error.lock().unwrap().take() {
            anyhow::bail!("{err}");
        }

        info!("finishing run");
        server_cmd(
            host,
            server_command,
            &[
                "finish-run",
                "--nickname",
                config.nickname.as_str(),
                "--run-id",
                run_id.as_str(),
            ],
        )?;
        info!("finish-run accepted");
        Ok(())
    })();

    // After finish-run succeeds, the run is no longer incoming.
    // A concurrent final heartbeat from the background thread may fail
    // harmlessly because the incoming directory no longer exists.
    // The heartbeat thread records failures in hb_error before exiting.
    // The sync result was checked immediately before finish-run, and
    // post-finish heartbeat errors are intentionally ignored.

    // Stop heartbeat thread regardless of sync/finish outcome
    stop_hb.store(true, Ordering::Relaxed);
    let _ = hb_handle.join();

    // Propagate sync/finish/heartbeat error first
    sync_result?;

    // 6. Poll for status via server command
    let status = poll_for_status(host, server_command, &config.nickname, &run_id)?;

    // 8. Verify status envelope before deletion
    if status.nickname != manifest.nickname {
        anyhow::bail!(
            "status nickname '{}' does not match manifest nickname '{}'; aborting deletion",
            status.nickname.as_str(),
            manifest.nickname.as_str()
        );
    }
    if status.run_id != manifest.run_id {
        anyhow::bail!(
            "status run_id '{}' does not match manifest run_id '{}'; aborting deletion",
            status.run_id.as_str(),
            manifest.run_id.as_str()
        );
    }

    // 9. Delete confirmed local files
    let deletion_count = delete_confirmed_files(config, &manifest, &status)?;
    info!(deleted = deletion_count, "cleanup complete");

    info!(state = %status.state.as_str(), "run finished");

    Ok(())
}

/// Walk all sync directories and build the manifest.
fn build_manifest(config: &ClientConfig, run_id: &RunId) -> Result<Manifest> {
    let mut entries = Vec::new();
    let nickname = config.nickname.clone();

    for sync in &config.sync {
        let from_path = sync.from_path.as_str();
        let to_path = sync.to_path.as_str();
        let from = Path::new(from_path);

        if !from.exists() {
            warn!(path = from_path, "sync path does not exist, skipping");
            continue;
        }

        for entry in WalkDir::new(from).follow_links(false).min_depth(1) {
            let entry = entry.with_context(|| format!("error walking {from_path}"))?;
            let path = entry.path();
            let relative = path.strip_prefix(from).with_context(|| {
                format!("failed to compute relative path for: {}", path.display())
            })?;
            let relative_path = camino::Utf8PathBuf::from_path_buf(relative.to_path_buf())
                .map_err(|path| {
                    anyhow::anyhow!("non-UTF-8 relative path is unsupported: {}", path.display())
                })?;
            let staged_path = camino::Utf8Path::new("files")
                .join(to_path)
                .join(&relative_path);
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("failed to read metadata: {}", path.display()))?;
            let file_type = metadata.file_type();

            let (kind, size, mtime_ns, sha256, link_target) = if file_type.is_dir() {
                (purgery_core::ManifestEntryKind::Directory, 0, 0, None, None)
            } else if file_type.is_file() {
                let mtime_ns = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos() as i64)
                    .unwrap_or(0);
                (
                    purgery_core::ManifestEntryKind::RegularFile,
                    metadata.len(),
                    mtime_ns,
                    compute_sha256(path).ok(),
                    None,
                )
            } else if file_type.is_symlink() {
                let target = fs::read_link(path)
                    .with_context(|| format!("failed to read symlink: {}", path.display()))?;
                let target = camino::Utf8PathBuf::from_path_buf(target).map_err(|path| {
                    anyhow::anyhow!(
                        "non-UTF-8 symlink target is unsupported: {}",
                        path.display()
                    )
                })?;
                (
                    purgery_core::ManifestEntryKind::Symlink,
                    0,
                    0,
                    None,
                    Some(target),
                )
            } else {
                anyhow::bail!("unsupported filesystem object: {}", path.display());
            };

            // Classify entry as passthrough or postprocessed
            let normalized_path = format!("{}/{}", to_path, relative_path.as_str());
            let postprocess_steps: Option<Vec<String>> = config
                .postprocess
                .rules
                .iter()
                .find(|r| purgery_core::rsync_pattern_match(&r.pattern, &normalized_path))
                .map(|r| r.steps.clone())
                .filter(|steps| !steps.is_empty());

            entries.push(ManifestEntry {
                sync_name: sync.name.clone(),
                local_path: ClientLocalPath::new(path.to_string_lossy().to_string())
                    .with_context(|| format!("invalid local path for: {}", path.display()))?,
                staged_path: NormalizedRelativePath::new(staged_path)
                    .with_context(|| format!("invalid staged path for: {}", path.display()))?,
                relative_path: NormalizedRelativePath::new(relative_path)
                    .with_context(|| format!("invalid relative path for: {}", path.display()))?,
                kind,
                size,
                mtime_ns,
                sha256,
                link_target,
                postprocess_steps,
            });
        }
    }

    entries.sort_by(|left, right| {
        let left_depth = left.relative_path.as_path().components().count();
        let right_depth = right.relative_path.as_path().components().count();
        let kind_order = |kind| match kind {
            purgery_core::ManifestEntryKind::Directory => 0,
            purgery_core::ManifestEntryKind::RegularFile
            | purgery_core::ManifestEntryKind::Symlink => 1,
        };
        left_depth
            .cmp(&right_depth)
            .then_with(|| kind_order(left.kind).cmp(&kind_order(right.kind)))
            .then_with(|| left.sync_name.as_str().cmp(right.sync_name.as_str()))
            .then_with(|| {
                left.relative_path
                    .as_str()
                    .cmp(right.relative_path.as_str())
            })
    });

    if entries.is_empty() {
        anyhow::bail!("no filesystem entries found to sync");
    }

    Ok(Manifest {
        run_id: run_id.clone(),
        nickname,
        entries,
    })
}

/// Compute SHA-256 of a file.
fn compute_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open file for SHA-256: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read file for SHA-256: {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Poll for status via the server's status command.
fn poll_for_status(
    host: &str,
    server_command: &str,
    nickname: &purgery_core::Nickname,
    run_id: &RunId,
) -> Result<RunStatus> {
    let max_attempts = 60;
    let poll_interval = Duration::from_secs(2);

    for attempt in 1..=max_attempts {
        let output = server_cmd(
            host,
            server_command,
            &[
                "status",
                "--nickname",
                nickname.as_str(),
                "--run-id",
                run_id.as_str(),
            ],
        );

        if let Ok(content) = output {
            if !content.trim().is_empty() {
                let status = RunStatus::from_toml(content.trim())
                    .with_context(|| "failed to parse status from server")?;
                return Ok(status);
            }
        }

        if attempt % 10 == 0 {
            info!(
                attempt,
                max = max_attempts,
                "waiting for server to process run"
            );
        }

        std::thread::sleep(poll_interval);
    }

    anyhow::bail!("timed out waiting for server to process run (checked {max_attempts} times)");
}

/// Delete local files that are confirmed imported and still match their uploaded identity.
fn delete_confirmed_files(
    config: &ClientConfig,
    manifest: &Manifest,
    status: &RunStatus,
) -> Result<usize> {
    let mut count = 0;

    // Build a lookup from local_path to manifest entry
    let manifest_by_path: std::collections::HashMap<&str, &ManifestEntry> = manifest
        .entries
        .iter()
        .map(|f| (f.local_path.as_str(), f))
        .collect();

    for entry_status in &status.entries {
        // Only delete files with status "imported"
        if entry_status.status != purgery_core::FileStatus::Imported {
            continue;
        }

        // Find the corresponding manifest entry
        let Some(manifest_entry) = manifest_by_path.get(entry_status.local_path.as_str()) else {
            warn!(
                local_path = %entry_status.local_path,
                "status references unknown local path"
            );
            continue;
        };

        if manifest_entry.kind != purgery_core::ManifestEntryKind::RegularFile {
            continue;
        }

        // Find the corresponding sync mapping
        let Some(sync) = config
            .sync
            .iter()
            .find(|s| s.name.as_str() == manifest_entry.sync_name.as_str())
        else {
            warn!(
                sync_name = %manifest_entry.sync_name.as_str(),
                "no sync mapping for file"
            );
            continue;
        };

        // Only delete if the sync mapping allows it
        if !sync.delete_after_import {
            continue;
        }

        let local_path_str = manifest_entry.local_path.as_str();
        let local_path = Path::new(local_path_str);

        // Use symlink_metadata to detect post-upload symlink replacements.
        let symmeta = match fs::symlink_metadata(local_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // File already gone — idempotent, count as success
                count += 1;
                continue;
            }
            Err(_) => {
                warn!(path = %local_path.display(), "cannot read metadata, not deleting");
                continue;
            }
        };

        // Refuse to delete if the current path is not a regular file.
        // This prevents cleanup from deleting a symlink that replaced the
        // original regular file after upload.
        if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
            warn!(
                path = %local_path.display(),
                "local path is no longer a regular file, not deleting"
            );
            continue;
        }

        if let Ok(metadata) = fs::metadata(local_path) {
            let current_size = metadata.len();
            let current_mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);

            let matches_size = current_size == manifest_entry.size;
            let matches_mtime = current_mtime == manifest_entry.mtime_ns;
            let matches_sha = if let Some(ref expected_sha) = manifest_entry.sha256 {
                compute_sha256(local_path).ok().as_deref() == Some(expected_sha)
            } else {
                true // SHA not available, skip check
            };

            if !matches_size || !matches_mtime || !matches_sha {
                warn!(
                    path = %local_path.display(),
                    "file changed since upload, not deleting"
                );
                continue;
            }
        } else {
            warn!(
                path = %local_path.display(),
                "cannot read metadata, not deleting"
            );
            continue;
        }

        // Safe to delete
        if let Err(e) = fs::remove_file(local_path) {
            warn!(path = %local_path.display(), error = %e, "failed to delete file");
        } else {
            count += 1;
        }
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use purgery_core::{EntryStatusEntry, FileStatus, ManifestEntryKind, RunState};

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
        let config = config_for(&source);
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

        let config = config_for(&source);
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
}
