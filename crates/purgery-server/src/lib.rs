use anyhow::{Context, Result};
use purgery_core::{Nickname, RunId, RunPhase, RunStatus, ServerConfig};
use std::fs;
use tracing::info;

#[cfg_attr(not(test), allow(unused_imports))]
use camino::Utf8Path;
#[cfg_attr(not(test), allow(unused_imports))]
use purgery_core::{
    work_dir, FileStatus, Manifest, ManifestEntryKind, PurgeryRoot, RunConfig, RunState,
};

mod commit;
mod gc;
mod phases;
mod postprocess;
mod process;
mod recover;

pub use gc::run_gc;
pub use phases::{begin_run, find_processing_runs, find_ready_runs, finish_run, move_to_failed};
pub use postprocess::{apply_postprocessing, apply_postprocessing_with_heartbeat};
pub use process::{process_once_raw, process_processing_run, process_ready_run};
pub use recover::recover_or_process_processing_run;

pub(crate) use process::validate_unique_final_paths;

#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use commit::{
    commit_directory_entry, commit_regular_file_entry, commit_symlink_entry, CommitDisposition,
};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use phases::{write_progress, write_progress_best_effort};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use process::planned_entry_outputs;

/// A compiled postprocess rule with resolved step definitions.
#[derive(Debug)]
pub struct CompiledRule {
    pub pattern: String,
    pub steps: Vec<ResolvedStep>,
    /// Optional sync group scoping (None means all groups).
    pub sync_names: Option<Vec<purgery_core::SyncName>>,
}

impl CompiledRule {
    /// Returns true if the normalized path matches this rule's rsync pattern.
    pub fn is_match(&self, normalized_path: &str) -> bool {
        purgery_core::rsync_pattern_match(&self.pattern, normalized_path)
    }

    /// Returns true if this rule applies to the given sync group.
    pub fn applies_to(&self, sync_name: &str) -> bool {
        match &self.sync_names {
            None => true,
            Some(names) => names.iter().any(|n| n.as_str() == sync_name),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedStep {
    pub step_name: String,
    pub step_def: purgery_core::PostprocessStepDefinition,
}

/// A validated run plan: precompiled rsync patterns and resolved step definitions.
#[derive(Debug)]
pub struct RunPlan {
    pub rules: Vec<CompiledRule>,
}

impl RunPlan {
    /// Return the first compiled rule that both applies to the given sync
    /// group AND matches the given normalized relative path, or None.
    pub fn first_matching_rule<'a>(
        &'a self,
        sync_name: &str,
        normalized_path: &str,
    ) -> Option<&'a CompiledRule> {
        self.rules
            .iter()
            .find(|r| r.applies_to(sync_name) && r.is_match(normalized_path))
    }

    /// Returns true if any rule applies to the given sync group and matches
    /// the given normalized relative path.
    pub fn entry_is_postprocess(&self, sync_name: &str, normalized_path: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.applies_to(sync_name) && r.is_match(normalized_path))
    }

    /// Return step names from the first matching rule for the given sync group
    /// and normalized relative path. Later matching rules are ignored (first-match-wins).
    pub fn selected_steps_for(&self, sync_name: &str, normalized_path: &str) -> Vec<String> {
        self.rules
            .iter()
            .find(|r| r.applies_to(sync_name) && r.is_match(normalized_path))
            .map(|r| r.steps.iter().map(|s| s.step_name.clone()).collect())
            .unwrap_or_default()
    }

    /// Build a run plan from server config and run config.
    ///
    /// Validates all patterns and step references. Returns an error
    /// (suitable for run-level failure) if anything is invalid.
    pub fn build(
        server_config: &ServerConfig,
        run_config: &purgery_core::RunConfig,
    ) -> Result<Self, String> {
        run_config
            .validate_uploaded_purgatory_run()
            .map_err(|e| format!("uploaded run config validation failed: {e}"))?;

        let mut rules = Vec::new();

        for rule in &run_config.postprocess.rules {
            if rule.pattern.is_empty() {
                return Err("postprocess rule has empty pattern".into());
            }

            let mut steps = Vec::new();
            for step_name in &rule.steps {
                let Some(def) = server_config.postprocess.steps.get(step_name.as_str()) else {
                    return Err(format!(
                        "postprocess step '{step_name}' referenced by rule is not defined on server"
                    ));
                };

                for output in &def.expected_outputs {
                    purgery_core::validate_expected_output_name(output).map_err(|e| {
                        format!("postprocess step '{step_name}': expected_output {output:?}: {e}")
                    })?;
                }

                if !def.keep_original && def.expected_outputs.is_empty() {
                    return Err(format!(
                        "postprocess step '{step_name}': keep_original=false with no \
                         expected_outputs would produce zero committed outputs"
                    ));
                }

                steps.push(ResolvedStep {
                    step_name: step_name.clone(),
                    step_def: def.clone(),
                });
            }

            rules.push(CompiledRule {
                pattern: rule.pattern.clone(),
                steps,
                sync_names: rule.sync_names.clone(),
            });
        }

        Ok(RunPlan { rules })
    }
}

/// Process a ready run. Kept as the public single-run entry point.
pub fn process_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    process_ready_run(config, nickname, run_id)
}

/// Server-side subcommand: validate the run plan and return transfer destinations.
///
/// Must be called after the client has written `run.toml` and `manifest.toml`
/// into the incoming directory but before any rsync transfer.
/// This is the gate that prevents passthrough transfers into final storage
/// for an invalid run plan.
pub fn prepare_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<String> {
    let incoming_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Incoming);
    if !incoming_path.exists() {
        anyhow::bail!(
            "incoming directory does not exist for run {}/{} at '{}'",
            nickname.as_str(),
            run_id.as_str(),
            incoming_path.as_str()
        );
    }

    let run_config_path = incoming_path.join("run.toml");
    let run_config_content =
        fs::read_to_string(&run_config_path).with_context(|| "failed to read run config")?;
    let run_config = purgery_core::RunConfig::from_toml(&run_config_content)
        .with_context(|| "failed to parse run config")?;

    run_config
        .validate_uploaded_purgatory_run()
        .map_err(|e| anyhow::anyhow!("uploaded run config validation failed: {e}"))?;

    let manifest_path = incoming_path.join("manifest.toml");
    let manifest_content =
        fs::read_to_string(&manifest_path).with_context(|| "failed to read manifest")?;
    let manifest = purgery_core::Manifest::from_toml(&manifest_content)
        .with_context(|| "failed to parse manifest")?;

    if let Err(e) = purgery_core::validate_envelope(nickname, run_id, &run_config, &manifest) {
        anyhow::bail!("envelope validation failed: {e}");
    }

    {
        let sync_map = run_config.sync_map();
        for entry in &manifest.entries {
            let _sync = sync_map.get(entry.sync_name.as_str());
            let rp = entry.relative_path.as_str();
            let scoped_rules = purgery_core::applicable_rules(
                &run_config.postprocess.rules,
                entry.sync_name.as_str(),
            );
            let matched = scoped_rules
                .into_iter()
                .find(|r| purgery_core::rsync_pattern_match(&r.pattern, rp));
            let expected_mode = match matched {
                Some(_) => purgery_core::ManifestEntryMode::Postprocess,
                None => purgery_core::ManifestEntryMode::Passthrough,
            };

            let covering_ancestor = manifest.entries.iter().find(|de| {
                de.kind == purgery_core::ManifestEntryKind::Directory
                    && de.mode == purgery_core::ManifestEntryMode::Postprocess
                    && de.sync_name.as_str() == entry.sync_name.as_str()
                    && rp.starts_with(de.relative_path.as_str())
                    && rp.as_bytes().get(de.relative_path.as_str().len()) == Some(&b'/')
            });

            if let Some(ancestor) = covering_ancestor {
                let expected_covered_by = ancestor.relative_path.as_str();
                if entry.mode != purgery_core::ManifestEntryMode::Covered {
                    anyhow::bail!(
                        "classification mismatch: '{}' is a descendant of postprocessed \
                         directory but has mode '{:?}' instead of 'covered'",
                        rp,
                        entry.mode
                    );
                }
                if entry.covered_by.as_deref() != Some(expected_covered_by) {
                    anyhow::bail!(
                        "covered entry '{}' has covered_by {:?} but expected '{}'",
                        rp,
                        entry.covered_by,
                        expected_covered_by
                    );
                }
                if !entry.postprocess_steps.is_empty() {
                    anyhow::bail!(
                        "covered entry '{}' has non-empty postprocess_steps {:?}",
                        rp,
                        entry.postprocess_steps
                    );
                }
                continue;
            }

            if entry.mode != expected_mode {
                anyhow::bail!(
                    "classification mismatch for '{}': manifest says '{:?}' but \
                     pattern classification says '{:?}'",
                    rp,
                    entry.mode,
                    expected_mode
                );
            }

            if entry.mode == purgery_core::ManifestEntryMode::Postprocess {
                let Some(rule) = matched else {
                    anyhow::bail!(
                        "classification mismatch for '{}': postprocess mode but no matching rule",
                        rp
                    );
                };
                if entry.postprocess_steps != rule.steps {
                    anyhow::bail!(
                        "classification mismatch for '{}': postprocess_steps {:?} do not \
                         match rule steps {:?}",
                        rp,
                        entry.postprocess_steps,
                        rule.steps
                    );
                }
            }
        }
    }

    let run_plan = RunPlan::build(config, &run_config)
        .map_err(|e| anyhow::anyhow!("run plan validation failed: {e}"))?;

    let sync_map = run_config.sync_map();
    let covered_by_dir: std::collections::HashSet<(String, String)> = manifest
        .entries
        .iter()
        .filter(|e| e.kind == purgery_core::ManifestEntryKind::Directory)
        .filter_map(|dir_entry| {
            let _sync = sync_map.get(dir_entry.sync_name.as_str())?;
            let np = dir_entry.relative_path.as_str().to_owned();
            let matched = run_plan
                .rules
                .iter()
                .any(|rule| rule.applies_to(dir_entry.sync_name.as_str()) && rule.is_match(&np));
            if matched {
                Some((dir_entry.sync_name.as_str().to_owned(), np))
            } else {
                None
            }
        })
        .collect();

    let sync_map2 = run_config.sync_map();
    let covered_indices: std::collections::HashSet<usize> = manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            let Some(_sync) = sync_map2.get(entry.sync_name.as_str()) else {
                return false;
            };
            let np2 = entry.relative_path.as_str().to_owned();
            let entry_sync = entry.sync_name.as_str();
            covered_by_dir.iter().any(|(sync_name, prefix)| {
                sync_name.as_str() == entry_sync
                    && match np2.as_str().strip_prefix(prefix.as_str()) {
                        Some(tail) => tail.starts_with('/'),
                        None => false,
                    }
            })
        })
        .map(|(i, _)| i)
        .collect();

    validate_unique_final_paths(
        config,
        nickname,
        &run_config,
        &manifest,
        &run_plan,
        &covered_indices,
    )
    .map_err(|e| anyhow::anyhow!("destination validation failed: {e}"))?;

    let final_root = config.root.as_path().join(nickname.as_str());
    let purgatory_root = incoming_path.join("files");
    let destinations: Vec<purgery_core::SyncDestination> = run_config
        .sync
        .iter()
        .map(|sync| {
            let passthrough_dest = final_root.join(sync.to_path.as_str());
            let purgatory_dest = purgatory_root.join(sync.to_path.as_str());
            purgery_core::SyncDestination {
                sync_name: sync.name.as_str().to_owned(),
                passthrough_dest: passthrough_dest.as_str().to_owned(),
                purgatory_dest: purgatory_dest.as_str().to_owned(),
            }
        })
        .collect();

    let response = purgery_core::PrepareRunResponse {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        destinations,
    };

    toml::to_string(&response)
        .map_err(|e| anyhow::anyhow!("failed to serialize prepare-run response: {e}"))
}

/// Server-side subcommand: resolve final storage destinations for pure passthrough groups.
///
/// Side-effect-free. Does not create run directories, leases, manifests, or status files.
/// Returns the same passthrough destinations that `prepare-run` would return, without
/// requiring a run ID or creating any run state.
pub fn resolve_destinations(
    config: &ServerConfig,
    nickname: &Nickname,
    run_config: &purgery_core::RunConfig,
) -> Result<String> {
    let final_root = config.root.as_path().join(nickname.as_str());
    let destinations: Vec<purgery_core::SyncPassthroughDestination> = run_config
        .sync
        .iter()
        .map(|sync| {
            let passthrough_dest = final_root.join(sync.to_path.as_str());
            purgery_core::SyncPassthroughDestination {
                sync_name: sync.name.as_str().to_owned(),
                passthrough_dest: passthrough_dest.as_str().to_owned(),
            }
        })
        .collect();

    let response = purgery_core::ResolveDestinationsResponse {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        destinations,
    };

    toml::to_string(&response)
        .map_err(|e| anyhow::anyhow!("failed to serialize resolve-destinations response: {e}"))
}

