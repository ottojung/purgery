use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand};
use purgery_core::{
    build_rsync_args, resolve_executable, shell_escape, BeginRunResponse, ClientConfig,
    ClientLocalPath, Manifest, ManifestEntry, ManifestEntryKind, NormalizedRelativePath, RunConfig,
    RunConfigSync, RunId, RunStatus, SyncExecutionClass,
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

/// Run the server command over SSH with stdin content.
fn server_cmd_with_stdin(
    host: &str,
    server_command: &str,
    args: &[&str],
    stdin_content: &str,
) -> Result<String> {
    let full_cmd = {
        let mut cmd = server_command.to_owned();
        for a in args {
            cmd.push(' ');
            cmd.push_str(&shell_escape(a));
        }
        cmd
    };
    let mut child = std::process::Command::new("ssh")
        .arg(host)
        .arg(&full_cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn SSH command to {host}"))?;

    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_content.as_bytes())
            .with_context(|| "failed to write stdin content")?;
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for SSH command on {host}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("SSH command on {host} failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
/// Build a RunConfig from ClientConfig, optionally filtering to include
/// only purgatory sync groups and rules applicable to those groups.
fn build_run_config(config: &ClientConfig, purgatory_only: bool) -> RunConfig {
    let sync: Vec<RunConfigSync> = config
        .sync
        .iter()
        .filter(|s| {
            if purgatory_only {
                let applicable =
                    purgery_core::applicable_rules(&config.postprocess.rules, s.name.as_str());
                !applicable.is_empty()
            } else {
                true
            }
        })
        .map(|s| RunConfigSync {
            name: s.name.clone(),
            to_path: s.to_path.clone(),
            delete_after_import: s.delete_after_import,
        })
        .collect();

    let purgatory_names: Vec<&str> = sync.iter().map(|s| s.name.as_str()).collect();
    let rules = if purgatory_only {
        config
            .postprocess
            .rules
            .iter()
            .filter(|r| {
                // Include rules that apply to at least one purgatory sync group
                purgatory_names.iter().any(|name| r.applies_to(name))
            })
            .cloned()
            .collect()
    } else {
        config.postprocess.rules.clone()
    };

    RunConfig {
        nickname: config.nickname.clone(),
        sync,
        postprocess: purgery_core::ClientPostprocessConfig { rules },
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

    let host = config.server.host.as_str();
    let server_command = &config.server.command;

    // Resume any pending cleanups from previous interrupted runs.
    // This runs before any new transfers so that partially-completed
    // cleanup does not accumulate.
    resume_pending_cleanups(config)?;

    // 1. Validate postprocess config before any walking
    let sync_names: Vec<purgery_core::SyncName> =
        config.sync.iter().map(|s| s.name.clone()).collect();
    if let Err(e) = config.postprocess.validate(&sync_names) {
        anyhow::bail!("postprocess config validation failed: {e}");
    }
    if let Err(e) = config
        .postprocess
        .validate_delete_after_import(&config.sync)
    {
        anyhow::bail!("postprocess config validation failed: {e}");
    }
    info!("postprocess rules validated");

    // 2. Classify sync groups by their execution class.
    let run_id = RunId::generate();
    let mut all_manifest_entries: Vec<purgery_core::ManifestEntry> = Vec::new();
    let mut passthrough_nodelete: Vec<&purgery_core::SyncMapping> = Vec::new();
    let mut passthrough_cleanup: Vec<&purgery_core::SyncMapping> = Vec::new();
    let mut purgatory_syncs: Vec<&purgery_core::SyncMapping> = Vec::new();

    let classes = purgery_core::classify_sync_groups(&config.sync, &config.postprocess.rules)
        .map_err(|e| anyhow::anyhow!("sync group classification failed: {e}"))?;
    for (class, sync) in &classes {
        match class {
            SyncExecutionClass::PassthroughNoDelete => {
                info!(
                    sync = sync.name.as_str(),
                    "PassthroughNoDelete: direct rsync only"
                );
                passthrough_nodelete.push(sync);
            }
            SyncExecutionClass::PassthroughDeleteAfterImport => {
                info!(
                    sync = sync.name.as_str(),
                    "PassthroughDeleteAfterImport: direct rsync with cleanup"
                );
                passthrough_cleanup.push(sync);
            }
            SyncExecutionClass::Purgatory => {
                let applicable =
                    purgery_core::applicable_rules(&config.postprocess.rules, sync.name.as_str());
                let (entries, _) = walk_and_classify_sync(config, sync, &run_id, &applicable)?;
                all_manifest_entries.extend(entries);
                purgatory_syncs.push(sync);
            }
        }
    }

    if all_manifest_entries.is_empty()
        && passthrough_nodelete.is_empty()
        && passthrough_cleanup.is_empty()
    {
        anyhow::bail!("no filesystem entries found to sync");
    }

    // Build the full manifest from walked entries
    let manifest = purgery_core::Manifest {
        run_id: run_id.clone(),
        nickname: config.nickname.clone(),
        entries: all_manifest_entries,
    };
    let transfer_plan = manifest.to_transfer_plan();
    info!(walked = manifest.entries.len(), "manifest built");

    // 3. Execute transfers based on whether any purgatory groups exist
    if !purgatory_syncs.is_empty() {
        run_postprocess_path(
            config,
            host,
            server_command,
            &manifest,
            &transfer_plan,
            &run_id,
            &passthrough_nodelete,
            &passthrough_cleanup,
        )
    } else {
        run_passthrough_path(
            config,
            host,
            server_command,
            &manifest,
            &transfer_plan,
            &passthrough_nodelete,
            &passthrough_cleanup,
        )
    }
}

/// Run a pure passthrough invocation (no postprocess entries in any sync group).
///
/// Every passthrough group uses direct unfiltered rsync.
/// PassthroughDeleteAfterImport groups additionally get a cleanup scan.
/// There is no per-entry filtered transfer loop — pure passthrough
/// does not use transfer_plan or transfer filters.
fn run_passthrough_path(
    config: &ClientConfig,
    host: &str,
    server_command: &str,
    _manifest: &Manifest,
    _transfer_plan: &[purgery_core::TransferPlanEntry],
    passthrough_nodelete: &[&purgery_core::SyncMapping],
    passthrough_cleanup: &[&purgery_core::SyncMapping],
) -> Result<()> {
    let _span = span!(
        Level::INFO,
        "client passthrough",
        nickname = %config.nickname.as_str()
    )
    .entered();

    info!("resolving destinations");
    let run_config_all = build_run_config(config, false);
    let run_config_all_toml = run_config_all
        .to_toml()
        .with_context(|| "failed to serialize run config")?;

    let resolve_out = server_cmd_with_stdin(
        host,
        server_command,
        &[
            "resolve-destinations",
            "--nickname",
            config.nickname.as_str(),
        ],
        &run_config_all_toml,
    )
    .context("resolve-destinations failed")?;
    let resolve_resp: purgery_core::ResolveDestinationsResponse =
        toml::from_str(&resolve_out).context("failed to parse resolve-destinations response")?;
    if resolve_resp.protocol_version != 1 {
        anyhow::bail!(
            "unsupported resolve-destinations protocol version: {}",
            resolve_resp.protocol_version
        );
    }

    let dest_map: std::collections::HashMap<&str, &purgery_core::SyncPassthroughDestination> =
        resolve_resp
            .destinations
            .iter()
            .map(|d| (d.sync_name.as_str(), d))
            .collect();

    // Every passthrough group uses direct unfiltered rsync — no per-entry filters.
    // PassthroughDeleteAfterImport groups also get a cleanup scan/state after rsync.
    for sync in passthrough_nodelete {
        run_unfiltered_rsync(config, host, sync, &dest_map)?;
    }
    for sync in passthrough_cleanup {
        let sync_name = sync.name.as_str();
        if let Some(dest) = dest_map.get(sync_name) {
            let rsync_dest = format!("{}:{}/", host, dest.passthrough_dest);
            info!(
                sync = sync_name,
                dest = %dest.passthrough_dest,
                "passthrough direct rsync with cleanup"
            );
            let rsync_args = build_rsync_args(sync.from_path.as_str(), &rsync_dest);
            let status = std::process::Command::new("rsync")
                .args(&rsync_args)
                .status()
                .with_context(|| {
                    format!("failed to execute rsync for {}", sync.from_path.as_str())
                })?;
            if !status.success() {
                anyhow::bail!("rsync failed for sync mapping '{sync_name}'");
            }
            scan_and_cleanup_sync(config, sync)?;
        }
    }

    info!("passthrough run complete");
    Ok(())
}

/// Run unfiltered rsync for a direct-rsync-only sync group (no filter file needed).
fn run_unfiltered_rsync(
    _config: &ClientConfig,
    host: &str,
    sync: &purgery_core::SyncMapping,
    dest_map: &std::collections::HashMap<&str, &purgery_core::SyncPassthroughDestination>,
) -> Result<()> {
    let sync_name = sync.name.as_str();
    let from_path = sync.from_path.as_str();
    let dest = dest_map
        .get(sync_name)
        .ok_or_else(|| anyhow::anyhow!("no destination for sync mapping '{sync_name}'"))?;
    let rsync_dest = format!("{}:{}/", host, dest.passthrough_dest);
    info!(
        sync = sync_name,
        from = from_path,
        dest = %dest.passthrough_dest,
        "direct rsync (no postprocess rules)"
    );
    let rsync_args = build_rsync_args(from_path, &rsync_dest);
    // No filter needed for direct-rsync-only groups
    let status = std::process::Command::new("rsync")
        .args(&rsync_args)
        .status()
        .with_context(|| format!("failed to execute rsync for {from_path}"))?;
    if !status.success() {
        anyhow::bail!("rsync failed for sync mapping '{sync_name}'");
    }
    info!(sync = sync_name, "direct rsync complete");
    Ok(())
}

/// Run a postprocess invocation (one or more sync groups have postprocess roots).
///
/// Creates a server run: begin-run, upload filtered manifest, prepare-run,
/// rsync passthrough + purgatory, finish-run, poll status, cleanup from status.
///
/// Passthrough-only sync groups (no applicable rules) are handled outside
/// the purgatory run lifecycle via resolve-destinations + direct rsync.
#[allow(clippy::too_many_arguments)]
fn run_postprocess_path(
    config: &ClientConfig,
    host: &str,
    server_command: &str,
    manifest: &Manifest,
    transfer_plan: &[purgery_core::TransferPlanEntry],
    run_id: &RunId,
    passthrough_nodelete: &[&purgery_core::SyncMapping],
    passthrough_cleanup: &[&purgery_core::SyncMapping],
) -> Result<()> {
    let _span = span!(
        Level::INFO,
        "client run",
        nickname = %config.nickname.as_str(),
        run_id = %run_id.as_str()
    )
    .entered();

    // Build server manifest (postprocess/covered entries only)
    let server_manifest = manifest.build_server_manifest();
    info!(
        entries = manifest.entries.len(),
        server_entries = server_manifest.entries.len(),
        "built server manifest"
    );

    // 0a. Resolve destinations for passthrough-only groups via resolve-destinations
    let passthrough_dest_map: std::collections::HashMap<String, String> =
        if !passthrough_nodelete.is_empty() || !passthrough_cleanup.is_empty() {
            let run_config_all = build_run_config(config, false);
            let run_config_all_toml = run_config_all
                .to_toml()
                .with_context(|| "failed to serialize run config")?;
            let resolve_out = server_cmd_with_stdin(
                host,
                server_command,
                &[
                    "resolve-destinations",
                    "--nickname",
                    config.nickname.as_str(),
                ],
                &run_config_all_toml,
            )
            .context("resolve-destinations failed")?;
            let resolve_resp: purgery_core::ResolveDestinationsResponse =
                toml::from_str(&resolve_out)
                    .context("failed to parse resolve-destinations response")?;
            resolve_resp
                .destinations
                .into_iter()
                .map(|d| (d.sync_name, d.passthrough_dest))
                .collect()
        } else {
            std::collections::HashMap::new()
        };

    // 0b. Handle PassthroughNoDelete groups before the purgatory run.
    //     These groups are outside the purgatory lifecycle.
    for sync in passthrough_nodelete {
        let sync_name = sync.name.as_str();
        let from_path = sync.from_path.as_str();
        if let Some(passthrough_dest) = passthrough_dest_map.get(sync_name) {
            let rsync_dest = format!("{}:{}/", host, passthrough_dest);
            info!(
                sync = sync_name,
                from = from_path,
                dest = %passthrough_dest,
                "passthrough-only direct rsync (outside purgatory run)"
            );
            let rsync_args = build_rsync_args(from_path, &rsync_dest);
            let status = std::process::Command::new("rsync")
                .args(&rsync_args)
                .status()
                .with_context(|| format!("failed to execute rsync for {from_path}"))?;
            if !status.success() {
                anyhow::bail!("rsync failed for sync mapping '{sync_name}'");
            }
            info!(sync = sync_name, "direct rsync complete");
        } else {
            anyhow::bail!("no destination resolved for passthrough sync mapping '{sync_name}'");
        }
    }

    // 0c. Handle PassthroughDeleteAfterImport groups:
    //     direct unfiltered rsync + filesystem scan for cleanup.
    for sync in passthrough_cleanup {
        let sync_name = sync.name.as_str();
        if let Some(passthrough_dest) = passthrough_dest_map.get(sync_name) {
            let rsync_dest = format!("{}:{}/", host, passthrough_dest);
            info!(
                sync = sync_name,
                dest = %passthrough_dest,
                "cleanup direct rsync (no-rule delete-after-import)"
            );
            let rsync_args = build_rsync_args(sync.from_path.as_str(), &rsync_dest);
            let status = std::process::Command::new("rsync")
                .args(&rsync_args)
                .status()
                .with_context(|| {
                    format!("failed to execute rsync for {}", sync.from_path.as_str())
                })?;
            if !status.success() {
                anyhow::bail!("rsync failed for sync mapping '{sync_name}'");
            }
            scan_and_cleanup_sync(config, sync)?;
        }
    }

    // 0d. The purgatory-only run config excludes passthrough-only sync groups
    let run_config = build_run_config(config, true);
    let run_config_toml = run_config
        .to_toml()
        .with_context(|| "failed to serialize purgatory run config")?;

    // If no purgatory groups remain after filtering, we're done (all groups were passthrough).
    // This should not normally happen since run_postprocess_path is only called when
    // any_postprocess is true, but handle it gracefully.
    if run_config.sync.is_empty() {
        return Ok(());
    }

    // 1. Begin run
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

    // 2. Write purgatory-only run.toml and filtered manifest.toml to server
    write_remote_file(host, &begin_resp.run_config_path, &run_config_toml)?;
    let manifest_toml = server_manifest
        .to_toml()
        .with_context(|| "failed to serialize server manifest")?;
    write_remote_file(host, &begin_resp.manifest_path, &manifest_toml)?;

    // 3. Prepare-run
    info!("validating run plan");
    let prepare_out = server_cmd(
        host,
        server_command,
        &[
            "prepare-run",
            "--nickname",
            config.nickname.as_str(),
            "--run-id",
            run_id.as_str(),
        ],
    )
    .context("prepare-run failed")?;
    let prepare_resp: purgery_core::PrepareRunResponse =
        toml::from_str(&prepare_out).context("failed to parse prepare-run response")?;
    if prepare_resp.protocol_version != 1 {
        anyhow::bail!(
            "unsupported prepare-run protocol version: {}",
            prepare_resp.protocol_version
        );
    }

    let dest_map: std::collections::HashMap<&str, &purgery_core::SyncDestination> = prepare_resp
        .destinations
        .iter()
        .map(|d| (d.sync_name.as_str(), d))
        .collect();

    // 4. Start heartbeat guard thread
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

    // 5. Transfer per purgatory sync group
    let sync_map = config
        .sync
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect::<std::collections::HashMap<&str, &purgery_core::SyncMapping>>();
    let sync_result = (|| -> Result<()> {
        // Iterate only purgatory groups via the purgatory-only run config.
        for rcsync in &run_config.sync {
            let sync_name = rcsync.name.as_str();
            let Some(sync) = sync_map.get(sync_name) else {
                anyhow::bail!("sync mapping '{}' not found in client config", sync_name);
            };
            let from_path = sync.from_path.as_str();
            let dest = dest_map
                .get(sync_name)
                .ok_or_else(|| anyhow::anyhow!("no destination for sync mapping '{sync_name}'"))?;

            let passthrough_roots: Vec<purgery_core::TransferRoot> = transfer_plan
                .iter()
                .filter(|e| {
                    e.sync_name.as_str() == sync_name
                        && e.mode == purgery_core::ManifestEntryMode::Passthrough
                })
                .map(|e| purgery_core::TransferRoot::Exact(e.relative_path.as_str().to_owned()))
                .collect();
            let mut purgatory_roots: Vec<purgery_core::TransferRoot> = transfer_plan
                .iter()
                .filter(|e| {
                    e.sync_name.as_str() == sync_name
                        && e.mode == purgery_core::ManifestEntryMode::Postprocess
                })
                .map(|e| {
                    if e.kind == purgery_core::ManifestEntryKind::Directory {
                        purgery_core::TransferRoot::Subtree(e.relative_path.as_str().to_owned())
                    } else {
                        purgery_core::TransferRoot::Exact(e.relative_path.as_str().to_owned())
                    }
                })
                .collect();
            purgatory_roots.sort_by(|a, b| {
                let a_str = match a {
                    purgery_core::TransferRoot::Exact(p)
                    | purgery_core::TransferRoot::Subtree(p) => p.as_str(),
                };
                let b_str = match b {
                    purgery_core::TransferRoot::Exact(p)
                    | purgery_core::TransferRoot::Subtree(p) => p.as_str(),
                };
                a_str.cmp(b_str)
            });

            let tmp_dir = std::env::temp_dir().join("purgery-filters");
            fs::create_dir_all(&tmp_dir).ok();
            let passthrough_file = tmp_dir.join(format!("passthrough-{sync_name}"));
            let purgatory_file = tmp_dir.join(format!("purgatory-{sync_name}"));

            // Passthrough rsync (non-postprocess entries)
            if !passthrough_roots.is_empty() {
                let passthrough_filter = purgery_core::transfer_set_filter(&passthrough_roots);
                fs::write(&passthrough_file, &passthrough_filter)
                    .with_context(|| "failed to write passthrough filter")?;

                let passthrough_rsync_dest = format!("{}:{}/", host, dest.passthrough_dest);
                info!(
                    sync = sync_name,
                    from = from_path,
                    dest = %dest.passthrough_dest,
                    mode = "passthrough",
                    "passthrough rsync started"
                );
                let mut pt_args = build_rsync_args(from_path, &passthrough_rsync_dest);
                let pt_filter_arg =
                    format!("--filter=merge {}", passthrough_file.to_string_lossy());
                pt_args.insert(5, pt_filter_arg);
                let pt_status = std::process::Command::new("rsync")
                    .args(&pt_args)
                    .status()
                    .with_context(|| {
                        format!("failed to execute passthrough rsync for {from_path}")
                    })?;
                if !pt_status.success() {
                    anyhow::bail!("passthrough rsync failed for sync mapping '{sync_name}'");
                }
                info!(sync = sync_name, mode = "passthrough", "rsync complete");
            } else {
                info!(
                    sync = sync_name,
                    mode = "passthrough",
                    "no passthrough roots, skipping rsync"
                );
            }

            // Durable cleanup state for delete_after_import=true passthrough regular files
            if sync.delete_after_import {
                process_cleanup_entries(config, sync_name, manifest)?;
            }

            // Check heartbeat
            if let Some(err) = hb_error.lock().unwrap().take() {
                anyhow::bail!("{err}");
            }

            // Purgatory rsync (postprocess entries)
            if !purgatory_roots.is_empty() {
                let purgatory_filter = purgery_core::transfer_set_filter(&purgatory_roots);
                fs::write(&purgatory_file, &purgatory_filter)
                    .with_context(|| "failed to write purgatory filter")?;

                let purgatory_rsync_dest = format!("{}:{}/", host, dest.purgatory_dest);
                info!(
                    sync = sync_name,
                    from = from_path,
                    dest = %dest.purgatory_dest,
                    mode = "purgatory",
                    "purgatory rsync started"
                );
                let mut pg_args = build_rsync_args(from_path, &purgatory_rsync_dest);
                let pg_filter_arg = format!("--filter=merge {}", purgatory_file.to_string_lossy());
                pg_args.insert(5, pg_filter_arg);
                let pg_status = std::process::Command::new("rsync")
                    .args(&pg_args)
                    .status()
                    .with_context(|| {
                        format!("failed to execute purgatory rsync for {from_path}")
                    })?;
                if !pg_status.success() {
                    anyhow::bail!("purgatory rsync failed for sync mapping '{sync_name}'");
                }
                info!(sync = sync_name, mode = "purgatory", "rsync complete");
            } else {
                info!(
                    sync = sync_name,
                    mode = "purgatory",
                    "no purgatory roots, skipping rsync"
                );
            }

            // Check heartbeat
            if let Some(err) = hb_error.lock().unwrap().take() {
                anyhow::bail!("{err}");
            }
        }

        // 6. Finish run
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

    stop_hb.store(true, Ordering::Relaxed);
    let _ = hb_handle.join();
    sync_result?;

    // 7. Poll for status (contains postprocess/covered entries only)
    let status = poll_for_status(host, server_command, &config.nickname, run_id)?;

    // 8. Verify status envelope
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

    // 9. Delete confirmed postprocess files from status
    let deletion_count = delete_confirmed_files(config, manifest, &status)?;
    info!(deleted = deletion_count, "cleanup complete");

    info!(state = %status.state.as_str(), "run finished");

    Ok(())
}

/// Walk one sync group and return manifest entries plus whether any are postprocess.
/// Uses only the provided applicable rules for classification.
fn walk_and_classify_sync(
    _config: &ClientConfig,
    sync: &purgery_core::SyncMapping,
    _run_id: &RunId,
    applicable_rules: &[&purgery_core::PostprocessRule],
) -> Result<(Vec<purgery_core::ManifestEntry>, bool)> {
    let from_path = sync.from_path.as_str();
    let to_path = sync.to_path.as_str();
    let from = Path::new(from_path);

    if !from.exists() {
        warn!(path = from_path, "sync path does not exist, skipping");
        return Ok((Vec::new(), false));
    }

    let mut entries = Vec::new();
    let mut has_postprocess = false;

    for walk_entry in WalkDir::new(from).follow_links(false).min_depth(1) {
        let walk_entry = walk_entry.with_context(|| format!("error walking {from_path}"))?;
        let path = walk_entry.path();
        let relative = path
            .strip_prefix(from)
            .with_context(|| format!("failed to compute relative path for: {}", path.display()))?;
        let relative_path =
            camino::Utf8PathBuf::from_path_buf(relative.to_path_buf()).map_err(|path| {
                anyhow::anyhow!("non-UTF-8 relative path is unsupported: {}", path.display())
            })?;
        let staged_path = camino::Utf8Path::new("files")
            .join(to_path)
            .join(&relative_path);
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to read metadata: {}", path.display()))?;
        let file_type = metadata.file_type();
        let normalized_path = relative_path.as_str().to_owned();

        // Classify using only applicable rules for this sync
        let matched_rule = applicable_rules
            .iter()
            .find(|r| purgery_core::rsync_pattern_match(&r.pattern, &normalized_path));
        let mode = if matched_rule.is_some() {
            has_postprocess = true;
            purgery_core::ManifestEntryMode::Postprocess
        } else {
            purgery_core::ManifestEntryMode::Passthrough
        };
        let postprocess_steps: Vec<String> =
            matched_rule.map(|r| r.steps.clone()).unwrap_or_default();
        let needs_bookkeeping = matched_rule.is_some() || sync.delete_after_import;

        let (kind, size, mtime_ns, sha256, link_target) = if file_type.is_dir() {
            (purgery_core::ManifestEntryKind::Directory, 0, 0, None, None)
        } else if file_type.is_file() {
            let (mtime_ns, sha256) = if needs_bookkeeping {
                let mtime_ns = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos() as i64)
                    .unwrap_or(0);
                (mtime_ns, compute_sha256(path).ok())
            } else {
                (0, None)
            };
            (
                purgery_core::ManifestEntryKind::RegularFile,
                metadata.len(),
                mtime_ns,
                sha256,
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

        entries.push(purgery_core::ManifestEntry {
            sync_name: sync.name.clone(),
            local_path: purgery_core::ClientLocalPath::new(path.to_string_lossy().to_string())
                .with_context(|| format!("invalid local path for: {}", path.display()))?,
            staged_path: purgery_core::NormalizedRelativePath::new(staged_path)
                .with_context(|| format!("invalid staged path for: {}", path.display()))?,
            relative_path: purgery_core::NormalizedRelativePath::new(relative_path)
                .with_context(|| format!("invalid relative path for: {}", path.display()))?,
            kind,
            size,
            mtime_ns,
            sha256,
            link_target,
            mode,
            postprocess_steps,
            covered_by: None,
        });
    }

    // Identify covered entries under postprocessed directories.
    let covering_dirs: Vec<String> = entries
        .iter()
        .filter(|e| {
            e.kind == purgery_core::ManifestEntryKind::Directory
                && e.mode == purgery_core::ManifestEntryMode::Postprocess
        })
        .map(|e| e.relative_path.as_str().to_owned())
        .collect();
    for entry in entries.iter_mut() {
        let rp = entry.relative_path.as_str();
        for dir_path in &covering_dirs {
            if rp == dir_path.as_str() {
                continue;
            }
            if rp.starts_with(dir_path.as_str()) && rp.as_bytes().get(dir_path.len()) == Some(&b'/')
            {
                entry.mode = purgery_core::ManifestEntryMode::Covered;
                entry.covered_by = Some(dir_path.clone());
                entry.postprocess_steps = Vec::new();
                break;
            }
        }
    }

    // Sort: directories first, then by depth, then by name
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

    Ok((entries, has_postprocess))
}

/// Walk all sync directories and build the manifest.
/// Used in tests; kept for backward compat but production code uses
/// walk_and_classify_sync per sync group.
#[allow(dead_code)]
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

            // Classify entry as passthrough or postprocessed before computing identity.
            let normalized_path = relative_path.as_str().to_owned();
            let matched_rule = config
                .postprocess
                .rules
                .iter()
                .find(|r| purgery_core::rsync_pattern_match(&r.pattern, &normalized_path));
            let mode = if matched_rule.is_some() {
                purgery_core::ManifestEntryMode::Postprocess
            } else {
                purgery_core::ManifestEntryMode::Passthrough
            };
            let postprocess_steps: Vec<String> =
                matched_rule.map(|r| r.steps.clone()).unwrap_or_default();

            // Identity bookkeeping is needed for postprocess entries (server verification)
            // and for passthrough entries with delete_after_import=true (local cleanup).
            let needs_bookkeeping = matched_rule.is_some() || sync.delete_after_import;

            let (kind, size, mtime_ns, sha256, link_target) = if file_type.is_dir() {
                (purgery_core::ManifestEntryKind::Directory, 0, 0, None, None)
            } else if file_type.is_file() {
                let (mtime_ns, sha256) = if needs_bookkeeping {
                    let mtime_ns = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_nanos() as i64)
                        .unwrap_or(0);
                    (mtime_ns, compute_sha256(path).ok())
                } else {
                    (0, None)
                };
                (
                    purgery_core::ManifestEntryKind::RegularFile,
                    metadata.len(),
                    mtime_ns,
                    sha256,
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
                mode,
                postprocess_steps,
                covered_by: None,
            });
        }
    }

    // Second pass: identify covered entries under postprocessed directories.
    let covering_dirs: Vec<String> = entries
        .iter()
        .filter(|e| {
            e.kind == purgery_core::ManifestEntryKind::Directory
                && e.mode == purgery_core::ManifestEntryMode::Postprocess
        })
        .map(|e| e.relative_path.as_str().to_owned())
        .collect();
    for entry in entries.iter_mut() {
        let rp = entry.relative_path.as_str();
        for dir_path in &covering_dirs {
            if rp == dir_path.as_str() {
                continue;
            }
            if rp.starts_with(dir_path.as_str()) && rp.as_bytes().get(dir_path.len()) == Some(&b'/')
            {
                entry.mode = purgery_core::ManifestEntryMode::Covered;
                entry.covered_by = Some(dir_path.clone());
                entry.postprocess_steps = Vec::new();
                break;
            }
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

/// Return the stable client state directory for cleanup bookkeeping.
fn cleanup_state_dir() -> Result<Utf8PathBuf> {
    // Prefer XDG_STATE_HOME, fall back to ~/.local/state/purgery/
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        if !dir.is_empty() {
            let path = Utf8PathBuf::from(dir).join("purgery");
            fs::create_dir_all(path.as_std_path())
                .with_context(|| format!("failed to create state dir: {path}"))?;
            return Ok(path);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let path = Utf8PathBuf::from(format!("{home}/.local/state/purgery"));
    fs::create_dir_all(path.as_std_path())
        .with_context(|| format!("failed to create state dir: {path}"))?;
    Ok(path)
}

/// Write cleanup state atomically to a file in the cleanup state directory.
fn write_cleanup_state(state: &purgery_core::DurableCleanupState) -> Result<Utf8PathBuf> {
    let dir = cleanup_state_dir()?;
    let filename = format!("cleanup-{}-{}.toml", state.nickname, state.operation_id);
    let final_path = dir.join(&filename);
    let tmp_path = dir.join(format!("{filename}.tmp"));
    let content = toml::to_string(state)
        .map_err(|e| anyhow::anyhow!("failed to serialize cleanup state: {e}"))?;
    fs::write(&tmp_path, &content)
        .with_context(|| format!("failed to write cleanup state: {tmp_path}"))?;
    fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("failed to atomically publish cleanup state: {final_path}"))?;
    Ok(final_path)
}

/// Mark one entry as cleaned in the cleanup state file, atomically.
fn mark_cleaned(state_path: &Utf8Path, sync_name: &str, local_path: &str) -> Result<()> {
    let content = fs::read_to_string(state_path.as_std_path())
        .with_context(|| format!("failed to read cleanup state: {state_path}"))?;
    let mut state: purgery_core::DurableCleanupState = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse cleanup state: {e}"))?;
    for entry in &mut state.entries {
        if entry.sync_name == sync_name && entry.local_path == local_path {
            entry.cleaned = true;
        }
    }
    let tmp_path = state_path.with_extension("toml.tmp");
    let new_content = toml::to_string(&state)
        .map_err(|e| anyhow::anyhow!("failed to serialize cleanup state: {e}"))?;
    fs::write(&tmp_path, &new_content)
        .with_context(|| format!("failed to write cleanup state: {tmp_path}"))?;
    fs::rename(&tmp_path, state_path)
        .with_context(|| format!("failed to atomically update cleanup state: {state_path}"))?;
    Ok(())
}

/// Scan for pending cleanup state files and process them.
/// Called at the start of sync_and_cleanup before any new transfers.
#[allow(dead_code)]
fn resume_pending_cleanups(_config: &ClientConfig) -> Result<()> {
    let dir = match cleanup_state_dir() {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    if !dir.exists() {
        return Ok(());
    }
    let mut deleted_total = 0usize;
    for entry in fs::read_dir(dir.as_std_path())
        .with_context(|| format!("failed to read state dir: {dir}"))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = toml::from_str::<purgery_core::DurableCleanupState>(&content) else {
            continue;
        };
        let state_path = match camino::Utf8PathBuf::from_path_buf(path) {
            Ok(p) => p,
            Err(_) => continue,
        };
        for entry in &state.entries {
            if !entry.rsync_succeeded || entry.cleaned {
                continue;
            }
            let local_path = Path::new(&entry.local_path);
            let symmeta = match fs::symlink_metadata(local_path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = mark_cleaned(&state_path, &entry.sync_name, &entry.local_path);
                    deleted_total += 1;
                    continue;
                }
                Err(_) => continue,
            };
            if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
                continue;
            }
            let Ok(meta) = fs::metadata(local_path) else {
                continue;
            };
            if meta.len() != entry.size {
                continue;
            }
            let current_mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            if current_mtime != entry.mtime_ns {
                continue;
            }
            if let Some(ref expected_sha) = entry.sha256 {
                if let Ok(actual_sha) = compute_sha256(local_path) {
                    if &actual_sha != expected_sha {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            if let Err(e) = fs::remove_file(local_path) {
                warn!(path = %entry.local_path, error = %e, "failed to delete");
            } else {
                let _ = mark_cleaned(&state_path, &entry.sync_name, &entry.local_path);
                deleted_total += 1;
            }
        }
    }
    if deleted_total > 0 {
        info!(deleted = deleted_total, "resumed pending cleanups");
    }
    Ok(())
}

/// Process cleanup entries: write durable state, delete verified files with atomic progress.
fn process_cleanup_entries(
    config: &ClientConfig,
    sync_name: &str,
    manifest: &Manifest,
) -> Result<()> {
    let cleanup_entries: Vec<purgery_core::CleanupEntry> = manifest
        .entries
        .iter()
        .filter(|e| {
            e.sync_name.as_str() == sync_name
                && e.kind == ManifestEntryKind::RegularFile
                && e.mode == purgery_core::ManifestEntryMode::Passthrough
        })
        .map(|e| purgery_core::CleanupEntry {
            sync_name: sync_name.to_owned(),
            relative_path: e.relative_path.as_str().to_owned(),
            local_path: e.local_path.as_str().to_owned(),
            size: e.size,
            mtime_ns: e.mtime_ns,
            sha256: e.sha256.clone(),
            rsync_succeeded: true,
            cleaned: false,
        })
        .collect();

    if cleanup_entries.is_empty() {
        return Ok(());
    }

    let cleanup_state = purgery_core::DurableCleanupState {
        nickname: config.nickname.as_str().to_owned(),
        operation_id: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string(),
        entries: cleanup_entries,
    };

    let state_path = write_cleanup_state(&cleanup_state)?;

    for entry in &cleanup_state.entries {
        let local_path = Path::new(&entry.local_path);
        let symmeta = match fs::symlink_metadata(local_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = mark_cleaned(&state_path, &entry.sync_name, &entry.local_path);
                continue;
            }
            Err(_) => continue,
        };
        if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
            continue;
        }
        let Ok(meta) = fs::metadata(local_path) else {
            continue;
        };
        if meta.len() != entry.size {
            continue;
        }
        let current_mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        if current_mtime != entry.mtime_ns {
            continue;
        }
        if let Some(ref expected_sha) = entry.sha256 {
            if let Ok(actual_sha) = compute_sha256(local_path) {
                if &actual_sha != expected_sha {
                    continue;
                }
            } else {
                continue;
            }
        }
        if let Err(e) = fs::remove_file(local_path) {
            warn!(path = %entry.local_path, error = %e, "failed to delete");
        } else {
            let _ = mark_cleaned(&state_path, &entry.sync_name, &entry.local_path);
        }
    }

    Ok(())
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

/// Scan a source directory after rsync and build cleanup entries for regular files,
/// then write durable cleanup state and delete verified files.
/// Used for PassthroughDeleteAfterImport groups after direct unfiltered rsync.
fn scan_and_cleanup_sync(config: &ClientConfig, sync: &purgery_core::SyncMapping) -> Result<()> {
    let from_path = sync.from_path.as_str();
    let from = Path::new(from_path);
    if !from.exists() {
        return Ok(());
    }

    let mut cleanup_entries = Vec::new();
    for entry in WalkDir::new(from).sort_by_file_name() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let local_path = entry.path();
        let metadata = match fs::symlink_metadata(local_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let relative = match local_path.strip_prefix(from) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let relative_str = relative.to_string_lossy().replace('\\', "/");

        let file_meta = match fs::metadata(local_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = file_meta.len();
        let mtime_ns = file_meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let sha256 = compute_sha256(local_path).ok();

        cleanup_entries.push(purgery_core::CleanupEntry {
            sync_name: sync.name.as_str().to_owned(),
            relative_path: relative_str,
            local_path: local_path.to_string_lossy().into_owned(),
            size,
            mtime_ns,
            sha256,
            rsync_succeeded: true,
            cleaned: false,
        });
    }

    if cleanup_entries.is_empty() {
        return Ok(());
    }

    let cleanup_state = purgery_core::DurableCleanupState {
        nickname: config.nickname.as_str().to_owned(),
        operation_id: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string(),
        entries: cleanup_entries,
    };

    let state_path = write_cleanup_state(&cleanup_state)?;

    for entry in &cleanup_state.entries {
        let local_path = Path::new(&entry.local_path);
        let symmeta = match fs::symlink_metadata(local_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = mark_cleaned(&state_path, &entry.sync_name, &entry.local_path);
                continue;
            }
            Err(_) => continue,
        };
        if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
            continue;
        }
        let Ok(meta) = fs::metadata(local_path) else {
            continue;
        };
        if meta.len() != entry.size {
            continue;
        }
        let current_mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        if current_mtime != entry.mtime_ns {
            continue;
        }
        if let Some(ref expected_sha) = entry.sha256 {
            if let Ok(actual_sha) = compute_sha256(local_path) {
                if &actual_sha != expected_sha {
                    continue;
                }
            } else {
                continue;
            }
        }
        if let Err(e) = fs::remove_file(local_path) {
            warn!(path = %entry.local_path, error = %e, "failed to delete");
        } else {
            let _ = mark_cleaned(&state_path, &entry.sync_name, &entry.local_path);
        }
    }

    Ok(())
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

        // Status-based cleanup is for postprocess entries only.
        // Passthrough entries are cleaned via the durable local cleanup state
        // (process_cleanup_entries), not from server status.
        if manifest_entry.mode == purgery_core::ManifestEntryMode::Passthrough {
            continue;
        }

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
