use crate::cleanup::{
    build_cleanup_entries_from_manifest, build_pre_rsync_cleanup_entries, mark_rsync_succeeded,
    process_cleanup_state_file, write_cleanup_state,
};
use crate::run::{build_run_config, wait_for_postprocess_run_and_cleanup};
use crate::ssh::{server_cmd, server_cmd_with_stdin, write_remote_file};

use anyhow::{Context, Result};
use camino::Utf8Path;
use purgery_core::{
    build_rsync_args, BeginRunResponse, ClientConfig, ClientRunPhase, Manifest, RunId,
};
use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tracing::{debug, info, span, Level};

/// Run a pure passthrough invocation (no postprocess entries in any sync group).
///
/// Every passthrough group uses direct unfiltered rsync.
/// PassthroughDeleteAfterImport groups additionally use the cleanup ledger
/// protocol: pre-rsync identity, cleanup state, rsync, success marker,
/// cleanup deletion.
pub(crate) fn run_passthrough_path(
    config: &ClientConfig,
    host: &str,
    server_command: &str,
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

    let dest_map: HashMap<&str, &purgery_core::SyncPassthroughDestination> = resolve_resp
        .destinations
        .iter()
        .map(|d| (d.sync_name.as_str(), d))
        .collect();

    // Every passthrough group uses direct unfiltered rsync — no per-entry filters.
    // PassthroughDeleteAfterImport groups capture cleanup identity before rsync.
    for sync in passthrough_nodelete {
        run_unfiltered_rsync(config, host, sync, &dest_map)?;
    }
    for sync in passthrough_cleanup {
        let sync_name = sync.name.as_str();
        let Some(dest) = dest_map.get(sync_name) else {
            anyhow::bail!("no destination for sync mapping '{sync_name}'");
        };
        let rsync_dest = format!("{}:{}/", host, dest.passthrough_dest);

        // 1. Capture pre-rsync cleanup identity
        let cleanup_entries = build_pre_rsync_cleanup_entries(config, sync)?;
        if cleanup_entries.is_empty() {
            // No regular files to clean up, just rsync
            run_unfiltered_rsync(config, host, sync, &dest_map)?;
            continue;
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
        let state_path = write_cleanup_state(&cleanup_state, &config.state_dir)
            .with_context(|| "failed to write pre-rsync cleanup state")?;

        // 2. Direct unfiltered rsync
        info!(
            sync = sync_name,
            dest = %dest.passthrough_dest,
            "passthrough direct rsync with cleanup"
        );
        let rsync_args = build_rsync_args(sync.from_path.as_str(), &rsync_dest);
        let status = Command::new("rsync")
            .args(&rsync_args)
            .status()
            .with_context(|| format!("failed to execute rsync for {}", sync.from_path.as_str()))?;
        if !status.success() {
            anyhow::bail!("rsync failed for sync mapping '{sync_name}'");
        }

        // 3. Mark rsync succeeded durably
        mark_rsync_succeeded(&state_path, sync_name)
            .with_context(|| "failed to mark rsync succeeded in cleanup state")?;

        // 4. Delete files whose identity still matches
        process_cleanup_state_file(&state_path)
            .with_context(|| "failed to process cleanup state after rsync")?;
    }

    info!("passthrough run complete");
    Ok(())
}

/// Run unfiltered rsync for a direct-rsync-only sync group (no filter file needed).
pub(crate) fn run_unfiltered_rsync(
    _config: &ClientConfig,
    host: &str,
    sync: &purgery_core::SyncMapping,
    dest_map: &HashMap<&str, &purgery_core::SyncPassthroughDestination>,
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
    let status = Command::new("rsync")
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
pub(crate) fn run_postprocess_path(
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
    let passthrough_dest_map: HashMap<String, String> = if !passthrough_nodelete.is_empty()
        || !passthrough_cleanup.is_empty()
    {
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
        let resolve_resp: purgery_core::ResolveDestinationsResponse = toml::from_str(&resolve_out)
            .context("failed to parse resolve-destinations response")?;
        resolve_resp
            .destinations
            .into_iter()
            .map(|d| (d.sync_name, d.passthrough_dest))
            .collect()
    } else {
        HashMap::new()
    };

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

    let dest_map: HashMap<&str, &purgery_core::SyncDestination> = prepare_resp
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

    // 4b. Handle PassthroughNoDelete groups after prepare-run validation.
    for sync in passthrough_nodelete {
        let sync_name = sync.name.as_str();
        let from_path = sync.from_path.as_str();
        if let Some(passthrough_dest) = passthrough_dest_map.get(sync_name) {
            let rsync_dest = format!("{}:{}/", host, passthrough_dest);
            info!(
                sync = sync_name,
                from = from_path,
                dest = %passthrough_dest,
                "passthrough-only direct rsync after prepare-run"
            );
            let rsync_args = build_rsync_args(from_path, &rsync_dest);
            let status = Command::new("rsync")
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

    // 4c. Handle PassthroughDeleteAfterImport groups after prepare-run validation.
    for sync in passthrough_cleanup {
        let sync_name = sync.name.as_str();
        let Some(passthrough_dest) = passthrough_dest_map.get(sync_name) else {
            anyhow::bail!("no destination resolved for passthrough sync mapping '{sync_name}'");
        };
        let rsync_dest = format!("{}:{}/", host, passthrough_dest);

        let cleanup_entries = build_pre_rsync_cleanup_entries(config, sync)?;
        if cleanup_entries.is_empty() {
            let args = build_rsync_args(sync.from_path.as_str(), &rsync_dest);
            let s = Command::new("rsync")
                .args(&args)
                .status()
                .with_context(|| {
                    format!("failed to execute rsync for {}", sync.from_path.as_str())
                })?;
            if !s.success() {
                anyhow::bail!("rsync failed for sync mapping '{sync_name}'");
            }
            continue;
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
        let state_path = write_cleanup_state(&cleanup_state, &config.state_dir)
            .with_context(|| "failed to write pre-rsync cleanup state")?;

        info!(
            sync = sync_name,
            dest = %passthrough_dest,
            "passthrough direct rsync with cleanup after prepare-run"
        );
        let rsync_args = build_rsync_args(sync.from_path.as_str(), &rsync_dest);
        let status = Command::new("rsync")
            .args(&rsync_args)
            .status()
            .with_context(|| format!("failed to execute rsync for {}", sync.from_path.as_str()))?;
        if !status.success() {
            anyhow::bail!("rsync failed for sync mapping '{sync_name}'");
        }

        mark_rsync_succeeded(&state_path, sync_name)
            .with_context(|| "failed to mark rsync succeeded in cleanup state")?;

        process_cleanup_state_file(&state_path)
            .with_context(|| "failed to process cleanup state after rsync")?;
    }

    // 5. Transfer per purgatory sync group
    let sync_map = config
        .sync
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect::<HashMap<&str, &purgery_core::SyncMapping>>();
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

            let tmp_dir = Utf8Path::new(&config.state_dir)
                .join("tmp")
                .join(run_id.as_str())
                .join("filters");
            fs::create_dir_all(&tmp_dir).with_context(|| {
                format!("failed to create filter directory: {}", tmp_dir.as_str())
            })?;
            let passthrough_file = tmp_dir.join(format!("passthrough-{sync_name}"));
            let purgatory_file = tmp_dir.join(format!("purgatory-{sync_name}"));

            // Pre-rsync cleanup ledger state for delete_after_import=true
            // Write cleanup state before passthrough rsync so that deletion
            // is only authorized after rsync succeeds.
            let maybe_cleanup_state: Option<purgery_core::DurableCleanupState> =
                if sync.delete_after_import && !passthrough_roots.is_empty() {
                    let cleanup_entries =
                        build_cleanup_entries_from_manifest(config, sync_name, manifest)?;
                    if cleanup_entries.is_empty() {
                        None
                    } else {
                        Some(purgery_core::DurableCleanupState {
                            nickname: config.nickname.as_str().to_owned(),
                            operation_id: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos()
                                .to_string(),
                            entries: cleanup_entries,
                        })
                    }
                } else {
                    None
                };

            let state_path = match &maybe_cleanup_state {
                Some(state) => Some(
                    write_cleanup_state(state, &config.state_dir)
                        .with_context(|| "failed to write pre-rsync cleanup state")?,
                ),
                None => None,
            };

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
                let pt_filter_arg = format!("--filter=merge {}", passthrough_file.as_str());
                purgery_core::insert_rsync_option_before_operands(&mut pt_args, pt_filter_arg)
                    .map_err(|e| anyhow::anyhow!("failed to insert passthrough filter arg: {e}"))?;
                let pt_status =
                    Command::new("rsync")
                        .args(&pt_args)
                        .status()
                        .with_context(|| {
                            format!("failed to execute passthrough rsync for {from_path}")
                        })?;
                if !pt_status.success() {
                    anyhow::bail!("passthrough rsync failed for sync mapping '{sync_name}'");
                }
                info!(sync = sync_name, mode = "passthrough", "rsync complete");

                // Mark rsync succeeded durably so cleanup is authorized
                if let Some(ref sp) = state_path {
                    mark_rsync_succeeded(sp, sync_name)
                        .with_context(|| "failed to mark rsync succeeded")?;
                }
            } else {
                info!(
                    sync = sync_name,
                    mode = "passthrough",
                    "no passthrough roots, skipping rsync"
                );
            }

            // Process cleanup state: remove entries whose identity still matches
            if let Some(ref sp) = state_path {
                process_cleanup_state_file(sp)
                    .with_context(|| "failed to process cleanup state")?;
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
                let pg_filter_arg = format!("--filter=merge {}", purgatory_file.as_str());
                purgery_core::insert_rsync_option_before_operands(&mut pg_args, pg_filter_arg)
                    .map_err(|e| anyhow::anyhow!("failed to insert purgatory filter arg: {e}"))?;
                let pg_status =
                    Command::new("rsync")
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
        // Persist local run state BEFORE calling finish-run, so a crash
        // after finish-run succeeds can be resumed with the same run.
        let run_config = build_run_config(config, true);
        crate::run::write_client_run_state(
            &config.state_dir,
            config.nickname.as_str(),
            run_id.as_str(),
            manifest,
            &run_config,
            ClientRunPhase::UploadCompleteFinishPending,
        )?;
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

        // Transition to waiting state after finish-run succeeds
        crate::run::write_client_run_state(
            &config.state_dir,
            config.nickname.as_str(),
            run_id.as_str(),
            manifest,
            &run_config,
            ClientRunPhase::WaitingForTerminalState,
        )?;

        Ok(())
    })();

    stop_hb.store(true, Ordering::Relaxed);
    let _ = hb_handle.join();
    sync_result?;

    // 7. Wait for terminal status and clean up
    wait_for_postprocess_run_and_cleanup(
        config,
        host,
        server_command,
        manifest,
        &config.nickname,
        run_id,
    )
}
