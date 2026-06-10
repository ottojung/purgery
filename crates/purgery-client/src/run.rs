use crate::classify::walk_and_classify_sync;
use crate::cleanup::{compute_sha256, resume_pending_cleanups};
use crate::ssh::server_cmd;
use crate::transfer::{run_passthrough_path, run_postprocess_path};

use anyhow::{Context, Result};
use purgery_core::{
    resolve_executable, ClientConfig, ClientPostprocessConfig, Manifest, ManifestEntry, RunConfig,
    RunConfigSync, RunId, RunStatus, SyncExecutionClass,
};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

/// Build a RunConfig from a ClientConfig, optionally filtering to include
/// only purgatory sync groups and rules applicable to those groups.
pub(crate) fn build_run_config(config: &ClientConfig, purgatory_only: bool) -> RunConfig {
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
        postprocess: ClientPostprocessConfig { rules },
    }
}

/// Client boot-time check: verify local executables and config only.
/// Does NOT SSH into the server or mutate anything.
pub(crate) fn client_check(config: &ClientConfig, _config_path: &str) -> Result<()> {
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

pub(crate) fn sync_and_cleanup(config: &ClientConfig) -> Result<()> {
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
            &passthrough_nodelete,
            &passthrough_cleanup,
        )
    }
}

/// Poll for status via the server's status command.
pub(crate) fn poll_for_status(
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

/// Remove local entries that are confirmed imported and still match their uploaded identity.
pub(crate) fn delete_confirmed_files(
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
        // Only process entries with status "imported"
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
        // Passthrough entries are cleaned via the durable cleanup ledger,
        // not from server status. Covered entries must not be independently
        // cleaned from status — they are retired as part of the postprocessed
        // directory-root cleanup.
        if manifest_entry.mode != purgery_core::ManifestEntryMode::Postprocess {
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
                "no sync mapping for entry"
            );
            continue;
        };

        // Only delete if the sync mapping allows it
        if !sync.delete_after_import {
            continue;
        }

        let local_path_str = manifest_entry.local_path.as_str();
        let local_path = Path::new(local_path_str);

        // Use symlink_metadata to detect post-upload replacements.
        let symmeta = match fs::symlink_metadata(local_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Entry already gone — idempotent, count as success
                count += 1;
                continue;
            }
            Err(_) => {
                warn!(path = %local_path.display(), "cannot read metadata, not removing");
                continue;
            }
        };

        match manifest_entry.kind {
            purgery_core::ManifestEntryKind::Directory => {
                // Verify the current path is still a directory
                if !symmeta.file_type().is_dir() {
                    warn!(path = %local_path.display(), "local path is no longer a directory, not removing");
                    continue;
                }
                // Recursive preflight: check the root directory itself and every
                // descendant directory for unexpected filesystem children.
                // First, collect all descendant entries from the manifest.
                let prefix = format!("{}/", local_path_str);
                let mut descendants: Vec<&ManifestEntry> = manifest
                    .entries
                    .iter()
                    .filter(|me| me.local_path.as_str().starts_with(&prefix))
                    .collect();
                let mut preflight_ok = true;
                // Check the root directory itself first.
                if let Ok(reader) = fs::read_dir(local_path) {
                    for child in reader {
                        let child = match child {
                            Ok(c) => c,
                            Err(_) => {
                                preflight_ok = false;
                                break;
                            }
                        };
                        let child_path = child.path();
                        if child_path == local_path {
                            continue;
                        }
                        let child_local = child_path.to_string_lossy();
                        if !manifest
                            .entries
                            .iter()
                            .any(|me| me.local_path.as_str() == child_local.as_ref())
                        {
                            preflight_ok = false;
                            break;
                        }
                    }
                } else {
                    preflight_ok = false;
                }
                // Then check every descendant directory.
                for desc in &descendants {
                    if desc.kind != purgery_core::ManifestEntryKind::Directory {
                        continue;
                    }
                    let desc_path = Path::new(desc.local_path.as_str());
                    if let Ok(reader) = fs::read_dir(desc_path) {
                        for child in reader {
                            let child = match child {
                                Ok(c) => c,
                                Err(_) => {
                                    preflight_ok = false;
                                    break;
                                }
                            };
                            let child_path = child.path();
                            if child_path == desc_path {
                                continue;
                            }
                            let child_local = child_path.to_string_lossy();
                            if !manifest
                                .entries
                                .iter()
                                .any(|me| me.local_path.as_str() == child_local.as_ref())
                            {
                                preflight_ok = false;
                                break;
                            }
                        }
                    }
                    if !preflight_ok {
                        break;
                    }
                }
                if !preflight_ok {
                    warn!(path = %local_path.display(), "subtree has new or unexpected entries, not removing");
                    continue;
                }
                // Verify all known descendants still match their identities.
                // Absent descendants are treated as idempotently already removed.
                let mut all_verified = true;
                for desc in &descendants {
                    let desc_path = Path::new(desc.local_path.as_str());
                    if !desc_path.exists() {
                        continue;
                    }
                    if !verify_manifest_entry_local(desc) {
                        warn!(
                            path = %desc.local_path.as_str(),
                            "known descendant changed since upload, not removing directory"
                        );
                        all_verified = false;
                        break;
                    }
                }
                if !all_verified {
                    continue;
                }
                // All preflight checks passed - delete bottom-up (deepest first)
                // Skip the root itself since we handle it separately
                descendants.sort_by(|a, b| {
                    b.local_path
                        .as_str()
                        .len()
                        .cmp(&a.local_path.as_str().len())
                });
                for child in &descendants {
                    let child_path = Path::new(child.local_path.as_str());
                    match child.kind {
                        purgery_core::ManifestEntryKind::Directory => {
                            let _ = fs::remove_dir(child_path);
                        }
                        _ => {
                            let _ = fs::remove_file(child_path);
                        }
                    }
                }
                if let Err(e) = fs::remove_dir(local_path) {
                    warn!(path = %local_path.display(), error = %e, "failed to remove directory");
                } else {
                    count += 1;
                }
            }
            purgery_core::ManifestEntryKind::Symlink => {
                // Verify it's still a symlink
                if !symmeta.file_type().is_symlink() {
                    warn!(path = %local_path.display(), "local path is no longer a symlink, not removing");
                    continue;
                }
                // Verify the link target matches
                if let Some(ref expected_target) = manifest_entry.link_target {
                    if let Ok(current_target) = fs::read_link(local_path) {
                        let current = current_target.to_string_lossy().into_owned();
                        if current != expected_target.as_str() {
                            warn!(path = %local_path.display(), "symlink target changed, not removing");
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                // Unlink the symlink (never follow the target)
                if let Err(e) = fs::remove_file(local_path) {
                    warn!(path = %local_path.display(), error = %e, "failed to unlink symlink");
                } else {
                    count += 1;
                }
            }
            purgery_core::ManifestEntryKind::RegularFile => {
                // Verify it's still a regular file (not replaced by symlink)
                if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
                    warn!(path = %local_path.display(), "local path is no longer a regular file, not removing");
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
                    let matches_sha = match &manifest_entry.sha256 {
                        Some(expected_sha) => match compute_sha256(local_path) {
                            Ok(actual_sha) => &actual_sha == expected_sha,
                            Err(_) => {
                                warn!(path = %local_path.display(), "SHA-256 computation failed during cleanup verification, not removing");
                                false
                            }
                        },
                        None => {
                            warn!(path = %local_path.display(), "entry has no SHA-256 identity, not removing");
                            false
                        }
                    };

                    if !matches_size || !matches_mtime || !matches_sha {
                        warn!(path = %local_path.display(), "file changed since upload, not removing");
                        continue;
                    }
                } else {
                    warn!(path = %local_path.display(), "cannot read metadata, not removing");
                    continue;
                }

                if let Err(e) = fs::remove_file(local_path) {
                    warn!(path = %local_path.display(), error = %e, "failed to delete file");
                } else {
                    count += 1;
                }
            }
        }
    }

    Ok(count)
}