/// Server-side subcommand: read the run status from done or failed.
pub fn read_run_status(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<RunStatus> {
    let phases = [RunPhase::Done, RunPhase::Failed];

    for phase in &phases {
        let status_path = config
            .purgery_root
            .run_dir(nickname, run_id, *phase)
            .join("status.toml");
        if !status_path.exists() {
            continue;
        }
        let content = fs::read_to_string(&status_path)
            .with_context(|| format!("failed to read status from '{}'", status_path.as_str()))?;
        match RunStatus::from_toml(&content) {
            Ok(status) => {
                if status.nickname != *nickname || status.run_id != *run_id {
                    anyhow::bail!(
                        "status envelope mismatch in '{}': expected {}/{}, got {}/{}",
                        status_path,
                        nickname.as_str(),
                        run_id.as_str(),
                        status.nickname.as_str(),
                        status.run_id.as_str()
                    );
                }
                return Ok(status);
            }
            Err(e) => {
                anyhow::bail!("malformed status file '{}': {e}", status_path.as_str());
            }
        }
    }

    anyhow::bail!(
        "status not found for run {}/{} in done or failed",
        nickname.as_str(),
        run_id.as_str()
    );
}

/// Read the current run phase without requiring terminal status.
/// Returns a RunStateResponse describing the run's filesystem phase.
pub fn run_state(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<purgery_core::RunStateResponse> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Check non-terminal phases first (incoming, ready, processing)
    let non_terminal = [
        (RunPhase::Incoming, "incoming", false),
        (RunPhase::Ready, "ready", false),
        (RunPhase::Processing, "processing", false),
    ];
    for (phase, phase_str, _) in &non_terminal {
        let dir = config.purgery_root.run_dir(nickname, run_id, *phase);
        if !dir.exists() {
            continue;
        }
        let (
            message,
            updated_at,
            progress_state,
            entry_index,
            entry_total,
            current_entry,
            current_step,
            progress_status,
        ) = if *phase == RunPhase::Processing {
            read_progress_fields(&dir, nickname, run_id, now)
        } else {
            (
                format!("run phase: {}", phase_str),
                dir_modified_at(&dir).unwrap_or(0),
                None::<String>,
                None::<usize>,
                None::<usize>,
                None::<String>,
                None::<String>,
                None::<String>,
            )
        };
        return Ok(purgery_core::RunStateResponse {
            protocol_version: 1,
            nickname: nickname.as_str().to_owned(),
            run_id: run_id.as_str().to_owned(),
            phase: phase_str.to_string(),
            terminal: false,
            message,
            updated_at_unix_secs: updated_at,
            observed_at_unix_secs: now,
            progress_state,
            entry_index,
            entry_total,
            current_entry,
            current_step,
            progress_status,
        });
    }

    // Check terminal phases (done, failed) — require valid status.toml
    let terminal_phases = [(RunPhase::Done, "done"), (RunPhase::Failed, "failed")];
    for (phase, phase_str) in &terminal_phases {
        let dir = config.purgery_root.run_dir(nickname, run_id, *phase);
        if !dir.exists() {
            continue;
        }
        let status_path = dir.join("status.toml");
        match try_read_status(&status_path, nickname, run_id) {
            Ok(()) => {
                return Ok(purgery_core::RunStateResponse {
                    protocol_version: 1,
                    nickname: nickname.as_str().to_owned(),
                    run_id: run_id.as_str().to_owned(),
                    phase: phase_str.to_string(),
                    terminal: true,
                    message: format!("run is {}", phase_str),
                    updated_at_unix_secs: dir_modified_at(&dir).unwrap_or(0),
                    observed_at_unix_secs: now,
                    progress_state: None,
                    entry_index: None,
                    entry_total: None,
                    current_entry: None,
                    current_step: None,
                    progress_status: None,
                });
            }
            Err(reason) => {
                return Ok(purgery_core::RunStateResponse {
                    protocol_version: 1,
                    nickname: nickname.as_str().to_owned(),
                    run_id: run_id.as_str().to_owned(),
                    phase: "corrupt".to_string(),
                    terminal: false,
                    message: format!("{} directory exists but status is {reason}", phase_str),
                    updated_at_unix_secs: dir_modified_at(&dir).unwrap_or(0),
                    observed_at_unix_secs: now,
                    progress_state: None,
                    entry_index: None,
                    entry_total: None,
                    current_entry: None,
                    current_step: None,
                    progress_status: None,
                });
            }
        }
    }

    // No phase directory found
    Ok(purgery_core::RunStateResponse {
        protocol_version: 1,
        nickname: nickname.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        phase: "not_found".to_string(),
        terminal: false,
        message: "no matching run found".to_string(),
        updated_at_unix_secs: 0,
        observed_at_unix_secs: now,
        progress_state: None,
        entry_index: None,
        entry_total: None,
        current_entry: None,
        current_step: None,
        progress_status: None,
    })
}

/// Try to read and validate a terminal status file.
/// Returns Ok(()) if the file exists, parses, and envelope matches.
/// Returns an error string explaining why validation failed.
fn try_read_status(
    status_path: &camino::Utf8Path,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<(), String> {
    let content = std::fs::read_to_string(status_path.as_std_path())
        .map_err(|e| format!("missing/unreadable: {e}"))?;
    let status =
        purgery_core::RunStatus::from_toml(&content).map_err(|e| format!("malformed: {e}"))?;
    if status.nickname != *nickname {
        return Err(format!(
            "envelope mismatch: expected nickname '{}', got '{}'",
            nickname.as_str(),
            status.nickname.as_str()
        ));
    }
    if status.run_id != *run_id {
        return Err(format!(
            "envelope mismatch: expected run_id '{}', got '{}'",
            run_id.as_str(),
            status.run_id.as_str()
        ));
    }
    Ok(())
}

/// Read progress.toml fields for a processing-phase response.
#[allow(clippy::type_complexity)]
fn read_progress_fields(
    dir: &camino::Utf8Path,
    nickname: &Nickname,
    run_id: &RunId,
    _now: u64,
) -> (
    String,
    u64,
    Option<String>,
    Option<usize>,
    Option<usize>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let progress_path = dir.join("progress.toml");
    match std::fs::read_to_string(&progress_path) {
        Ok(content) => match toml::from_str::<purgery_core::ProcessingProgress>(&content) {
            Ok(prog) if prog.nickname == nickname.as_str() && prog.run_id == run_id.as_str() => {
                let msg = format!(
                    "processing: {}/{} entries, current: {} step: {}",
                    prog.entry_index + 1,
                    prog.entry_total,
                    prog.current_entry,
                    prog.current_step
                );
                (
                    msg,
                    prog.updated_at_unix_secs,
                    Some(prog.state),
                    Some(prog.entry_index),
                    Some(prog.entry_total),
                    Some(prog.current_entry),
                    Some(prog.current_step),
                    Some("valid".to_string()),
                )
            }
            Ok(_) => (
                "run phase: processing (progress envelope mismatch)".to_string(),
                dir_modified_at(dir).unwrap_or(0),
                None,
                None,
                None,
                None,
                None,
                Some("envelope_mismatch".to_string()),
            ),
            Err(_) => (
                "run phase: processing (malformed progress)".to_string(),
                dir_modified_at(dir).unwrap_or(0),
                None,
                None,
                None,
                None,
                None,
                Some("malformed".to_string()),
            ),
        },
        Err(_) => (
            "run phase: processing".to_string(),
            dir_modified_at(dir).unwrap_or(0),
            None,
            None,
            None,
            None,
            None,
            Some("missing".to_string()),
        ),
    }
}

/// Get the modification time of a directory in unix seconds.
fn dir_modified_at(dir: &camino::Utf8Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(dir.as_std_path()).ok()?;
    let modified = meta.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Side-effect-free server check: verify config and programs without creating anything.
pub fn server_check(config: &ServerConfig) -> Result<()> {
    info!("checking server configuration");

    if config.gc.incoming_lease_secs == 0 {
        anyhow::bail!("gc.incoming_lease_secs must be greater than 0");
    }
    if config.gc.heartbeat_interval_secs == 0 {
        anyhow::bail!("gc.heartbeat_interval_secs must be greater than 0");
    }
    if config.gc.heartbeat_interval_secs > config.gc.incoming_lease_secs / 2 {
        anyhow::bail!(
            "gc.heartbeat_interval_secs ({}) must be <= half of gc.incoming_lease_secs ({}) \
             to provide a safety margin for lease renewal",
            config.gc.heartbeat_interval_secs,
            config.gc.incoming_lease_secs
        );
    }

    let root_path = config.root.as_path();
    if !root_path.exists() {
        anyhow::bail!(
            "root path '{}' does not exist (run `purgery-server bootstrap` to create it)",
            root_path.as_str()
        );
    }
    if !root_path.is_dir() {
        anyhow::bail!(
            "root path '{}' exists but is not a directory",
            root_path.as_str()
        );
    }
    info!(path = %root_path.as_str(), "root: OK");

    let purgery_path = config.purgery_root.as_path();
    if !purgery_path.exists() {
        anyhow::bail!(
            "purgery_root '{}' does not exist (run `purgery-server bootstrap` to create it)",
            purgery_path.as_str()
        );
    }
    if !purgery_path.is_dir() {
        anyhow::bail!(
            "purgery_root '{}' exists but is not a directory",
            purgery_path.as_str()
        );
    }
    info!(path = %purgery_path.as_str(), "purgery_root: OK");

    for (name, step) in &config.postprocess.steps {
        let program = &step.program;
        if program.is_empty() {
            anyhow::bail!("postprocess step '{}' has empty program", name);
        }

        if !step.keep_original && step.expected_outputs.is_empty() {
            anyhow::bail!(
                "postprocess step '{}': keep_original=false with no expected_outputs \
                 would produce zero committed outputs",
                name
            );
        }

        for output in &step.expected_outputs {
            purgery_core::validate_expected_output_name(output).map_err(|e| {
                anyhow::anyhow!("postprocess step '{name}': expected_output {output:?}: {e}")
            })?;
        }

        purgery_core::resolve_executable(program)
            .map(|r| info!(step = name, path = %r.path.as_str(), "postprocess program found"))?;
    }

    info!("server configuration: OK");
    Ok(())
}

/// Bootstrap: create root and purgery_root directories.
pub fn bootstrap(config: &ServerConfig) -> Result<()> {
    info!("bootstrapping server directories");

    let root_path = config.root.as_path();
    fs::create_dir_all(root_path.as_std_path())
        .with_context(|| format!("failed to create root: {}", root_path.as_str()))?;
    info!(path = %root_path.as_str(), "created root");

    let purgery_path = config.purgery_root.as_path();
    fs::create_dir_all(purgery_path.as_std_path())
        .with_context(|| format!("failed to create purgery_root: {}", purgery_path.as_str()))?;
    info!(path = %purgery_path.as_str(), "created purgery_root");

    info!("bootstrap complete");
    Ok(())
}

/// Heartbeat: update lease file for an incoming run.
pub fn heartbeat_run(config: &ServerConfig, nickname: &Nickname, run_id: &RunId) -> Result<()> {
    let incoming_path = config
        .purgery_root
        .run_dir(nickname, run_id, RunPhase::Incoming);
    if !incoming_path.exists() {
        anyhow::bail!(
            "run {}/{} is not in incoming phase",
            nickname.as_str(),
            run_id.as_str()
        );
    }

    let lease_path = incoming_path.join("lease.toml");
    let lease_content = fs::read_to_string(lease_path.as_std_path())
        .with_context(|| "failed to read lease file")?;
    let mut lease: purgery_core::LeaseFile =
        toml::from_str(&lease_content).with_context(|| "failed to parse lease file")?;

    if lease.protocol_version != 1 {
        anyhow::bail!(
            "lease protocol version {} does not match expected 1",
            lease.protocol_version
        );
    }
    if lease.nickname != nickname.as_str() {
        anyhow::bail!(
            "lease nickname '{}' does not match expected '{}'",
            lease.nickname,
            nickname.as_str()
        );
    }
    if lease.run_id != run_id.as_str() {
        anyhow::bail!(
            "lease run_id '{}' does not match expected '{}'",
            lease.run_id,
            run_id.as_str()
        );
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    lease.last_heartbeat_unix_secs = now;
    lease.expires_at_unix_secs = now + config.gc.incoming_lease_secs;

    let new_content = toml::to_string(&lease).with_context(|| "failed to serialize lease")?;
    let tmp_path = incoming_path.join("lease.toml.tmp");
    fs::write(tmp_path.as_std_path(), &new_content).with_context(|| "failed to write lease")?;
    fs::rename(tmp_path.as_std_path(), lease_path.as_std_path())
        .with_context(|| "failed to commit lease")?;

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
        ClientLocalPath, ManifestEntry, NormalizedRelativePath, PostprocessConfig, PostprocessKind,
        PostprocessStepDefinition, ServerRoot, SyncName,
    };

    /// Call apply_postprocessing with a no-op progress callback for testing.
    fn test_apply_postprocessing(
        run_plan: &RunPlan,
        sync_name: &str,
        normalized_path: &str,
        work_path: &Utf8Path,
    ) -> Result<Vec<Utf8PathBuf>, String> {
        apply_postprocessing(
            run_plan,
            sync_name,
            normalized_path,
            work_path,
            &mut |_: &purgery_core::ProgressUpdate| {},
            0,
            1,
            normalized_path,
        )
    }

    fn test_server_config(purgery_root: &Utf8Path, server_root: &Utf8Path) -> ServerConfig {
        fs::create_dir_all(server_root).unwrap();
        fs::create_dir_all(purgery_root).unwrap();
        ServerConfig {
            root: ServerRoot::new(server_root.to_owned()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.to_owned()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        }
    }

    fn write_run_toml(dir: &Utf8Path, nickname: &Nickname) {
        let content = format!(
            r#"nickname = "{}"
"#,
            nickname.as_str()
        );
        fs::write(dir.join("run.toml"), &content).unwrap();
    }

    fn write_run_toml_with_sync(
        dir: &Utf8Path,
        nickname: &Nickname,
        sync_name: &str,
        to_path: &str,
    ) {
        let content = format!(
            r#"nickname = "{}"

[[sync]]
name = "{}"
to = "{}"
delete_after_import = true
"#,
            nickname.as_str(),
            sync_name,
            to_path,
        );
        fs::write(dir.join("run.toml"), &content).unwrap();
    }

    /// Helper to create a basic setup with a ready run containing one file.
    #[allow(clippy::too_many_arguments)]
    fn setup_single_file_ready(
        purgery_root: &Utf8Path,
        server_root: &Utf8Path,
        nickname: &Nickname,
        run_id: &RunId,
        sync_name: &str,
        to_path: &str,
        staged_rel: &str,
        content: &[u8],
    ) -> (ServerConfig, Utf8PathBuf) {
        let config = test_server_config(purgery_root, server_root);
        let ready_path = config
            .purgery_root
            .run_dir(nickname, run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        let staged_path = ready_path.join(staged_rel);
        if let Some(parent) = staged_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&staged_path, content).unwrap();

        write_run_toml_with_sync(&ready_path, nickname, sync_name, to_path);

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new(sync_name.into()).unwrap(),
                local_path: ClientLocalPath::new(format!("/home/user/{sync_name}/{staged_rel}"))
                    .unwrap(),
                staged_path: NormalizedRelativePath::new(staged_rel.into()).unwrap(),
                relative_path: NormalizedRelativePath::new(
                    staged_rel
                        .rsplit_once('/')
                        .map(|(_, f)| f)
                        .unwrap_or(staged_rel)
                        .into(),
                )
                .unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: content.len() as u64,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        (config, staged_path)
    }

    // ── Core pipeline test ──

    #[test]
    fn test_full_processing_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-001".into()).unwrap();

        let (config, staged_file_path) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"hello world",
        );

        process_run(&config, &nickname, &run_id).unwrap();
        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done_path.exists());

        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(
            status.entries[0].final_paths,
            vec!["laptop/videos/test.mp4"],
            "single-output import must record one final path"
        );

        let final_path = server_root.join("laptop/videos/test.mp4");
        assert!(final_path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "hello world");
        assert!(!staged_file_path.exists());
    }

    #[test]
    fn test_processing_skips_unknown_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-002".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        // Run config has no sync mappings
        write_run_toml(&ready_path, &nickname);

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("unknown-sync".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Skipped);
    }

    #[test]
    fn test_processing_missing_staged_file() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-run-003".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        write_run_toml_with_sync(&ready_path, &nickname, "videos", "videos");

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/missing.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/missing.mp4".into())
                    .unwrap(),
                relative_path: NormalizedRelativePath::new("missing.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 11,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("failed to read staged metadata"));
    }

    #[test]
    fn test_rule_matching() {
        use purgery_core::rsync_pattern_match;
        // Unanchored patterns match at any position
        assert!(rsync_pattern_match("*.mp4", "videos/a.mp4"));
        assert!(rsync_pattern_match("*.mov", "videos/subdir/b.mov"));
        assert!(rsync_pattern_match("*.webm", "videos/c.webm"));
        assert!(rsync_pattern_match("*.mp3", "audio/song.mp3")); // unanchored matches at "song.mp3"
        assert!(!rsync_pattern_match("*.mp4", "videos/a.txt"));
        // Anchored patterns match from start of path
        assert!(rsync_pattern_match("/videos/*", "videos/a.mp4"));
        assert!(!rsync_pattern_match("/audio/*", "videos/a.mp4"));
        // ** patterns
        assert!(rsync_pattern_match("**/*.mp4", "videos/sub/a.mp4"));
        assert!(rsync_pattern_match("cache/**", "cache/sub/file.txt"));
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

    #[test]
    fn test_nickname_mismatch_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let dir_nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-env-001".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&dir_nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        // Run config has different nickname than the directory
        let run_config_content = r#"nickname = "other-machine""#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: Nickname::new("other-machine".into()).unwrap(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let result = process_run(&config, &dir_nickname, &run_id);
        assert!(result.is_err());

        let failed_path = config
            .purgery_root
            .run_dir(&dir_nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("envelope validation failed"));
    }

    #[test]
    fn test_bad_manifest_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-manifest".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        write_run_toml(&ready_path, &nickname);
        fs::write(ready_path.join("manifest.toml"), "not valid toml {{{").unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(result.is_err());

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("failed to parse manifest"));
    }

    #[test]
    fn test_bad_run_config_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-config".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(&ready_path).unwrap();

        fs::write(ready_path.join("run.toml"), "not valid toml {{{").unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
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
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(status.error.unwrap().contains("failed to parse run config"));
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
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".to_owned(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                    sync_names: None,
                }],
            },
        };

        let tmp = tempfile::tempdir().unwrap();
        let work_area = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let work_path = work_area.join("some file.mp4");
        fs::write(&work_path, b"test data").unwrap();

        let run_plan = RunPlan::build(&server_config, &run_config).unwrap();
        let results =
            test_apply_postprocessing(&run_plan, "videos", "videos/some file.mp4", &work_path);
        assert!(results.is_ok(), "postprocess with spaces should succeed");
        let outputs = results.unwrap();
        assert!(!outputs.is_empty());
        assert!(outputs.contains(&work_path));
    }

    #[test]
    fn test_postprocessing_failure_does_not_create_final_output() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let server_str = server_root.as_str();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_str.into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.as_str().into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "false".to_owned(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-fail-pp".into()).unwrap();

        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/test.mp4"), b"video content").unwrap();

        write_run_toml_with_sync(&ready_path, &nickname, "videos", "videos");
        let run_config_content = r#"nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#
        .to_string();
        fs::write(ready_path.join("run.toml"), &run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let failed_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0].error.as_ref().unwrap().contains("failed"));

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
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".to_owned(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*.mp4".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                    sync_names: None,
                }],
            },
        };

        let pp_run_plan = RunPlan::build(&server_config, &run_config).unwrap();
        let result =
            test_apply_postprocessing(&pp_run_plan, "videos", "videos/video.mp4", &work_path);
        assert!(result.is_ok());
        let outputs = result.unwrap();
        assert!(outputs.contains(&work_path));
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
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".to_owned(),
                            args: vec![],
                            expected_outputs: vec!["{stem}.Z.webm".into()],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                    sync_names: None,
                }],
            },
        };

        let pp_run_plan = RunPlan::build(&server_config, &run_config).unwrap();
        let result =
            test_apply_postprocessing(&pp_run_plan, "videos", "videos/video.mp4", &work_path);
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
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".to_owned(),
                            args: vec![],
                            expected_outputs: vec!["{stem}.Z.webm".into()],
                            keep_original: false,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*".to_owned(),
                    steps: vec!["compress-video".to_owned()],
                    sync_names: None,
                }],
            },
        };

        let pp_run_plan = RunPlan::build(&server_config, &run_config).unwrap();
        let result =
            test_apply_postprocessing(&pp_run_plan, "videos", "videos/video.mp4", &work_path);
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

    // ── Temp-file commit test ──

    #[test]
    fn test_temp_file_commit_no_direct_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-tmp-commit".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"hello",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let final_path = server_root.join("laptop/videos/test.mp4");
        assert!(final_path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "hello");

        let has_temp_files = std::fs::read_dir(final_path.parent().unwrap())
            .unwrap()
            .any(|e| {
                e.ok()
                    .and_then(|e| e.file_name().to_str().map(|s| s.to_owned()))
                    .map(|s| s.starts_with(".purgery-commit"))
                    .unwrap_or(false)
            });
        assert!(
            !has_temp_files,
            "temp files must be cleaned up after commit"
        );
    }

    // ── Atomic replacement tests ──

    #[test]
    fn test_existing_regular_final_output_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-replace".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"new content",
        );

        let final_path = server_root.join("laptop/videos/test.mp4");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"old content").unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "new content");
        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done_path.join("status.toml")).unwrap())
                .unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    #[test]
    fn test_regular_file_replaces_existing_empty_directory_like_rsync() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-directory-block".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"content",
        );
        let final_path = server_root.join("laptop/videos/test.mp4");
        fs::create_dir_all(&final_path).unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "content");
        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done_path.join("status.toml")).unwrap())
                .unwrap();
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    #[test]
    fn test_regular_file_replaces_existing_symlink_like_rsync() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-final-symlink".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "documents",
            "documents",
            "files/documents/a.txt",
            b"content",
        );
        let final_path = server_root.join("laptop/documents/a.txt");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("missing-target", &final_path).unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "content");
        assert!(!fs::symlink_metadata(&final_path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    // ── Work area namespacing test ──

    #[test]
    fn test_work_area_namespacing_no_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-ns".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::create_dir_all(ready_path.join("files/pictures")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"video content").unwrap();
        fs::write(ready_path.join("files/pictures/a.mp4"), b"picture content").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[sync]]
name = "pictures"
to = "pictures"
delete_after_import = true
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("videos".into()).unwrap(),
                    local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 13,
                    mtime_ns: 1000000,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("pictures".into()).unwrap(),
                    local_path: ClientLocalPath::new("/home/user/Pictures/a.mp4".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/pictures/a.mp4".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 15,
                    mtime_ns: 1000001,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let video_final = server_root.join("laptop/videos/a.mp4");
        let picture_final = server_root.join("laptop/pictures/a.mp4");
        assert!(video_final.exists());
        assert!(picture_final.exists());
        assert_eq!(fs::read_to_string(&video_final).unwrap(), "video content");
        assert_eq!(
            fs::read_to_string(&picture_final).unwrap(),
            "picture content"
        );

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[1].status, FileStatus::Imported);
    }

    // ── Staged path mismatch test ──

    #[test]
    fn test_manifest_staged_path_mismatch_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-sp-mismatch".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"content").unwrap();

        write_run_toml_with_sync(&ready_path, &nickname, "videos", "videos");

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/other/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 7,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("staged_path mismatch"));
    }

    #[test]
    fn test_manifest_staged_path_match_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-sp-match".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/a.mp4",
            b"content",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let done_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
    }

    // ── Staged symlink rejection test ──

    #[test]
    fn test_staged_symlink_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-symlink".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();

        let real_file = ready_path.join("files/videos/real.mp4");
        fs::write(&real_file, b"real content").unwrap();
        let staged_link = ready_path.join("files/videos/a.mp4");
        std::os::unix::fs::symlink(&real_file, &staged_link).unwrap();

        write_run_toml_with_sync(&ready_path, &nickname, "videos", "videos");

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 12,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&config, &nickname, &run_id).unwrap();

        let final_path = server_root.join("laptop/videos/a.mp4");
        assert!(
            !final_path.exists(),
            "symlink must not be imported to final path"
        );

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status_content = fs::read_to_string(failed_path.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(status.entries[0].status, FileStatus::Failed);
        assert!(status.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("kind does not match"));
    }

    // ── Invalid regex test ──

    #[test]
    fn test_empty_postprocess_pattern_produces_failed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-bad-pattern".into()).unwrap();

        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/a.mp4"), b"content").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = ""
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let result = process_run(&config, &nickname, &run_id);
        assert!(result.is_err(), "process_run must error on empty pattern");

        let failed_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed_path.exists());
        let status_path = failed_path.join("status.toml");
        assert!(status_path.exists());
        let status_content = fs::read_to_string(&status_path).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert!(
            status.error.as_deref().unwrap().contains("pattern")
                || status.error.as_deref().unwrap().contains("invalid")
        );
    }

    // ── Work area cleanup tests ──

    #[test]
    fn test_run_state_done_removes_work_area() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-done-wa".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/a.mp4",
            b"hello",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let work_area = purgery_core::work_dir(&config.purgery_root, &nickname, &run_id);
        assert!(!work_area.exists(), "work area must be removed on Done");
    }

    #[test]
    fn test_run_state_partial_keeps_work_area() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_root.as_str().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.as_str().into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "false".to_owned(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-partial-wa".into()).unwrap();

        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/test.mp4"), b"video content").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/test.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 13,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        process_run(&server_config, &nickname, &run_id).unwrap();

        let failed_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed_path.exists());
        // The work area lives inside the run's processing directory, so it moves
        // with the run to the failed directory when the run is finalized.
        let work_area_after_move = failed_path.join("work");
        assert!(
            work_area_after_move.exists(),
            "work area must be kept for Failed state"
        );
    }

    // ── compress-video keep_original end-to-end ──

    #[test]
    fn test_compress_video_keep_original_records_both_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();

        let script_path = tmp.path().join("compress.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nbase=$(basename \"$2\");stem=\"${base%.*}\";dir=$(dirname \"$2\");touch \"$dir/$stem.Z.webm\"\n",
        ).unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_root.as_str().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.as_str().into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: script_path.to_string_lossy().to_string(),
                            args: vec!["--input".into(), "{input}".into()],
                            expected_outputs: vec!["{stem}.Z.webm".into()],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-both".into()).unwrap();

        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/video.mp4"), b"video").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/video.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/video.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("video.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
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
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[0].final_paths.len(), 2);

        let original_final = server_root.join("laptop/videos/video.mp4");
        let compressed_final = server_root.join("laptop/videos/video.Z.webm");
        assert!(original_final.exists());
        assert!(compressed_final.exists());
    }

    #[test]
    fn test_compress_video_keep_original_false_records_one_path() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();

        let script_path = tmp.path().join("compress.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nbase=$(basename \"$2\");stem=\"${base%.*}\";dir=$(dirname \"$2\");touch \"$dir/$stem.Z.webm\"\n",
        ).unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new(server_root.as_str().into()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.as_str().into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress-video".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: script_path.to_string_lossy().to_string(),
                            args: vec!["--input".into(), "{input}".into()],
                            expected_outputs: vec!["{stem}.Z.webm".into()],
                            keep_original: false,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-pp-comp-only".into()).unwrap();

        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/videos")).unwrap();
        fs::write(ready_path.join("files/videos/video.mp4"), b"video").unwrap();

        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["compress-video"]
"#;
        fs::write(ready_path.join("run.toml"), run_config_content).unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/home/user/Videos/video.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/video.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("video.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 1000000,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
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
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert_eq!(status.entries[0].final_paths.len(), 1);

        let original_final = server_root.join("laptop/videos/video.mp4");
        let compressed_final = server_root.join("laptop/videos/video.Z.webm");
        assert!(
            !original_final.exists(),
            "original must NOT exist with keep_original=false"
        );
        assert!(compressed_final.exists());
    }

    // ── Run Plan tests ──

    #[test]
    fn test_run_plan_validates_empty_pattern() {
        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "".into(),
                    steps: vec!["compress-video".into()],
                    sync_names: None,
                }],
            },
        };
        let result = RunPlan::build(&server_config, &run_config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty pattern"));
    }

    #[test]
    fn test_run_plan_validates_step_references() {
        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "videos/*".into(),
                    steps: vec!["nonexistent-step".into()],
                    sync_names: None,
                }],
            },
        };
        let result = RunPlan::build(&server_config, &run_config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not defined on server"));
    }

    // ── begin_run / finish_run tests ──

    #[test]
    fn test_begin_run_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-begin".into()).unwrap();

        let response_str = begin_run(&server_config, &nickname, &run_id).unwrap();
        let response: purgery_core::BeginRunResponse = toml::from_str(&response_str).unwrap();
        assert_eq!(response.protocol_version, 1);
        assert_eq!(response.nickname, "laptop");
        assert_eq!(response.run_id, "test-begin");

        let incoming_path = Utf8Path::new(&response.incoming_dir);
        assert!(incoming_path.exists(), "incoming dir must exist");
        assert!(
            Utf8Path::new(&response.files_dir).exists(),
            "files dir must exist"
        );
    }

    #[test]
    fn test_finish_run_moves_from_incoming_to_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-finish".into()).unwrap();

        // Begin the run
        begin_run(&server_config, &nickname, &run_id).unwrap();

        let incoming_path =
            server_config
                .purgery_root
                .run_dir(&nickname, &run_id, RunPhase::Incoming);
        assert!(incoming_path.exists());

        // Finish it
        finish_run(&server_config, &nickname, &run_id).unwrap();

        assert!(
            !incoming_path.exists(),
            "incoming must be gone after finish"
        );
        let ready_path = server_config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        assert!(ready_path.exists(), "ready dir must exist after finish");
    }

    #[test]
    fn test_read_run_status_from_done() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-status".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/a.mp4",
            b"data",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let status = read_run_status(&config, &nickname, &run_id).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.nickname, nickname);
        assert_eq!(status.run_id, run_id);
    }

    #[test]
    fn test_read_run_status_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("nonexistent".into()).unwrap();

        let result = read_run_status(&server_config, &nickname, &run_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_finish_run_rejects_expired_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-expired-lease".into()).unwrap();

        begin_run(&server_config, &nickname, &run_id).unwrap();

        let incoming_path =
            server_config
                .purgery_root
                .run_dir(&nickname, &run_id, RunPhase::Incoming);
        let lease_path = incoming_path.join("lease.toml");
        let mut lease: purgery_core::LeaseFile =
            toml::from_str(&fs::read_to_string(&lease_path).unwrap()).unwrap();
        lease.expires_at_unix_secs = 0;
        fs::write(&lease_path, toml::to_string(&lease).unwrap()).unwrap();

        let result = finish_run(&server_config, &nickname, &run_id);
        assert!(result.is_err(), "finish-run must reject expired lease");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expired") || err.contains("lease"),
            "error: {err}"
        );
    }

    #[test]
    fn test_finish_run_rejects_mismatched_lease_nickname() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-wrong-nickname".into()).unwrap();

        begin_run(&server_config, &nickname, &run_id).unwrap();

        let incoming_path =
            server_config
                .purgery_root
                .run_dir(&nickname, &run_id, RunPhase::Incoming);
        let lease_path = incoming_path.join("lease.toml");
        let mut lease: purgery_core::LeaseFile =
            toml::from_str(&fs::read_to_string(&lease_path).unwrap()).unwrap();
        lease.nickname = "wrong-machine".into();
        fs::write(&lease_path, toml::to_string(&lease).unwrap()).unwrap();

        let result = finish_run(&server_config, &nickname, &run_id);
        assert!(
            result.is_err(),
            "finish-run must reject mismatched nickname"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nickname"), "error: {err}");
    }

    #[test]
    fn test_process_once_processes_ready_run_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ready-after-restart".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "documents",
            "documents",
            "files/documents/a.txt",
            b"ready",
        );

        process_once_raw(&config).unwrap();

        assert!(config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done)
            .exists());
        assert_eq!(
            fs::read_to_string(server_root.join("laptop/documents/a.txt")).unwrap(),
            "ready"
        );
    }

    #[test]
    fn test_process_once_recovers_processing_run_without_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-interrupted".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "documents",
            "documents",
            "files/documents/a.txt",
            b"hello",
        );
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.parent().unwrap()).unwrap();
        fs::rename(&ready, &processing).unwrap();
        let stale_work = work_dir(&config.purgery_root, &nickname, &run_id);
        fs::create_dir_all(&stale_work).unwrap();
        fs::write(stale_work.join("stale"), b"stale").unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        assert!(done.join("status.toml").exists());
        assert_eq!(
            fs::read_to_string(server_root.join("laptop/documents/a.txt")).unwrap(),
            "hello"
        );
        assert!(!stale_work.exists());
    }

    #[test]
    fn test_process_once_finalizes_processing_run_with_valid_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-status".into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        let status = RunStatus {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            state: RunState::Partial,
            entries: vec![],
            error: None,
        };
        fs::write(processing.join("status.toml"), status.to_toml().unwrap()).unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        assert!(config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done)
            .exists());
    }

    fn assert_mismatched_processing_status_fails(
        status_nickname: Nickname,
        status_run_id: RunId,
        directory_run_id: &str,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new(directory_run_id.into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        let status = RunStatus {
            run_id: status_run_id,
            nickname: status_nickname,
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(processing.join("status.toml"), status.to_toml().unwrap()).unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        assert!(!config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done)
            .exists());
        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let failed_status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert_eq!(failed_status.nickname, nickname);
        assert_eq!(failed_status.run_id, run_id);
        assert_eq!(failed_status.state, RunState::Failed);
        assert_eq!(
            failed_status.error.as_deref(),
            Some("interrupted processing had mismatched status envelope")
        );
    }

    #[test]
    fn test_process_once_fails_processing_run_with_mismatched_status_nickname() {
        assert_mismatched_processing_status_fails(
            Nickname::new("other-machine".into()).unwrap(),
            RunId::new("recover-wrong-nickname".into()).unwrap(),
            "recover-wrong-nickname",
        );
    }

    #[test]
    fn test_process_once_fails_processing_run_with_mismatched_status_run_id() {
        assert_mismatched_processing_status_fails(
            Nickname::new("laptop".into()).unwrap(),
            RunId::new("other-run".into()).unwrap(),
            "recover-wrong-run-id",
        );
    }

    #[test]
    fn test_mismatched_status_recovery_propagates_terminal_move_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("blocked-failed-move".into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(&failed).unwrap();
        fs::write(failed.join("existing"), b"occupied").unwrap();
        let mismatched_status = RunStatus {
            run_id: RunId::new("other-run".into()).unwrap(),
            nickname: nickname.clone(),
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(
            processing.join("status.toml"),
            mismatched_status.to_toml().unwrap(),
        )
        .unwrap();

        let error = recover_or_process_processing_run(&config, &nickname, &run_id).unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to move run-level failure to failed"));
        assert!(processing.exists());
        let status =
            RunStatus::from_toml(&fs::read_to_string(processing.join("status.toml")).unwrap())
                .unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("interrupted processing had mismatched status envelope")
        );
    }

    #[test]
    fn test_malformed_status_recovery_propagates_failed_status_write_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("blocked-status-write".into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.join("status.toml.tmp")).unwrap();
        fs::write(processing.join("status.toml"), "not valid = [toml").unwrap();

        let error = recover_or_process_processing_run(&config, &nickname, &run_id).unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to write temporary run failure status"));
        assert!(processing.exists());
        assert_eq!(
            fs::read_to_string(processing.join("status.toml")).unwrap(),
            "not valid = [toml"
        );
    }

    #[test]
    fn test_process_once_fails_processing_run_with_malformed_status() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-malformed".into()).unwrap();
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(&processing).unwrap();
        fs::write(processing.join("status.toml"), "not valid = [toml").unwrap();

        process_once_raw(&config).unwrap();

        assert!(!processing.exists());
        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("interrupted processing had malformed status")
        );
    }

    #[test]
    fn test_replay_after_final_replacement_without_status_converges() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("recover-committed-output".into()).unwrap();
        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "documents",
            "documents",
            "files/documents/a.txt",
            b"new",
        );
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        let processing = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing.parent().unwrap()).unwrap();
        fs::rename(&ready, &processing).unwrap();
        let final_path = server_root.join("laptop/documents/a.txt");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"new").unwrap();
        assert!(!processing.join("status.toml").exists());

        process_once_raw(&config).unwrap();

        assert_eq!(fs::read_to_string(&final_path).unwrap(), "new");
        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Done);
    }

    #[test]
    fn test_repeated_imports_same_destination_are_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();

        for (run, content) in [("repeat-1", b"hello".as_slice()), ("repeat-2", b"hello")] {
            let run_id = RunId::new(run.into()).unwrap();
            let (config, _) = setup_single_file_ready(
                &purgery_root,
                &server_root,
                &nickname,
                &run_id,
                "documents",
                "documents",
                "files/documents/a.txt",
                content,
            );
            process_run(&config, &nickname, &run_id).unwrap();
            assert!(config
                .purgery_root
                .run_dir(&nickname, &run_id, RunPhase::Done)
                .exists());
        }

        assert_eq!(
            fs::read_to_string(server_root.join("laptop/documents/a.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn test_repeated_import_replaces_changed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();

        for (run, content) in [("version-1", b"v1".as_slice()), ("version-2", b"v2")] {
            let run_id = RunId::new(run.into()).unwrap();
            let (config, _) = setup_single_file_ready(
                &purgery_root,
                &server_root,
                &nickname,
                &run_id,
                "documents",
                "documents",
                "files/documents/a.txt",
                content,
            );
            process_run(&config, &nickname, &run_id).unwrap();
        }

        assert_eq!(
            fs::read_to_string(server_root.join("laptop/documents/a.txt")).unwrap(),
            "v2"
        );
    }

    #[test]
    fn test_gc_collects_abandoned_incoming_upload() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("abandoned-upload".into()).unwrap();
        begin_run(&config, &nickname, &run_id).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::write(incoming.join("files/partial.txt"), b"partial").unwrap();
        let lease_path = incoming.join("lease.toml");
        let mut lease: purgery_core::LeaseFile =
            toml::from_str(&fs::read_to_string(&lease_path).unwrap()).unwrap();
        lease.expires_at_unix_secs = 0;
        fs::write(&lease_path, toml::to_string(&lease).unwrap()).unwrap();

        run_gc(&config).unwrap();

        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(!failed.join("files").exists());
        let status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Failed);
    }

    /// begin-run output must be parseable as BeginRunResponse TOML.
    /// This is a stdout-clean invariant: protocol output must never be
    /// contaminated by log output, and the returned string must always
    /// be valid TOML regardless of logging configuration.
    #[test]
    fn test_begin_run_stdout_is_parseable_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("storage");
        let server_config = ServerConfig {
            root: ServerRoot::new(Utf8PathBuf::from_path_buf(root_path).unwrap()).unwrap(),
            purgery_root: PurgeryRoot::new(
                Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            )
            .unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig::default(),
            logging: Default::default(),
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-stdout-begin".into()).unwrap();

        let response_str = begin_run(&server_config, &nickname, &run_id).unwrap();
        // Must parse as BeginRunResponse — if logging contaminated stdout,
        // this parse would fail.
        let response: purgery_core::BeginRunResponse = toml::from_str(&response_str)
            .expect("begin-run stdout must be valid BeginRunResponse TOML");
        assert_eq!(response.protocol_version, 1);
        assert_eq!(response.nickname, "laptop");
        assert_eq!(response.run_id, "test-stdout-begin");
    }

    /// status output must be parseable as RunStatus TOML.
    #[test]
    fn test_status_stdout_is_parseable_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("test-stdout-status".into()).unwrap();

        let (config, _) = setup_single_file_ready(
            &purgery_root,
            &server_root,
            &nickname,
            &run_id,
            "videos",
            "videos",
            "files/videos/test.mp4",
            b"hello",
        );

        process_run(&config, &nickname, &run_id).unwrap();

        let status = read_run_status(&config, &nickname, &run_id).unwrap();
        let status_str = status.to_toml().unwrap();
        // Must parse back as RunStatus
        let parsed: purgery_core::RunStatus = purgery_core::RunStatus::from_toml(&status_str)
            .expect("status stdout must be valid RunStatus TOML");
        assert_eq!(parsed.state, purgery_core::RunState::Done);
    }

    #[test]
    fn test_rsync_oracle_directory_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();

        let missing = root.join("missing");
        assert_eq!(
            commit_directory_entry(&missing, &root).unwrap(),
            CommitDisposition::Created
        );

        let existing = root.join("existing");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("extra"), "keep").unwrap();
        assert_eq!(
            commit_directory_entry(&existing, &root).unwrap(),
            CommitDisposition::Kept
        );
        assert_eq!(fs::read_to_string(existing.join("extra")).unwrap(), "keep");

        let file = root.join("file");
        fs::write(&file, "old").unwrap();
        assert_eq!(
            commit_directory_entry(&file, &root).unwrap(),
            CommitDisposition::Replaced
        );
        assert!(file.is_dir());

        let symlink = root.join("symlink");
        std::os::unix::fs::symlink("elsewhere", &symlink).unwrap();
        assert_eq!(
            commit_directory_entry(&symlink, &root).unwrap(),
            CommitDisposition::Replaced
        );
        assert!(symlink.is_dir());
    }

    #[test]
    fn test_rsync_oracle_regular_file_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let source = Utf8PathBuf::from_path_buf(tmp.path().join("source")).unwrap();
        fs::write(&source, "new content").unwrap();
        let run_id = RunId::new("oracle-file".into()).unwrap();

        for name in ["missing", "file", "symlink", "empty-dir"] {
            let destination = root.join(name);
            match name {
                "file" => fs::write(&destination, "old").unwrap(),
                "symlink" => std::os::unix::fs::symlink("target", &destination).unwrap(),
                "empty-dir" => fs::create_dir(&destination).unwrap(),
                _ => {}
            }
            commit_regular_file_entry(&source, &destination, &root, &run_id).unwrap();
            assert_eq!(fs::read_to_string(&destination).unwrap(), "new content");
            assert!(!fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink());
        }

        let nonempty = root.join("nonempty-dir");
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("extra"), "keep").unwrap();
        assert!(commit_regular_file_entry(&source, &nonempty, &root, &run_id).is_err());
        assert_eq!(fs::read_to_string(nonempty.join("extra")).unwrap(), "keep");
    }

    #[test]
    fn test_rsync_oracle_symlink_conflicts_and_literal_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let run_id = RunId::new("oracle-link".into()).unwrap();
        let target = Utf8Path::new("../literal-target");

        for name in ["missing", "file", "symlink", "empty-dir"] {
            let destination = root.join(name);
            match name {
                "file" => fs::write(&destination, "old").unwrap(),
                "symlink" => std::os::unix::fs::symlink("old-target", &destination).unwrap(),
                "empty-dir" => fs::create_dir(&destination).unwrap(),
                _ => {}
            }
            commit_symlink_entry(target, &destination, &root, &run_id).unwrap();
            assert_eq!(fs::read_link(&destination).unwrap(), target.as_std_path());
        }

        let nonempty = root.join("nonempty-dir");
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("extra"), "keep").unwrap();
        assert!(commit_symlink_entry(target, &nonempty, &root, &run_id).is_err());
        assert_eq!(fs::read_to_string(nonempty.join("extra")).unwrap(), "keep");
    }

    #[test]
    fn test_rsync_oracle_parent_conflicts_are_resolved_by_directory_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("root")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let source = Utf8PathBuf::from_path_buf(tmp.path().join("source")).unwrap();
        fs::write(&source, "child").unwrap();
        let run_id = RunId::new("oracle-parent".into()).unwrap();

        for name in ["file-parent", "symlink-parent"] {
            let parent = root.join(name);
            if name == "file-parent" {
                fs::write(&parent, "old").unwrap();
            } else {
                std::os::unix::fs::symlink("elsewhere", &parent).unwrap();
            }
            commit_directory_entry(&parent, &root).unwrap();
            let child = parent.join("child");
            commit_regular_file_entry(&source, &child, &root, &run_id).unwrap();
            assert_eq!(fs::read_to_string(child).unwrap(), "child");
        }
    }

    #[test]
    fn test_process_run_overlays_directory_file_and_symlink_without_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("tree-overlay".into()).unwrap();
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        let staged = ready.join("files/data/tree");
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("new.txt"), "new").unwrap();
        std::os::unix::fs::symlink("../target", staged.join("link")).unwrap();
        write_run_toml_with_sync(&ready, &nickname, "data", "data");

        let entry = |relative: &str, kind, size, target: Option<&str>| ManifestEntry {
            sync_name: SyncName::new("data".into()).unwrap(),
            local_path: ClientLocalPath::new(format!("/source/{relative}")).unwrap(),
            staged_path: NormalizedRelativePath::new(format!("files/data/{relative}").into())
                .unwrap(),
            relative_path: NormalizedRelativePath::new(relative.into()).unwrap(),
            kind,
            size,
            mtime_ns: 0,
            sha256: None,
            link_target: target.map(Utf8PathBuf::from),
            mode: Default::default(),
            postprocess_steps: Vec::new(),
            covered_by: None,
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                entry("tree", ManifestEntryKind::Directory, 0, None),
                entry(
                    "tree/link",
                    ManifestEntryKind::Symlink,
                    0,
                    Some("../target"),
                ),
                entry("tree/new.txt", ManifestEntryKind::RegularFile, 3, None),
            ],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let final_tree = server_root.join("laptop/data/tree");
        fs::create_dir_all(&final_tree).unwrap();
        fs::write(final_tree.join("extra.txt"), "keep").unwrap();
        process_run(&config, &nickname, &run_id).unwrap();

        assert_eq!(
            fs::read_to_string(final_tree.join("new.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(final_tree.join("extra.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::read_link(final_tree.join("link")).unwrap(),
            std::path::Path::new("../target")
        );
        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status =
            RunStatus::from_toml(&fs::read_to_string(done.join("status.toml")).unwrap()).unwrap();
        assert_eq!(status.state, RunState::Done);
        assert_eq!(status.entries.len(), 3);
        assert_eq!(status.entries[0].kind, ManifestEntryKind::Directory);
        assert_eq!(status.entries[1].kind, ManifestEntryKind::Symlink);
        assert_eq!(status.entries[2].kind, ManifestEntryKind::RegularFile);
    }

    #[test]
    fn test_read_run_status_rejects_mismatched_terminal_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("requested".into()).unwrap();
        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        fs::create_dir_all(&done).unwrap();
        let status = RunStatus {
            run_id: RunId::new("different".into()).unwrap(),
            nickname: nickname.clone(),
            state: RunState::Done,
            entries: vec![],
            error: None,
        };
        fs::write(done.join("status.toml"), status.to_toml().unwrap()).unwrap();

        let error = read_run_status(&config, &nickname, &run_id).unwrap_err();
        assert!(error.to_string().contains("status envelope mismatch"));
    }

    fn expected_output_test_plan() -> RunPlan {
        RunPlan {
            rules: vec![CompiledRule {
                pattern: "data/*.txt".into(),
                sync_names: None,
                steps: vec![ResolvedStep {
                    step_name: "generate".into(),
                    step_def: PostprocessStepDefinition {
                        kind: PostprocessKind::Subprocess,
                        program: "true".into(),
                        args: vec![],
                        expected_outputs: vec!["{stem}.out".into()],
                        keep_original: false,
                    },
                }],
            }],
        }
    }

    #[test]
    fn postprocess_regular_expected_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        fs::write(work_path.with_file_name("input.out"), "output").unwrap();

        let outputs = test_apply_postprocessing(
            &expected_output_test_plan(),
            "data",
            "data/input.txt",
            &work_path,
        )
        .unwrap();
        assert_eq!(outputs, vec![work_path.with_file_name("input.out")]);
    }

    #[test]
    fn postprocess_missing_expected_output_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();

        let error = test_apply_postprocessing(
            &expected_output_test_plan(),
            "data",
            "data/input.txt",
            &work_path,
        )
        .unwrap_err();
        assert!(error.contains("expected output not found"));
    }

    #[test]
    fn postprocess_symlink_expected_output_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        let target = work_path.with_file_name("target.txt");
        fs::write(&target, "secret target contents").unwrap();
        // Place a symlink to the target as the expected output.  The symlink
        // itself must be accepted — Purgery must not follow or reject it.
        std::os::unix::fs::symlink(&target, work_path.with_file_name("input.out")).unwrap();

        let outputs = test_apply_postprocessing(
            &expected_output_test_plan(),
            "data",
            "data/input.txt",
            &work_path,
        )
        .unwrap();
        assert!(
            outputs.contains(&work_path.with_file_name("input.out")),
            "symlink expected output must be accepted"
        );
        // The symlink must still point to the original target (not be
        // replaced by the target's content).
        let link = fs::read_link(work_path.with_file_name("input.out")).unwrap();
        assert_eq!(
            link,
            target.as_std_path(),
            "symlink target must be preserved"
        );
    }

    #[test]
    fn postprocess_directory_expected_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        fs::create_dir(work_path.with_file_name("input.out")).unwrap();

        let outputs = test_apply_postprocessing(
            &expected_output_test_plan(),
            "data",
            "data/input.txt",
            &work_path,
        )
        .unwrap();
        assert!(outputs.contains(&work_path.with_file_name("input.out")));
    }

    #[test]
    fn postprocess_symlink_expected_output_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        std::os::unix::fs::symlink("some-target", work_path.with_file_name("input.out")).unwrap();

        let outputs = test_apply_postprocessing(
            &expected_output_test_plan(),
            "data",
            "data/input.txt",
            &work_path,
        )
        .unwrap();
        assert!(outputs.contains(&work_path.with_file_name("input.out")));
    }

    #[test]
    fn postprocess_fifo_expected_output_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, "input").unwrap();
        // Create a FIFO (named pipe)
        std::process::Command::new("mkfifo")
            .arg(work_path.with_file_name("input.out").as_std_path())
            .status()
            .unwrap();

        let error = test_apply_postprocessing(
            &expected_output_test_plan(),
            "data",
            "data/input.txt",
            &work_path,
        )
        .unwrap_err();
        assert!(error.contains("expected output is not a supported entry type"));
    }

    fn duplicate_path_test_entry(
        sync_name: &str,
        relative_path: &str,
        kind: ManifestEntryKind,
    ) -> ManifestEntry {
        ManifestEntry {
            sync_name: SyncName::new(sync_name.into()).unwrap(),
            local_path: ClientLocalPath::new(format!("/source/{sync_name}/{relative_path}"))
                .unwrap(),
            staged_path: NormalizedRelativePath::new(
                format!("files/{sync_name}/{relative_path}").into(),
            )
            .unwrap(),
            relative_path: NormalizedRelativePath::new(relative_path.into()).unwrap(),
            kind,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            mode: purgery_core::ManifestEntryMode::Postprocess,
            postprocess_steps: Vec::new(),
            covered_by: None,
        }
    }

    fn duplicate_path_run_config(first_to: &str, second_to: &str) -> RunConfig {
        RunConfig::from_toml(&format!(
            r#"
nickname = "laptop"

[[sync]]
name = "first"
to = "{first_to}"

[[sync]]
name = "second"
to = "{second_to}"
"#,
        ))
        .unwrap()
    }

    #[test]
    fn duplicate_final_file_paths_across_syncs_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = duplicate_path_run_config("shared", "shared");
        let manifest = Manifest {
            run_id: RunId::new("duplicate-files".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                duplicate_path_test_entry("first", "same.txt", ManifestEntryKind::RegularFile),
                duplicate_path_test_entry("second", "same.txt", ManifestEntryKind::RegularFile),
            ],
        };

        let empty_plan = RunPlan { rules: vec![] };
        let empty_covered = std::collections::HashSet::new();
        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &empty_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(error.contains("duplicate final path"));
    }

    #[test]
    fn identical_relative_paths_under_different_destinations_are_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = duplicate_path_run_config("first-dest", "second-dest");
        let empty_plan = RunPlan { rules: vec![] };
        let empty_covered = std::collections::HashSet::new();
        let manifest = Manifest {
            run_id: RunId::new("distinct-files".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                duplicate_path_test_entry("first", "same.txt", ManifestEntryKind::RegularFile),
                duplicate_path_test_entry("second", "same.txt", ManifestEntryKind::RegularFile),
            ],
        };

        validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &empty_plan,
            &empty_covered,
        )
        .unwrap();
    }

    #[test]
    fn duplicate_final_directory_paths_across_syncs_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = duplicate_path_run_config("shared", "shared");
        let empty_plan = RunPlan { rules: vec![] };
        let empty_covered = std::collections::HashSet::new();
        let manifest = Manifest {
            run_id: RunId::new("duplicate-directories".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                duplicate_path_test_entry("first", "same-dir", ManifestEntryKind::Directory),
                duplicate_path_test_entry("second", "same-dir", ManifestEntryKind::Directory),
            ],
        };

        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &empty_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(error.contains("duplicate final path"));
    }

    #[test]
    fn processing_rejects_duplicate_final_paths_before_importing_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("duplicate-run".into()).unwrap();
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready.join("files/shared")).unwrap();
        fs::write(ready.join("files/shared/same.txt"), "staged").unwrap();
        fs::write(
            ready.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "first"
to = "shared"
delete_after_import = true

[[sync]]
name = "second"
to = "shared"
delete_after_import = true
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("first".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/first/same.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/shared/same.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("same.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 6,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("second".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/second/same.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/shared/same.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("same.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 6,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        assert!(process_run(&config, &nickname, &run_id).is_err());
        assert!(!server_root.join("laptop/shared/same.txt").exists());
        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        let status =
            RunStatus::from_toml(&fs::read_to_string(failed.join("status.toml")).unwrap()).unwrap();
        assert!(status.entries.is_empty());
        assert!(status
            .error
            .as_deref()
            .unwrap()
            .contains("duplicate final path"));
    }

    // ── Postprocess-derived duplicate final path tests ──

    #[test]
    fn postprocessed_directory_does_not_cause_false_overlap_rejection() {
        // Postprocessed directory + descendant file must not trigger a false
        // overlap validation failure.  The descendant should be skipped as
        // covered, not rejected as a planned-path conflict.
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("dir-transform".into()).unwrap();
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);

        // Create staged directory with child file
        fs::create_dir_all(ready.join("files/data/photos")).unwrap();
        fs::write(ready.join("files/data/photos/photo.txt"), "content").unwrap();

        // Run config with a postprocess rule that matches the directory
        let run_config_src = r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["pack"]
"#;
        fs::write(ready.join("run.toml"), run_config_src).unwrap();
        // Server config with a matching step
        let config = ServerConfig {
            root: ServerRoot::new(server_root.clone()).unwrap(),
            purgery_root: PurgeryRoot::new(purgery_root.clone()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/photos".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/photos".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("photos".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/photos/photo.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/photos/photo.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("photos/photo.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 7,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        // This must succeed — no false overlap rejection.
        assert!(
            process_run(&config, &nickname, &run_id).is_ok(),
            "postprocessed directory with descendant must not be rejected by overlap validation"
        );

        // The descendant should be skipped, not imported independently.
        let status = read_run_status(&config, &nickname, &run_id).unwrap();
        assert_eq!(status.entries.len(), 2);
        let dir_entry = &status.entries[0];
        let child_entry = &status.entries[1];
        assert_eq!(dir_entry.kind, ManifestEntryKind::Directory);
        assert_eq!(dir_entry.status, FileStatus::Imported);
        assert_eq!(child_entry.status, FileStatus::Skipped);
        assert!(
            child_entry
                .error
                .as_deref()
                .unwrap()
                .contains("covered by postprocessed ancestor"),
            "child must be skipped: {:?}",
            child_entry.error
        );
    }

    fn postprocess_collision_run_config() -> RunConfig {
        RunConfig::from_toml(
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "*.txt"
steps = ["compress"]
"#,
        )
        .unwrap()
    }

    fn postprocess_collision_run_plan() -> RunPlan {
        RunPlan {
            rules: vec![CompiledRule {
                pattern: "*.txt".into(),
                sync_names: None,
                steps: vec![ResolvedStep {
                    step_name: "compress".into(),
                    step_def: PostprocessStepDefinition {
                        kind: PostprocessKind::Subprocess,
                        program: "true".into(),
                        args: vec![],
                        expected_outputs: vec!["{stem}.Z.webm".into()],
                        keep_original: true,
                    },
                }],
            }],
        }
    }

    #[test]
    fn postprocess_output_collides_with_manifest_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = postprocess_collision_run_config();
        let run_plan = postprocess_collision_run_plan();

        let manifest = Manifest {
            run_id: RunId::new("pp-collision".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                // document.txt → postprocess (keep_original) produces document.txt + document.Z.webm
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/document.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/document.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("document.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                // document.Z.webm — would collide with the postprocess output above
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/document.Z.webm".into())
                        .unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/document.Z.webm".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("document.Z.webm".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 200,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };

        let empty_covered = std::collections::HashSet::new();
        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &run_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(
            error.contains("duplicate final path"),
            "error must mention duplicate final path: {error}"
        );
        assert!(
            error.contains("document.Z.webm"),
            "error must mention the colliding filename: {error}"
        );
    }

    #[test]
    fn postprocess_output_from_two_entries_collides() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = RunConfig::from_toml(
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "*.txt"
steps = ["compress"]
"#,
        )
        .unwrap();

        let pp_plan = RunPlan {
            rules: vec![CompiledRule {
                pattern: "*.txt".into(),
                sync_names: None,
                steps: vec![ResolvedStep {
                    step_name: "generate".into(),
                    step_def: PostprocessStepDefinition {
                        kind: PostprocessKind::Subprocess,
                        program: "true".into(),
                        args: vec![],
                        expected_outputs: vec!["result.bin".into()],
                        keep_original: false,
                    },
                }],
            }],
        };

        let manifest = Manifest {
            run_id: RunId::new("pp-cross-entry".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/a.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/a.txt".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("a.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 50,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/b.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/b.txt".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("b.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 60,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };

        let empty_covered = std::collections::HashSet::new();
        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &pp_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(
            error.contains("duplicate final path"),
            "error must mention duplicate final path: {error}"
        );
    }

    #[test]
    fn postprocess_output_collides_with_directory_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let purgery = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let config = test_server_config(&purgery, &root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_config = RunConfig::from_toml(
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"

[[postprocess.rules]]
match = "*.txt"
steps = ["compress"]
"#,
        )
        .unwrap();
        let run_plan = RunPlan {
            rules: vec![CompiledRule {
                pattern: "*.txt".into(),
                sync_names: None,
                steps: vec![ResolvedStep {
                    step_name: "compress".into(),
                    step_def: PostprocessStepDefinition {
                        kind: PostprocessKind::Subprocess,
                        program: "true".into(),
                        args: vec![],
                        expected_outputs: vec!["output_dir".into()],
                        keep_original: true,
                    },
                }],
            }],
        };

        let manifest = Manifest {
            run_id: RunId::new("pp-dir-collision".into()).unwrap(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/input.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/input.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("input.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 50,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                // Directory with the same name as the postprocess output
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/data/output_dir".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/output_dir".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("output_dir".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: Default::default(),
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };

        let empty_covered = std::collections::HashSet::new();
        let error = validate_unique_final_paths(
            &config,
            &nickname,
            &run_config,
            &manifest,
            &run_plan,
            &empty_covered,
        )
        .unwrap_err();
        assert!(
            error.contains("duplicate final path"),
            "error must mention duplicate final path: {error}"
        );
    }

    #[test]
    fn source_relative_classification_does_not_use_sync_to_prefix() {
        // Classification must evaluate match patterns against the source-relative
        // path, not the sync.to-prefixed path.
        let matched_mp4 = purgery_core::rsync_pattern_match("*.mp4", "a.mp4");
        assert!(matched_mp4, "*.mp4 must match a.mp4");
        let matched_videos = purgery_core::rsync_pattern_match("videos/*.mp4", "a.mp4");
        assert!(
            !matched_videos,
            "videos/*.mp4 must NOT match a.mp4 (source-relative)"
        );
        let matched_nested = purgery_core::rsync_pattern_match("**/*.mp4", "sub/b.mp4");
        assert!(matched_nested, "**/*.mp4 must match sub/b.mp4");
    }

    #[test]
    fn covered_entries_have_covered_mode_and_covered_by() {
        let entry_descendant = ManifestEntry {
            sync_name: SyncName::new("data".into()).unwrap(),
            local_path: ClientLocalPath::new("/source/photos/photo.txt".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("files/data/photos/photo.txt".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("photos/photo.txt".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 7,
            mtime_ns: 0,
            sha256: None,
            link_target: None,
            mode: purgery_core::ManifestEntryMode::Covered,
            postprocess_steps: Vec::new(),
            covered_by: Some("photos".into()),
        };
        assert_eq!(
            entry_descendant.mode,
            purgery_core::ManifestEntryMode::Covered
        );
        assert_eq!(entry_descendant.covered_by.as_deref(), Some("photos"));
    }

    // ── prepare-run covered_by validation tests ──

    #[test]
    fn prepare_run_rejects_covered_entry_with_missing_covered_by() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        // Set up a postprocess step so the directory can be postprocessed
        let config = ServerConfig {
            root: config.root,
            purgery_root: config.purgery_root,
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("covered-by-missing".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        write_run_toml_with_sync(&incoming, &nickname, "data", "data");
        let run_config_content = r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#;
        fs::write(incoming.join("run.toml"), run_config_content).unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Covered,
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("covered_by"),
            "must reject missing covered_by: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_covered_entry_with_wrong_covered_by() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("covered-by-wrong".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Covered,
                    postprocess_steps: Vec::new(),
                    covered_by: Some("wrong-path".into()),
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("covered_by"),
            "must reject wrong covered_by: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_covered_entry_with_non_empty_postprocess_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("covered-steps".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Covered,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: Some("album".into()),
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("postprocess_steps"),
            "must reject non-empty steps: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_descendant_marked_passthrough_under_postprocessed_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("descendant-passthrough".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Passthrough,
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("covered"),
            "must reject passthrough descendant: {error}"
        );
    }

    #[test]
    fn prepare_run_rejects_descendant_marked_postprocess_under_postprocessed_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("descendant-postprocess".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "album"
steps = ["pack"]
"#,
        )
        .unwrap();
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 100,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();
        let error = prepare_run(&config, &nickname, &run_id).unwrap_err();
        assert!(
            error.to_string().contains("covered"),
            "must reject postprocess descendant: {error}"
        );
    }

    #[test]
    fn processing_run_status_excludes_passthrough_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("no-passthrough-status".into()).unwrap();

        // Create a ready run with only postprocess/covered entries (no passthrough)
        let ready_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready_path.join("files/data/photos")).unwrap();
        fs::write(ready_path.join("files/data/photos/photo.txt"), b"photo").unwrap();

        fs::write(
            ready_path.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["pack"]
"#,
        )
        .unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/photos".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/photos".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("photos".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/photos/photo.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/photos/photo.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("photos/photo.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 5,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Covered,
                    postprocess_steps: Vec::new(),
                    covered_by: Some("photos".into()),
                },
            ],
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

        // With only postprocess/covered entries in the manifest, the status must
        // contain only those types of entries (no ordinary passthrough).
        assert_eq!(status.entries.len(), 2, "expected 2 status entries");
        assert!(status
            .entries
            .iter()
            .all(|e| e.status != FileStatus::Imported || e.final_paths.len() == 1));
    }

    #[test]
    fn prepare_run_without_passthrough_entries_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("no-passthrough-manifest".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        // Run config with postprocess rule
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "data"
to = "data"
delete_after_import = true

[[postprocess.rules]]
match = "photos"
steps = ["pack"]
"#,
        )
        .unwrap();

        // Manifest with only postprocess/covered entries (no ordinary passthrough)
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/photos".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/photos".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("photos".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                ManifestEntry {
                    sync_name: SyncName::new("data".into()).unwrap(),
                    local_path: ClientLocalPath::new("/source/photos/photo.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/data/photos/photo.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("photos/photo.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 13,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Covered,
                    postprocess_steps: Vec::new(),
                    covered_by: Some("photos".into()),
                },
            ],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        // prepare_run must succeed with a manifest that has only postprocess/covered entries
        let result = prepare_run(&config, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "prepare_run must succeed with only postprocess/covered entries: {:?}",
            result.err()
        );
    }

    #[test]
    fn prepare_run_rejects_sync_with_delete_after_import_false() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("no-delete-purgatory".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        // Run config with delete_after_import = false but a postprocess rule
        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = false

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
"#,
        )
        .unwrap();

        // Minimal manifest with one postprocess entry
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/source/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 10,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: purgery_core::ManifestEntryMode::Postprocess,
                postprocess_steps: vec!["pack".into()],
                covered_by: None,
            }],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let result = prepare_run(&config, &nickname, &run_id);
        assert!(
            result.is_err(),
            "prepare_run must reject purgatory sync with delete_after_import=false"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("delete_after_import = false"),
            "error should mention delete_after_import=false: {err}"
        );
    }

    #[test]
    fn scoped_rule_does_not_cover_directory_in_other_sync_group() {
        // A postprocess rule scoped to "videos" must not cause a directory
        // in "docs" to be considered covered by a postprocessed ancestor.
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("scoped-coverage".into()).unwrap();
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);

        // Set up two sync groups: "videos" (with album dir) and "docs" (with album dir)
        fs::create_dir_all(ready.join("files/videos/album")).unwrap();
        fs::write(ready.join("files/videos/album/song.mp3"), b"audio").unwrap();
        fs::create_dir_all(ready.join("files/docs/album")).unwrap();
        fs::write(ready.join("files/docs/album/report.txt"), b"text").unwrap();

        // Run config: rule scoped to "videos" only
        fs::write(
            ready.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[sync]]
name = "docs"
to = "docs"
delete_after_import = true

[[postprocess.rules]]
match = "album"
steps = ["pack"]
for = ["videos"]
"#,
        )
        .unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![
                // videos/album is postprocess (rule applies to videos)
                ManifestEntry {
                    sync_name: SyncName::new("videos".into()).unwrap(),
                    local_path: ClientLocalPath::new("/src/videos/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/videos/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Postprocess,
                    postprocess_steps: vec!["pack".into()],
                    covered_by: None,
                },
                // videos/album/song.mp3 is covered
                ManifestEntry {
                    sync_name: SyncName::new("videos".into()).unwrap(),
                    local_path: ClientLocalPath::new("/src/videos/album/song.mp3".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/videos/album/song.mp3".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/song.mp3".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 5,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Covered,
                    postprocess_steps: Vec::new(),
                    covered_by: Some("album".into()),
                },
                // docs/album is NOT postprocess (rule is scoped to videos only)
                ManifestEntry {
                    sync_name: SyncName::new("docs".into()).unwrap(),
                    local_path: ClientLocalPath::new("/src/docs/album".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/docs/album".into()).unwrap(),
                    relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Passthrough,
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
                // docs/album/report.txt is NOT covered (rule is scoped to videos only)
                ManifestEntry {
                    sync_name: SyncName::new("docs".into()).unwrap(),
                    local_path: ClientLocalPath::new("/src/docs/album/report.txt".into()).unwrap(),
                    staged_path: NormalizedRelativePath::new("files/docs/album/report.txt".into())
                        .unwrap(),
                    relative_path: NormalizedRelativePath::new("album/report.txt".into()).unwrap(),
                    kind: ManifestEntryKind::RegularFile,
                    size: 4,
                    mtime_ns: 0,
                    sha256: None,
                    link_target: None,
                    mode: purgery_core::ManifestEntryMode::Passthrough,
                    postprocess_steps: Vec::new(),
                    covered_by: None,
                },
            ],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        // Processing must succeed — the docs entries are valid passthrough
        process_run(&config, &nickname, &run_id).unwrap();

        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();

        // The docs/album/report.txt must not be skipped as "covered"
        let docs_entry = status
            .entries
            .iter()
            .find(|e| e.relative_path == "album/report.txt" && e.sync_name.as_str() == "docs");
        assert!(
            docs_entry.is_some(),
            "docs/album/report.txt must have a status entry"
        );
        assert_eq!(
            docs_entry.unwrap().status,
            FileStatus::Imported,
            "docs/album/report.txt must be imported, not skipped as covered"
        );
    }

    #[test]
    fn prepare_run_rejects_rule_with_empty_for() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("empty-for".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
for = []
"#,
        )
        .unwrap();
        fs::write(
            incoming.join("manifest.toml"),
            r#"
run_id = "empty-for"
nickname = "laptop"

[[entries]]
sync_name = "videos"
local_path = "/source/a.mp4"
staged_path = "files/videos/a.mp4"
relative_path = "a.mp4"
kind = "regular_file"
size = 5
mtime_ns = 100
mode = "postprocess"
postprocess_steps = ["pack"]
"#,
        )
        .unwrap();

        let result = prepare_run(&config, &nickname, &run_id);
        assert!(result.is_err(), "empty for must be rejected");
    }

    #[test]
    fn prepare_run_rejects_rule_with_unknown_for() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("unknown-for".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
for = ["missing"]
"#,
        )
        .unwrap();
        fs::write(
            incoming.join("manifest.toml"),
            r#"
run_id = "unknown-for"
nickname = "laptop"

[[entries]]
sync_name = "videos"
local_path = "/source/a.mp4"
staged_path = "files/videos/a.mp4"
relative_path = "a.mp4"
kind = "regular_file"
size = 5
mtime_ns = 100
mode = "postprocess"
postprocess_steps = ["pack"]
"#,
        )
        .unwrap();

        let result = prepare_run(&config, &nickname, &run_id);
        assert!(result.is_err(), "unknown sync in for must be rejected");
    }

    #[test]
    fn out_of_scope_rule_does_not_process_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("scoped-processing".into()).unwrap();
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);

        // videos/ has a matching file pattern, but the rule is scoped to "pictures"
        fs::create_dir_all(ready.join("files/videos")).unwrap();
        fs::write(ready.join("files/videos/a.mp4"), b"video").unwrap();
        fs::write(
            ready.join("run.toml"),
            br#"nickname = "laptop"
[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[sync]]
name = "pictures"
to = "pictures"
delete_after_import = true

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
for = ["pictures"]
"#,
        )
        .unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/src/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 5,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: purgery_core::ManifestEntryMode::Passthrough,
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        // process_run must succeed — the rule is out of scope for videos
        process_run(&config, &nickname, &run_id).unwrap();
        let done = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Done);
        let status_content = fs::read_to_string(done.join("status.toml")).unwrap();
        let status = RunStatus::from_toml(&status_content).unwrap();
        // videos/a.mp4 must be imported as passthrough, not processed by pack
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].status, FileStatus::Imported);
        assert!(
            status.entries[0].postprocess.is_none()
                || status.entries[0].postprocess.as_deref() == Some(&[])
        );
    }

    #[test]
    fn out_of_scope_rule_does_not_affect_planned_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec!["{file_stem}.out".into()],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("scoped-outputs".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();
        fs::create_dir_all(incoming.join("files/videos")).unwrap();
        fs::write(incoming.join("files/videos/album"), b"video-data").unwrap();
        fs::write(
            incoming.join("run.toml"),
            br#"nickname = "laptop"
[[sync]]
name = "videos"
to = "videos"
delete_after_import = true

[[sync]]
name = "pictures"
to = "pictures"
delete_after_import = true

[[postprocess.rules]]
match = "album"
steps = ["pack"]
for = ["videos"]
"#,
        )
        .unwrap();

        // Build the run config and run plan
        let run_config_content = fs::read_to_string(incoming.join("run.toml")).unwrap();
        let run_config = purgery_core::RunConfig::from_toml(&run_config_content).unwrap();
        let run_plan = RunPlan::build(&config, &run_config).unwrap();
        let sync_map = run_config.sync_map();

        // Create entries with the same relative path but different sync groups
        let videos_entry = ManifestEntry {
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/src/videos/album".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("files/videos/album".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 9,
            mtime_ns: 100,
            sha256: None,
            link_target: None,
            mode: purgery_core::ManifestEntryMode::Postprocess,
            postprocess_steps: vec!["pack".into()],
            covered_by: None,
        };
        let pictures_entry = ManifestEntry {
            sync_name: SyncName::new("pictures".into()).unwrap(),
            local_path: ClientLocalPath::new("/src/pictures/album".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("files/pictures/album".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("album".into()).unwrap(),
            kind: ManifestEntryKind::RegularFile,
            size: 12,
            mtime_ns: 200,
            sha256: None,
            link_target: None,
            mode: purgery_core::ManifestEntryMode::Postprocess,
            postprocess_steps: vec!["pack".into()],
            covered_by: None,
        };

        let videos_sync = sync_map.get("videos").unwrap();
        let pictures_sync = sync_map.get("pictures").unwrap();

        let videos_outputs =
            planned_entry_outputs(&config, &nickname, videos_sync, &videos_entry, &run_plan);
        let pictures_outputs = planned_entry_outputs(
            &config,
            &nickname,
            pictures_sync,
            &pictures_entry,
            &run_plan,
        );

        // videos/album should have postprocess outputs (keep_original + .out)
        assert!(
            videos_outputs.len() >= 2,
            "videos/album should have postprocess outputs, got: {videos_outputs:?}"
        );
        // pictures/album should have only its own final path (no rule applies)
        assert_eq!(
            pictures_outputs.len(),
            1,
            "pictures/album should have only its own final path, got: {pictures_outputs:?}"
        );
    }

    #[test]
    fn process_processing_run_rejects_delete_after_import_false() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("processing-no-delete".into()).unwrap();
        let ready = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Ready);
        fs::create_dir_all(ready.join("files/videos")).unwrap();
        fs::write(ready.join("files/videos/a.mp4"), b"content").unwrap();

        // Run config with delete_after_import = false but a postprocess rule
        fs::write(
            ready.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = false

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
"#,
        )
        .unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/source/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 7,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: purgery_core::ManifestEntryMode::Postprocess,
                postprocess_steps: vec!["pack".into()],
                covered_by: None,
            }],
        };
        fs::write(ready.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        // process_run goes through ready -> claim -> processing
        let result = process_run(&config, &nickname, &run_id);
        assert!(
            result.is_err(),
            "process_run must fail when run config has delete_after_import=false"
        );
        // The run should be in failed state
        let failed = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Failed);
        assert!(failed.exists(), "failed run dir must exist");
    }

    // ── Progress context tests ──

    #[test]
    fn processing_progress_has_real_entry_context() {
        // Use direct write_progress to assert entry context is preserved.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        write_progress(&path, &n, &r, "processing_entry", 1, 3, "b.txt", "").unwrap();
        let content = fs::read_to_string(path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(p.state, "processing_entry");
        assert_eq!(p.entry_index, 1);
        assert_eq!(p.entry_total, 3);
        assert_eq!(p.current_entry, "b.txt");
        assert_eq!(p.current_step, "");
    }

    #[test]
    fn publishing_status_progress_is_written() {
        // Use direct write_progress to assert publishing_status fields.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        write_progress(&path, &n, &r, "publishing_status", 0, 1, "", "").unwrap();
        let content = fs::read_to_string(path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(p.state, "publishing_status");
        assert_eq!(p.entry_index, 0);
        assert_eq!(p.entry_total, 1);
        assert!(p.current_entry.is_empty());
        assert!(p.current_step.is_empty());
    }

    // ── Progress sentinel and write-failure tests ──

    #[test]
    fn progress_update_has_no_sentinel_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, b"input").unwrap();
        let compressed = work_path.with_file_name("input.out");
        fs::write(&compressed, b"output").unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec!["{stem}.out".into()],
                            keep_original: false,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "*.txt".into(),
                    steps: vec!["compress".into()],
                    sync_names: None,
                }],
            },
        };
        let run_plan = RunPlan::build(&server_config, &run_config).unwrap();

        let captured = std::sync::Mutex::new(Vec::new());
        let mut callback = |update: &purgery_core::ProgressUpdate| {
            captured.lock().unwrap().push((
                update.state.to_owned(),
                update.entry_index,
                update.entry_total,
                update.current_entry.to_owned(),
                update.current_step.to_owned(),
            ));
        };

        apply_postprocessing_with_heartbeat(
            &run_plan,
            "data",
            "data/input.txt",
            &work_path,
            std::time::Duration::from_millis(1),
            &mut callback,
            0,
            1,
            "data/input.txt",
        )
        .expect("postprocessing must succeed");

        let updates = captured.lock().unwrap();
        assert!(
            !updates.is_empty(),
            "must have captured at least one progress update"
        );
        for (state, _ei, et, _ce, _cs) in updates.iter() {
            assert!(
                *et > 0,
                "step '{state}' must have entry_total > 0, got {et}"
            );
        }
    }

    #[test]
    fn progress_write_failure_does_not_fail_import() {
        let tmp = tempfile::tempdir().unwrap();
        let _purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("progress-fail".into()).unwrap();

        // Use a ready run with one file
        let ready_path = Utf8PathBuf::from_path_buf(tmp.path().join("purgery"))
            .unwrap()
            .join("laptop")
            .join("ready")
            .join(run_id.as_str());
        fs::create_dir_all(ready_path.join("files/data")).unwrap();
        fs::write(ready_path.join("files/data/file.txt"), b"content").unwrap();

        fs::write(
            ready_path.join("run.toml"),
            r#"nickname = "laptop"

[[sync]]
name = "data"
to = "data"
delete_after_import = true
"#,
        )
        .unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![ManifestEntry {
                sync_name: SyncName::new("data".into()).unwrap(),
                local_path: ClientLocalPath::new("/src/file.txt".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/data/file.txt".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("file.txt".into()).unwrap(),
                kind: ManifestEntryKind::RegularFile,
                size: 7,
                mtime_ns: 100,
                sha256: None,
                link_target: None,
                mode: Default::default(),
                postprocess_steps: Vec::new(),
                covered_by: None,
            }],
        };
        fs::write(
            ready_path.join("manifest.toml"),
            manifest.to_toml().unwrap(),
        )
        .unwrap();

        let config = test_server_config(
            &Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap(),
            &server_root,
        );

        // Move from ready to processing
        let processing_path = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Processing);
        fs::create_dir_all(processing_path.parent().unwrap()).unwrap();
        fs::rename(&ready_path, &processing_path).unwrap();

        // Pre-create a directory at the progress temp path so the progress write fails
        // (fs::write to an existing directory fails on Unix).
        // This simulates a progress write failure without blocking status writes.
        let progress_tmp = processing_path.join("progress.toml.tmp");
        fs::create_dir(&progress_tmp).unwrap();

        // Processing must still succeed despite progress write failure
        let result = process_processing_run(&config, &nickname, &run_id);
        assert!(
            result.is_ok(),
            "import must succeed even with progress write failure: {:?}",
            result.err()
        );

        // The final file must exist
        let final_path = server_root.join("laptop/data/file.txt");
        assert_eq!(
            fs::read_to_string(&final_path).unwrap(),
            "content",
            "file must be imported despite progress failure"
        );
    }

    // ── Publishing status and per-entry progress tests ──

    #[test]
    fn publishing_status_is_run_level_progress() {
        // publishing_status is tested via direct write_progress above.
        // This test is kept for documentation: run-level progress has empty
        // current_entry and current_step and coherent entry_total.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        write_progress(&path, &n, &r, "publishing_status", 0, 10, "", "").unwrap();
        let content = fs::read_to_string(path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(p.state, "publishing_status");
        assert_eq!(p.entry_index, 0);
        assert_eq!(p.entry_total, 10, "entry_total must be coherent");
        assert!(
            p.current_entry.is_empty() && p.current_step.is_empty(),
            "run-level progress must have empty entry/step"
        );
    }

    #[test]
    fn per_entry_progress_has_real_context() {
        // Use a progress callback capture to verify entry context is propagated
        // through the postprocessing pipeline.
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, b"input").unwrap();
        let compressed = work_path.with_file_name("input.out");
        fs::write(&compressed, b"output").unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec!["{stem}.out".into()],
                            keep_original: false,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "*.txt".into(),
                    steps: vec!["compress".into()],
                    sync_names: None,
                }],
            },
        };
        let run_plan = RunPlan::build(&server_config, &run_config).unwrap();

        let captured = std::sync::Mutex::new(Vec::new());
        let mut callback = |update: &purgery_core::ProgressUpdate| {
            captured.lock().unwrap().push((
                update.state.to_owned(),
                update.entry_index,
                update.entry_total,
                update.current_entry.to_owned(),
                update.current_step.to_owned(),
            ));
        };

        apply_postprocessing_with_heartbeat(
            &run_plan,
            "data",
            "data/input.txt",
            &work_path,
            std::time::Duration::from_millis(1),
            &mut callback,
            0,
            1,
            "data/input.txt",
        )
        .unwrap();

        let updates = captured.lock().unwrap();
        assert!(
            !updates.is_empty(),
            "must have at least one progress update"
        );
        for (state, ei, et, ce, cs) in updates.iter() {
            assert!(*et > 0, "entry_total must be > 0 for '{state}', got {et}");
            assert!(
                *ei < *et,
                "entry_index ({ei}) must be < entry_total ({et}) for '{state}'"
            );
            assert!(
                !ce.is_empty(),
                "current_entry must be non-empty for '{state}'"
            );
            // step_started/step_running/step_finished must have current_step
            if state.starts_with("step_") {
                assert!(!cs.is_empty(), "step '{state}' must have current_step");
            }
        }
    }
    // ── Entry index and progress invariant tests ──

    #[test]
    fn progress_tests_do_not_ignore_postprocess_result() {
        // Regression guard: progress tests must not discard the result of
        // apply_postprocessing_with_heartbeat with let _ = .
        let source = include_str!("lib.rs");
        // Check each line for the bad pattern, skipping this test's own assertion text.
        for (lineno, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed == "let _ = apply_postprocessing_with_heartbeat(" {
                panic!(
                    "line {}: progress tests must not ignore apply_postprocessing_with_heartbeat results;\n\
                     use .unwrap() or .expect() instead",
                    lineno + 1
                );
            }
        }
    }

    #[test]
    fn per_entry_first_entry_allows_index_zero() {
        // A manifest with at least one postprocessed entry should have
        // step_started/step_running/step_finished with entry_index=0 for
        // the first entry, entry_total>0, and current_entry!="".
        let tmp = tempfile::tempdir().unwrap();
        let work_path = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        fs::write(&work_path, b"input").unwrap();
        fs::write(work_path.with_file_name("input.out"), b"output").unwrap();

        let server_config = ServerConfig {
            root: ServerRoot::new("/data".into()).unwrap(),
            purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            gc: Default::default(),
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "compress".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec!["{stem}.out".into()],
                            keep_original: false,
                        },
                    );
                    m
                },
            },
            logging: Default::default(),
        };
        let run_config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![],
            postprocess: purgery_core::ClientPostprocessConfig {
                rules: vec![purgery_core::PostprocessRule {
                    pattern: "*.txt".into(),
                    steps: vec!["compress".into()],
                    sync_names: None,
                }],
            },
        };
        let run_plan = RunPlan::build(&server_config, &run_config).unwrap();

        let captured = std::sync::Mutex::new(Vec::new());
        let mut callback = |update: &purgery_core::ProgressUpdate| {
            captured.lock().unwrap().push((
                update.state.to_owned(),
                update.entry_index,
                update.entry_total,
                update.current_entry.to_owned(),
                update.current_step.to_owned(),
            ));
        };

        apply_postprocessing_with_heartbeat(
            &run_plan,
            "data",
            "data/input.txt",
            &work_path,
            std::time::Duration::from_millis(1),
            &mut callback,
            0,
            1,
            "data/input.txt",
        )
        .expect("postprocessing must succeed");

        let updates = captured.lock().unwrap();
        for (state, ei, et, ce, _cs) in updates.iter() {
            match state.as_str() {
                "step_started" | "step_running" | "step_finished" => {
                    // Per-entry invariants
                    assert!(
                        *et > 0,
                        "entry_total must be > 0 for per-entry state '{state}'"
                    );
                    assert!(
                        !ce.is_empty(),
                        "current_entry must be non-empty for per-entry state '{state}'"
                    );
                    assert!(ei < et, "entry_index ({ei}) must be < entry_total ({et})");
                    // entry_index = 0 is valid for the first entry
                    if *ei == 0 {
                        // This is fine — 0 is the valid index for the first entry.
                        // The test exists to confirm we don't reject it.
                    }
                }
                _ => {}
            }
        }
        // At least some step updates must have occurred
        assert!(
            updates.iter().any(|(s, _, _, _, _)| matches!(
                s.as_str(),
                "step_started" | "step_running" | "step_finished"
            )),
            "must have at least one step progress update"
        );
    }

    // ── Progress validation tests ──
    #[test]
    fn per_entry_progress_rejects_entry_total_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "step_started", 0, 0, "a.txt", "c");
        assert!(result.is_err(), "entry_total=0 must be rejected");
    }

    #[test]
    fn per_entry_progress_rejects_empty_current_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "step_started", 0, 1, "", "c");
        assert!(result.is_err(), "empty current_entry must be rejected");
    }

    #[test]
    fn per_entry_step_progress_rejects_empty_current_step() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "step_started", 0, 1, "a.txt", "");
        assert!(
            result.is_err(),
            "step state with empty current_step must be rejected"
        );
    }

    #[test]
    fn per_entry_progress_rejects_index_out_of_range() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "step_running", 5, 1, "a.txt", "c");
        assert!(
            result.is_err(),
            "entry_index >= entry_total must be rejected"
        );
    }

    #[test]
    fn processing_entry_rejects_empty_current_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "processing_entry", 0, 1, "", "");
        assert!(
            result.is_err(),
            "processing_entry with empty current_entry must be rejected"
        );
    }

    #[test]
    fn processing_entry_with_empty_step_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "processing_entry", 0, 1, "a.txt", "");
        assert!(
            result.is_ok(),
            "processing_entry with empty step must succeed"
        );
    }

    #[test]
    fn per_entry_first_entry_index_zero_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "step_started", 0, 1, "a.txt", "c");
        assert!(result.is_ok(), "entry_index=0 for first entry must succeed");
    }

    #[test]
    fn run_level_progress_with_empty_fields_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        let r1 = write_progress(&path, &n, &r, "processing_started", 0, 2, "", "");
        assert!(
            r1.is_ok(),
            "processing_started with empty fields must succeed"
        );

        let r2 = write_progress(&path, &n, &r, "publishing_status", 0, 2, "", "");
        assert!(
            r2.is_ok(),
            "publishing_status with empty fields must succeed"
        );
    }

    #[test]
    fn unknown_progress_state_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();
        let result = write_progress(&path, &n, &r, "nonsense", 0, 1, "a.txt", "c");
        assert!(result.is_err(), "unknown state must be rejected");
    }

    #[test]
    fn write_progress_best_effort_with_invalid_state_does_not_panic() {
        // Even with invalid progress, write_progress_best_effort must
        // log a warning and continue, not panic or propagate the error.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        // Invalid: entry_total=0 for per-entry
        write_progress_best_effort(&path, &n, &r, "step_started", 0, 0, "a.txt", "c");

        // Invalid: unknown state
        write_progress_best_effort(&path, &n, &r, "nonsense", 0, 1, "a.txt", "c");

        // These should not panic. The function logs a warning and returns.
    }

    #[test]
    fn invalid_best_effort_progress_does_not_clobber_valid_progress() {
        // Invalid best-effort progress must not overwrite an existing valid
        // progress.toml because validation rejects the write before any I/O.
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p")).unwrap();
        fs::create_dir_all(&path).unwrap();
        let n = Nickname::new("laptop".into()).unwrap();
        let r = RunId::new("t".into()).unwrap();

        // First, write a valid progress file
        write_progress(&path, &n, &r, "processing_entry", 0, 2, "a.txt", "").unwrap();
        let valid_content = fs::read_to_string(path.join("progress.toml")).unwrap();

        // Now call best-effort with invalid progress that doesn't clobber
        write_progress_best_effort(&path, &n, &r, "step_started", 0, 0, "a.txt", "c");

        // The file content must be unchanged
        let after_content = fs::read_to_string(path.join("progress.toml")).unwrap();
        assert_eq!(
            after_content, valid_content,
            "invalid best-effort progress must not clobber valid progress file"
        );

        // Also test with unknown state
        write_progress_best_effort(&path, &n, &r, "nonsense", 0, 1, "a.txt", "c");
        let after2 = fs::read_to_string(path.join("progress.toml")).unwrap();
        assert_eq!(
            after2, valid_content,
            "unknown state best-effort must not clobber valid progress"
        );
    }

    #[test]
    fn run_level_progress_may_have_empty_current_entry() {
        // processing_started and publishing_status are run-level.
        // They may have empty current_entry/current_step.
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("run-level".into()).unwrap();

        // Write processing_started (run-level)
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_started",
            0,
            2,
            "",
            "",
        )
        .unwrap();
        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(p.state, "processing_started");
        assert!(
            p.current_entry.is_empty() && p.current_step.is_empty(),
            "run-level progress may have empty entry/step"
        );
        assert_eq!(p.entry_total, 2, "run-level progress still has entry_total");
        assert_eq!(p.entry_index, 0, "run-level progress entry_index is 0");
    }

    // ── Progress start-time preservation tests ──

    #[test]
    fn progress_start_time_preserved_only_when_envelope_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-envelope".into()).unwrap();

        // Write an existing progress file with matching envelope
        let old_progress = purgery_core::ProcessingProgress {
            protocol_version: 1,
            nickname: "laptop".into(),
            run_id: "ts-envelope".into(),
            phase: "processing".into(),
            state: "old".into(),
            entry_index: 0,
            entry_total: 1,
            current_entry: String::new(),
            current_step: String::new(),
            started_at_unix_secs: 5000,
            updated_at_unix_secs: 5000,
        };
        let old_content = toml::to_string(&old_progress).unwrap();
        fs::write(processing_path.join("progress.toml"), &old_content).unwrap();

        // Matching envelope — started_at should be preserved
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            1,
            "a.txt",
            "",
        )
        .unwrap();
        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(
            p.started_at_unix_secs, 5000,
            "matching envelope must preserve started_at"
        );
    }

    #[test]
    fn progress_start_time_not_preserved_with_mismatched_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-mismatch".into()).unwrap();

        // Write existing progress with DIFFERENT nickname
        let old_progress = purgery_core::ProcessingProgress {
            protocol_version: 1,
            nickname: "other-machine".into(), // different nickname
            run_id: "ts-mismatch".into(),
            phase: "processing".into(),
            state: "old".into(),
            entry_index: 0,
            entry_total: 1,
            current_entry: String::new(),
            current_step: String::new(),
            started_at_unix_secs: 5000,
            updated_at_unix_secs: 5000,
        };
        let old_content = toml::to_string(&old_progress).unwrap();
        fs::write(processing_path.join("progress.toml"), &old_content).unwrap();

        // Mismatched envelope — started_at must NOT be preserved (should be fresh)
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            1,
            "a.txt",
            "",
        )
        .unwrap();
        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert!(
            p.started_at_unix_secs >= before,
            "mismatched envelope must initialize fresh started_at, got {} < {}",
            p.started_at_unix_secs,
            before
        );
    }

    #[test]
    fn malformed_existing_progress_does_not_preserve_start_time() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-malformed".into()).unwrap();

        // Write invalid TOML to progress.toml
        fs::write(processing_path.join("progress.toml"), "not valid toml {{{").unwrap();

        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            1,
            "a.txt",
            "",
        )
        .unwrap();
        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert!(
            p.started_at_unix_secs >= before,
            "malformed existing file must initialize fresh started_at, got {} < {}",
            p.started_at_unix_secs,
            before
        );
    }

    // ── Progress timestamp tests ──

    #[test]
    fn progress_timestamp_started_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-stable".into()).unwrap();

        // First write
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_started",
            0,
            3,
            "",
            "",
        )
        .unwrap();
        let content1 = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p1: purgery_core::ProcessingProgress = toml::from_str(&content1).unwrap();
        let first_started = p1.started_at_unix_secs;
        let first_updated = p1.updated_at_unix_secs;

        // Second write (after a short sleep to advance time)
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            3,
            "a.txt",
            "",
        )
        .unwrap();
        let content2 = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p2: purgery_core::ProcessingProgress = toml::from_str(&content2).unwrap();

        // started_at must be stable
        assert_eq!(
            p2.started_at_unix_secs, first_started,
            "started_at must not change between progress writes"
        );
        // updated_at must be >= previous (could be same second in tests)
        assert!(
            p2.updated_at_unix_secs >= first_updated,
            "updated_at must not go backwards: {} < {}",
            p2.updated_at_unix_secs,
            first_updated
        );
    }

    #[test]
    fn progress_timestamp_existing_file_preserves_started_at() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-existing".into()).unwrap();

        // Manually write a progress file with a specific started_at
        let old_progress = purgery_core::ProcessingProgress {
            protocol_version: 1,
            nickname: "laptop".into(),
            run_id: run_id.as_str().into(),
            phase: "processing".into(),
            state: "old".into(),
            entry_index: 0,
            entry_total: 1,
            current_entry: String::new(),
            current_step: String::new(),
            started_at_unix_secs: 1000,
            updated_at_unix_secs: 1000,
        };
        let old_content = toml::to_string(&old_progress).unwrap();
        fs::write(processing_path.join("progress.toml"), &old_content).unwrap();

        // Now write a new progress update
        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_entry",
            0,
            1,
            "a.txt",
            "",
        )
        .unwrap();

        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        assert_eq!(
            p.started_at_unix_secs, 1000,
            "started_at must be preserved from existing file"
        );
        assert!(
            p.updated_at_unix_secs >= 1000,
            "updated_at must be >= existing value"
        );
    }

    #[test]
    fn progress_timestamp_first_write_initializes_both() {
        let tmp = tempfile::tempdir().unwrap();
        let processing_path = Utf8PathBuf::from_path_buf(tmp.path().join("processing")).unwrap();
        fs::create_dir_all(&processing_path).unwrap();
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("ts-first".into()).unwrap();

        // No progress file exists yet
        assert!(!processing_path.join("progress.toml").exists());

        write_progress(
            &processing_path,
            &nickname,
            &run_id,
            "processing_started",
            0,
            2,
            "",
            "",
        )
        .unwrap();

        let content = fs::read_to_string(processing_path.join("progress.toml")).unwrap();
        let p: purgery_core::ProcessingProgress = toml::from_str(&content).unwrap();
        // Both fields must be set to reasonable values (> 0 means some time after epoch)
        assert!(
            p.started_at_unix_secs > 1000000000,
            "started_at must be set to current time on first write, got {}",
            p.started_at_unix_secs
        );
        assert!(
            p.updated_at_unix_secs > 1000000000,
            "updated_at must be set to current time on first write, got {}",
            p.updated_at_unix_secs
        );
        // They should be approximately equal (same write)
        assert!(
            p.updated_at_unix_secs >= p.started_at_unix_secs,
            "updated_at must be >= started_at"
        );
    }

    #[test]
    fn prepare_run_rejection_mentions_conformance() {
        let tmp = tempfile::tempdir().unwrap();
        let purgery_root = Utf8PathBuf::from_path_buf(tmp.path().join("purgery")).unwrap();
        let server_root = Utf8PathBuf::from_path_buf(tmp.path().join("storage")).unwrap();
        let config = test_server_config(&purgery_root, &server_root);
        let config = ServerConfig {
            postprocess: PostprocessConfig {
                steps: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(
                        "pack".to_owned(),
                        PostprocessStepDefinition {
                            kind: PostprocessKind::Subprocess,
                            program: "true".into(),
                            args: vec![],
                            expected_outputs: vec![],
                            keep_original: true,
                        },
                    );
                    m
                },
            },
            ..config
        };
        let nickname = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("conformance-test".into()).unwrap();
        let incoming = config
            .purgery_root
            .run_dir(&nickname, &run_id, RunPhase::Incoming);
        fs::create_dir_all(&incoming).unwrap();

        fs::write(
            incoming.join("run.toml"),
            r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"
delete_after_import = false

[[postprocess.rules]]
match = "*.mp4"
steps = ["pack"]
"#,
        )
        .unwrap();

        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nickname.clone(),
            entries: vec![],
        };
        fs::write(incoming.join("manifest.toml"), manifest.to_toml().unwrap()).unwrap();

        let result = prepare_run(&config, &nickname, &run_id);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("conformance")
                || err.contains("import-and-retire")
                || err.contains("indefinite"),
            "rejection must explain conformance tradeoff, got: {err}"
        );
    }
}
