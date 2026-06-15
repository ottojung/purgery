use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashSet;
use std::fs;
use tracing::info;

use crate::ResolvedTransform;

/// Default heartbeat interval for subprocess progress updates (5 seconds).
const DEFAULT_HEARTBEAT_SECS: u64 = 5;

#[allow(clippy::too_many_arguments)]
pub fn apply_transform(
    resolved: &ResolvedTransform,
    work_path: &Utf8Path,
    target_directory: &Utf8Path,
    progress_cb: &mut dyn FnMut(&purgery_core::ProgressUpdate),
    entry_index: usize,
    entry_total: usize,
    current_entry: &str,
) -> Result<Vec<Utf8PathBuf>, String> {
    apply_transform_with_heartbeat(
        resolved,
        work_path,
        target_directory,
        std::time::Duration::from_secs(DEFAULT_HEARTBEAT_SECS),
        progress_cb,
        entry_index,
        entry_total,
        current_entry,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn apply_transform_with_heartbeat(
    resolved: &ResolvedTransform,
    work_path: &Utf8Path,
    target_directory: &Utf8Path,
    heartbeat_interval: std::time::Duration,
    progress_cb: &mut dyn FnMut(&purgery_core::ProgressUpdate),
    entry_index: usize,
    entry_total: usize,
    current_entry: &str,
) -> Result<Vec<Utf8PathBuf>, String> {
    let def = &resolved.def;

    let work_parent = work_path
        .parent()
        .ok_or_else(|| "work path has no parent directory".to_string())?;

    match def.kind {
        purgery_core::TransformKind::Subprocess => {
            let args = def.build_args(work_path, target_directory);
            info!(transform = %resolved.name, program = %def.program, "running transform");
            progress_cb(&purgery_core::ProgressUpdate::new(
                "transform_started",
                entry_index,
                entry_total,
                current_entry,
                &resolved.name,
            ));

            let mut child = std::process::Command::new(&def.program)
                .args(&args)
                .current_dir(work_parent)
                .spawn()
                .map_err(|e| format!("failed to spawn {}: {e}", resolved.name))?;

            let transform_status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {
                        progress_cb(&purgery_core::ProgressUpdate::new(
                            "transform_running",
                            entry_index,
                            entry_total,
                            current_entry,
                            &resolved.name,
                        ));
                        std::thread::sleep(heartbeat_interval);
                    }
                    Err(e) => {
                        return Err(format!("failed to wait for {}: {e}", resolved.name));
                    }
                }
            };

            if !transform_status.success() {
                return Err(format!(
                    "{} failed with exit code {:?}",
                    resolved.name,
                    transform_status.code()
                ));
            }

            progress_cb(&purgery_core::ProgressUpdate::new(
                "transform_finished",
                entry_index,
                entry_total,
                current_entry,
                &resolved.name,
            ));

            let expected = def
                .resolve_expected_outputs(work_path, target_directory)
                .map_err(|e| format!("{}: {e}", resolved.name))?;
            for exp in &expected {
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

            let mut results: Vec<Utf8PathBuf> = expected;
            {
                let mut seen = HashSet::new();
                results.retain(|p| seen.insert(p.as_str().to_owned()));
            }
            Ok(results)
        }
    }
}