/// Verify that a manifest entry still matches its captured local identity.
/// Used by directory cleanup to check children before removal.
pub(crate) fn verify_manifest_entry_local(entry: &ManifestEntry) -> bool {
    let path = Path::new(entry.local_path.as_str());
    let symmeta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    match entry.kind {
        purgery_core::ManifestEntryKind::RegularFile => {
            if !symmeta.file_type().is_file() || symmeta.file_type().is_symlink() {
                return false;
            }
            let Ok(meta) = fs::metadata(path) else {
                return false;
            };
            if meta.len() != entry.size {
                return false;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            if mtime != entry.mtime_ns {
                return false;
            }
            let Some(expected_sha) = entry.sha256.as_ref() else {
                return false;
            };
            let Ok(actual_sha) = compute_sha256(path) else {
                return false;
            };
            actual_sha == *expected_sha
        }
        purgery_core::ManifestEntryKind::Symlink => {
            if !symmeta.file_type().is_symlink() {
                return false;
            }
            if let Some(ref expected_target) = entry.link_target {
                if let Ok(current) = fs::read_link(path) {
                    current.to_string_lossy().into_owned() == expected_target.as_str()
                } else {
                    false
                }
            } else {
                true
            }
        }
        purgery_core::ManifestEntryKind::Directory => symmeta.file_type().is_dir(),
    }
}
