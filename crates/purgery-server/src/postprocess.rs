use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashSet;
use std::fs;
use tracing::info;

use crate::RunPlan;

pub fn apply_postprocessing(
    run_plan: &RunPlan,
    sync_name: &str,
    normalized_path: &str,
    work_path: &Utf8Path,
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

                let status = std::process::Command::new(&step_def.program)
                    .args(&args)
                    .status()
                    .map_err(|e| format!("failed to run {}: {e}", step.step_name))?;

                if !status.success() {
                    return Err(format!(
                        "{} failed with exit code {:?}",
                        step.step_name,
                        status.code()
                    ));
                }

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
