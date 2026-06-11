use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashSet;
use std::fs;
use tracing::info;

use crate::RunPlan;

/// Default heartbeat interval for subprocess progress updates (5 seconds).
const DEFAULT_HEARTBEAT_SECS: u64 = 5;

pub fn apply_postprocessing(
    run_plan: &RunPlan,
    sync_name: &str,
    normalized_path: &str,
    work_path: &Utf8Path,
    progress_step: &mut dyn FnMut(&purgery_core::ProgressUpdate),
) -> Result<Vec<Utf8PathBuf>, String> {
    apply_postprocessing_with_heartbeat(
        run_plan,
        sync_name,
        normalized_path,
        work_path,
        std::time::Duration::from_secs(DEFAULT_HEARTBEAT_SECS),
        progress_step,
    )
}

pub fn apply_postprocessing_with_heartbeat(
    run_plan: &RunPlan,
    sync_name: &str,
    normalized_path: &str,
    work_path: &Utf8Path,
    heartbeat_interval: std::time::Duration,
    progress_step: &mut dyn FnMut(&purgery_core::ProgressUpdate),
) -> Result<Vec<Utf8PathBuf>, String> {
    let mut results: Vec<Utf8PathBuf> = Vec::new();

    let work_parent = work_path
        .parent()
        .ok_or_else(|| "work path has no parent directory".to_string())?;

    let Some(compiled) = run_plan.first_matching_rule(sync_name, normalized_path) else {
        return Err("no selected postprocess rule for entry".into());
    };

    for step in &compiled.steps {
        let step_def = &step.step_def;

        match step_def.kind {
            purgery_core::PostprocessKind::Subprocess => {
                let args = step_def.build_args(work_path);
                info!(step = %step.step_name, program = %step_def.program, "running postprocess step");
                progress_step(&purgery_core::ProgressUpdate::new(
                    "step_started",
                    0,
                    0,
                    normalized_path,
                    &step.step_name,
                ));

                // Use spawn + try_wait loop for heartbeat, not blocking .status()
                let mut child = std::process::Command::new(&step_def.program)
                    .args(&args)
                    .spawn()
                    .map_err(|e| format!("failed to spawn {}: {e}", step.step_name))?;

                let step_status = loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break status,
                        Ok(None) => {
                            // Still running — update progress heartbeat
                            progress_step(&purgery_core::ProgressUpdate::new(
                                "step_running",
                                0,
                                0,
                                normalized_path,
                                &step.step_name,
                            ));
                            std::thread::sleep(heartbeat_interval);
                        }
                        Err(e) => {
                            return Err(format!("failed to wait for {}: {e}", step.step_name));
                        }
                    }
                };

                if !step_status.success() {
                    return Err(format!(
                        "{} failed with exit code {:?}",
                        step.step_name,
                        step_status.code()
                    ));
                }

                progress_step(&purgery_core::ProgressUpdate::new(
                    "step_finished",
                    0,
                    0,
                    normalized_path,
                    &step.step_name,
                ));

                let expected = step_def
                    .resolve_expected_outputs(work_path)
                    .map_err(|e| format!("{}: {e}", step.step_name))?;
                for exp in &expected {
                    if !exp.starts_with(work_parent) {
                        return Err(format!(
                            "expected output '{}' is outside work area '{}'",
                            exp.as_str(),
                            work_parent.as_str()
                        ));
                    }
                    let metadata = fs::symlink_metadata(exp.as_std_path()).map_err(|error| {
                        if error.kind() == std::io::ErrorKind::NotFound {
                            format!("expected output not found: {}", exp.as_str())
                        } else {
                            format!(
                                "failed to inspect expected output '{}': {error}",
                                exp.as_str()
                            )
                        }
                    })?;
                    let file_type = metadata.file_type();
                    let ok = file_type.is_dir() && !file_type.is_symlink()
                        || file_type.is_file()
                        || file_type.is_symlink();
                    if !ok {
                        return Err(format!(
                            "expected output is not a supported entry type: {}",
                            exp.as_str()
                        ));
                    }
                }

                if step_def.keep_original {
                    results.push(work_path.to_owned());
                }
                results.extend(expected);
            }
        }
    }

    {
        let mut seen = HashSet::new();
        results.retain(|p| seen.insert(p.as_str().to_owned()));
    }

    if results.is_empty() {
        return Err("postprocessing produced zero outputs, but at least one is required".into());
    }

    Ok(results)
}
