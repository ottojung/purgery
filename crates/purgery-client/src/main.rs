use anyhow::{Context, Result};
use camino::Utf8Path;
use clap::{Parser, Subcommand};
use purgery_core::{
    build_rsync_args, resolve_executable, shell_escape, BeginRunResponse, ClientConfig,
    ClientLocalPath, Manifest, ManifestFileEntry, NormalizedRelativePath, RunConfig, RunConfigSync,
    RunId, RunStatus,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

#[derive(Parser)]
#[command(
    name = "purgery-client",
    about = "Purgery client: sync files to server and clean up imported files",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
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
    match cli.command {
        Command::SyncAndCleanup { config } => {
            sync_and_cleanup(&config)?;
        }
        Command::Check { config } => {
            client_check(&config)?;
        }
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
fn client_check(config_path: &str) -> Result<()> {
    eprintln!("checking client configuration...");

    // 1. Check ssh is accessible
    resolve_executable("ssh").map(|r| {
        eprintln!("  ssh: found at {}", r.path.as_str());
    })?;

    // 2. Check rsync is accessible
    resolve_executable("rsync").map(|r| {
        eprintln!("  rsync: found at {}", r.path.as_str());
    })?;

    // 3. Validate config
    let config_content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read client config: {config_path}"))?;
    let config = ClientConfig::from_toml(&config_content)
        .with_context(|| "failed to parse client config")?;

    if config.server.host.as_str().is_empty() {
        anyhow::bail!("server host is empty");
    }
    if config.server.command.is_empty() {
        anyhow::bail!("server command is empty");
    }

    eprintln!("client configuration: OK");
    Ok(())
}

fn sync_and_cleanup(config_path: &str) -> Result<()> {
    // 0. Run local checks before any remote operations
    client_check(config_path)?;

    let config_content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read client config: {config_path}"))?;
    let config = ClientConfig::from_toml(&config_content)
        .with_context(|| "failed to parse client config")?;

    // 1. Generate a unique run ID and build manifest BEFORE begin-run
    let run_id = RunId::generate();
    let host = config.server.host.as_str();
    let server_command = &config.server.command;

    eprintln!(
        "starting run {}/{}",
        config.nickname.as_str(),
        run_id.as_str()
    );

    let manifest = build_manifest(&config, &run_id)?;
    eprintln!("discovered {} file(s) to sync", manifest.files.len());

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

    eprintln!("  incoming dir: {}", begin_resp.incoming_dir);

    // 3. Write run.toml and manifest.toml to server
    let run_config = build_run_config(&config);
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

    // 5. Rsync files per sync mapping
    let sync_result = (|| -> Result<()> {
        for sync in &config.sync {
            let from_path = sync.from_path.as_str();
            let to_path = sync.to_path.as_str();
            let remote_files_dir = format!("{}/{to_path}/", begin_resp.files_dir);

            eprintln!("syncing {from_path} -> {remote_files_dir}");
            let rsync_dest = format!("{}:{}", host, remote_files_dir);
            let args = build_rsync_args(from_path, &rsync_dest);
            let status = std::process::Command::new("rsync")
                .args(&args)
                .status()
                .with_context(|| format!("failed to execute rsync for {from_path}"))?;

            if !status.success() {
                anyhow::bail!("rsync failed for sync mapping '{}'", sync.name);
            }

            // Check for heartbeat failure after each mapping so we can
            // avoid calling finish-run if the lease is already lost.
            if let Some(err) = hb_error.lock().unwrap().take() {
                anyhow::bail!("{err}");
            }
        }

        // 6. Check heartbeat one last time before finish-run. If the lease has
        // expired, finish-run would succeed but the client would still exit
        // without polling status — operationally awkward, not data-lossy,
        // but still better to fail early here.
        if let Some(err) = hb_error.lock().unwrap().take() {
            anyhow::bail!("{err}");
        }

        // 7. Finish run: move from incoming to ready
        eprintln!("finishing run...");
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
        eprintln!("run moved to ready");
        Ok(())
    })();

    // Stop heartbeat thread regardless of sync/finish outcome
    stop_hb.store(true, Ordering::Relaxed);
    let _ = hb_handle.join();

    // Propagate sync/finish error first
    sync_result?;

    // Then check for heartbeat failure
    if let Some(err) = hb_error.lock().unwrap().take() {
        anyhow::bail!("{err}");
    }

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
    let deletion_count = delete_confirmed_files(&config, &manifest, &status)?;
    eprintln!("deleted {deletion_count} confirmed local file(s)");

    eprintln!(
        "run {}/{} finished with state {}",
        config.nickname.as_str(),
        run_id.as_str(),
        status.state.as_str()
    );

    Ok(())
}

/// Walk all sync directories and build the manifest.
fn build_manifest(config: &ClientConfig, run_id: &RunId) -> Result<Manifest> {
    let mut files = Vec::new();
    let nickname = config.nickname.clone();

    for sync in &config.sync {
        let from_path = sync.from_path.as_str();
        let to_path = sync.to_path.as_str();
        let from = Path::new(from_path);

        if !from.exists() {
            eprintln!("warning: sync path does not exist, skipping: {from_path}");
            continue;
        }

        for entry in WalkDir::new(from).follow_links(false) {
            let entry = entry.with_context(|| format!("error walking {from_path}"))?;
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let metadata = fs::metadata(path)
                .with_context(|| format!("failed to read metadata: {}", path.display()))?;

            let relative = path.strip_prefix(from).with_context(|| {
                format!("failed to compute relative path for: {}", path.display())
            })?;

            let relative_str = relative.to_string_lossy().to_string();
            let staged_path_str = format!("files/{to_path}/{relative_str}");

            let size = metadata.len();
            let mtime_ns = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);

            let sha256 = compute_sha256(path).ok();

            files.push(ManifestFileEntry {
                sync_name: sync.name.clone(),
                local_path: ClientLocalPath::new(path.to_string_lossy().to_string())
                    .with_context(|| format!("invalid local path for: {}", path.display()))?,
                staged_path: NormalizedRelativePath::new(staged_path_str.into())
                    .with_context(|| format!("invalid staged path for: {}", path.display()))?,
                relative_path: NormalizedRelativePath::new(relative_str.into())
                    .with_context(|| format!("invalid relative path for: {}", path.display()))?,
                size,
                mtime_ns,
                sha256,
            });
        }
    }

    if files.is_empty() {
        anyhow::bail!("no files found to sync (all sync directories may be empty or missing)");
    }

    Ok(Manifest {
        run_id: run_id.clone(),
        nickname,
        files,
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
            eprintln!("waiting for server to process run (attempt {attempt}/{max_attempts})...");
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
    let manifest_by_path: std::collections::HashMap<&str, &ManifestFileEntry> = manifest
        .files
        .iter()
        .map(|f| (f.local_path.as_str(), f))
        .collect();

    for file_status in &status.files {
        // Only delete files with status "imported"
        if file_status.status != purgery_core::FileStatus::Imported {
            continue;
        }

        // Find the corresponding manifest entry
        let Some(manifest_entry) = manifest_by_path.get(file_status.local_path.as_str()) else {
            eprintln!(
                "warning: status references unknown local path: {}",
                file_status.local_path
            );
            continue;
        };

        // Find the corresponding sync mapping
        let Some(sync) = config
            .sync
            .iter()
            .find(|s| s.name.as_str() == manifest_entry.sync_name.as_str())
        else {
            eprintln!(
                "warning: no sync mapping for '{}'",
                manifest_entry.sync_name.as_str()
            );
            continue;
        };

        // Only delete if the sync mapping allows it
        if !sync.delete_after_import {
            continue;
        }

        let local_path_str = manifest_entry.local_path.as_str();
        let local_path = Path::new(local_path_str);

        // Check that file still exists and matches identity
        if !local_path.exists() {
            // File already gone — idempotent, count as success
            count += 1;
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
                eprintln!(
                    "warning: file '{}' changed since upload, not deleting",
                    local_path.display()
                );
                continue;
            }
        } else {
            // Can't read metadata — skip for safety
            eprintln!(
                "warning: cannot read metadata for '{}', not deleting",
                local_path.display()
            );
            continue;
        }

        // Safe to delete
        if let Err(e) = fs::remove_file(local_path) {
            eprintln!("warning: failed to delete '{}': {e}", local_path.display());
        } else {
            count += 1;
        }
    }

    Ok(count)
}
