use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use purgery_core::{
    build_rsync_args, shell_escape, ClientConfig, ClientLocalPath, Manifest, ManifestFileEntry,
    NormalizedRelativePath, RunId, RunStatus,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::SyncAndCleanup { config } => {
            sync_and_cleanup(&config)?;
        }
    }
    Ok(())
}

fn sync_and_cleanup(config_path: &str) -> Result<()> {
    let config_content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read client config: {config_path}"))?;
    let config = ClientConfig::from_toml(&config_content)
        .with_context(|| "failed to parse client config")?;

    // 1. Generate a unique run ID
    let run_id = RunId::generate();
    eprintln!(
        "starting run {}/{}",
        config.nickname.as_str(),
        run_id.as_str()
    );

    // 2. Walk local files and build manifest
    let manifest = build_manifest(&config, &run_id)?;
    eprintln!("discovered {} file(s) to sync", manifest.files.len());

    // 3. Create run directory on server
    let remote_incoming_dir = format!(
        "{}/{}/incoming/{}",
        config.server.purgery_root.as_str(),
        config.nickname.as_str(),
        run_id.as_str()
    );

    // 4. Create directories on server via SSH
    let mkdir_cmd = format!(
        "mkdir -p {}",
        purgery_core::shell_escape(&format!("{remote_incoming_dir}/files"))
    );
    ssh_run(config.server.host.as_str(), &mkdir_cmd)?;

    // 5. Write config and manifest to server
    let config_toml = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config: {config_path}"))?;
    write_remote_file(
        config.server.host.as_str(),
        &format!("{remote_incoming_dir}/config.toml"),
        &config_toml,
    )?;
    let manifest_toml = manifest.to_toml()?;
    write_remote_file(
        config.server.host.as_str(),
        &format!("{remote_incoming_dir}/manifest.toml"),
        &manifest_toml,
    )?;

    // 6. Rsync files per sync mapping
    for sync in &config.sync {
        let from_path = sync.from_path.as_str();
        let to_path = sync.to_path.as_str();
        let remote_files_dir = format!("{remote_incoming_dir}/files/{to_path}/");

        eprintln!("syncing {from_path} -> {remote_files_dir}");
        let rsync_dest = format!("{}:{}", config.server.host.as_str(), remote_files_dir);
        let args = build_rsync_args(from_path, &rsync_dest);
        let status = std::process::Command::new("rsync")
            .args(&args)
            .status()
            .with_context(|| format!("failed to execute rsync for {from_path}"))?;

        if !status.success() {
            anyhow::bail!("rsync failed for sync mapping '{}'", sync.name);
        }
    }

    // 7. Atomically move from incoming to ready
    let remote_ready_dir = format!(
        "{}/{}/ready/{}",
        config.server.purgery_root.as_str(),
        config.nickname.as_str(),
        run_id.as_str()
    );
    let ready_cmd = format!(
        "mv {} {}",
        purgery_core::shell_escape(&remote_incoming_dir),
        purgery_core::shell_escape(&remote_ready_dir),
    );
    ssh_run(config.server.host.as_str(), &ready_cmd)?;
    eprintln!("run moved to ready");

    // 8. Poll for status in done or failed directory
    let done_dir = format!(
        "{}/{}/done/{}",
        config.server.purgery_root.as_str(),
        config.nickname.as_str(),
        run_id.as_str()
    );
    let failed_dir = format!(
        "{}/{}/failed/{}",
        config.server.purgery_root.as_str(),
        config.nickname.as_str(),
        run_id.as_str()
    );

    let status = poll_for_status(config.server.host.as_str(), &done_dir, &failed_dir)?;

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

/// Poll for status.toml in done or failed directories.
fn poll_for_status(host: &str, done_dir: &str, failed_dir: &str) -> Result<RunStatus> {
    let max_attempts = 60;
    let poll_interval = Duration::from_secs(2);

    for attempt in 1..=max_attempts {
        // Check done directory
        let status_path = format!("{done_dir}/status.toml");
        let output = ssh_run(host, &format!("cat {}", shell_escape(&status_path)));

        if let Ok(content) = output {
            if !content.trim().is_empty() {
                return RunStatus::from_toml(content.trim())
                    .with_context(|| "failed to parse status from done");
            }
        }

        // Check failed directory
        let failed_status_path = format!("{failed_dir}/status.toml");
        let output = ssh_run(host, &format!("cat {}", shell_escape(&failed_status_path)));

        if let Ok(content) = output {
            if !content.trim().is_empty() {
                return RunStatus::from_toml(content.trim())
                    .with_context(|| "failed to parse status from failed");
            }
        }

        // Check if the ready directory still exists (run not yet claimed)
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
