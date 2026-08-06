use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashSet;
use std::fs;
use tracing::{info, warn};

use crate::ResolvedTransform;

const MAX_TRANSFORM_OUTPUT_BYTES: usize = 64 * 1024;

/// Default heartbeat interval for subprocess progress updates (5 seconds).
const DEFAULT_HEARTBEAT_SECS: u64 = 5;

/// Spawn a reader thread that drains a pipe and keeps a bounded tail.
fn bounded_output_reader(
    mut pipe: impl std::io::Read + Send + 'static,
) -> std::thread::JoinHandle<Result<Vec<u8>, String>> {
    std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(MAX_TRANSFORM_OUTPUT_BYTES);
        let mut truncated = false;
        let mut chunk = [0u8; 4096];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > MAX_TRANSFORM_OUTPUT_BYTES {
                        let excess = buf.len() - MAX_TRANSFORM_OUTPUT_BYTES;
                        let _ = buf.drain(..excess);
                        truncated = true;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("pipe read error: {e}")),
            }
        }
        if truncated {
            let marker = format!(
                "...(output truncated; showing last {MAX_TRANSFORM_OUTPUT_BYTES} bytes)...\n"
            );
            let mut result = Vec::with_capacity(marker.len() + buf.len());
            result.extend_from_slice(marker.as_bytes());
            result.extend_from_slice(&buf);
            Ok(result)
        } else {
            Ok(buf)
        }
    })
}

/// Join a bounded output reader thread.  Returns the captured bytes
/// or an error message if the thread panicked or the reader failed.
fn join_bounded_output(
    handle: Option<std::thread::JoinHandle<Result<Vec<u8>, String>>>,
) -> Result<Vec<u8>, String> {
    match handle {
        Some(h) => match h.join() {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("output reader thread panicked".to_string()),
        },
        None => Ok(Vec::new()),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn apply_transform_for_target(
    resolved: &ResolvedTransform,
    work_path: &Utf8Path,
    target: &purgery_core::ResolvedDestinationPlan,
    progress_cb: &mut dyn FnMut(&purgery_core::ProgressUpdate),
    entry_index: usize,
    entry_total: usize,
    current_entry: &str,
) -> Result<Vec<Utf8PathBuf>, String> {
    apply_transform_for_target_with_heartbeat(
        resolved,
        work_path,
        target,
        std::time::Duration::from_secs(DEFAULT_HEARTBEAT_SECS),
        progress_cb,
        entry_index,
        entry_total,
        current_entry,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn apply_transform_for_target_with_heartbeat(
    resolved: &ResolvedTransform,
    work_path: &Utf8Path,
    target: &purgery_core::ResolvedDestinationPlan,
    heartbeat_interval: std::time::Duration,
    progress_cb: &mut dyn FnMut(&purgery_core::ProgressUpdate),
    entry_index: usize,
    entry_total: usize,
    current_entry: &str,
) -> Result<Vec<Utf8PathBuf>, String> {
    let def = &resolved.def;

    purgery_core::validate_transform_definition(def)
        .map_err(|e| format!("transform '{}' definition is invalid: {e}", resolved.name))?;

    let work_parent = work_path
        .parent()
        .ok_or_else(|| "work path has no parent directory".to_string())?;

    match def.kind {
        purgery_core::TransformKind::Subprocess => {
            let args = def.build_args_for_target(work_path, target);
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
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("failed to spawn {}: {e}", resolved.name))?;

            // Drain stdout and stderr concurrently so the transform
            // never blocks on a full pipe buffer.  Do NOT inherit
            // these pipes — protocol stdout must remain machine-
            // readable TOML.
            let stdout_handle = child.stdout.take().map(bounded_output_reader);
            let stderr_handle = child.stderr.take().map(bounded_output_reader);

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

            // Always join drainers after the child exits, regardless
            // of success or failure.
            let captured_stdout = join_bounded_output(stdout_handle);
            let captured_stderr = join_bounded_output(stderr_handle);

            if !transform_status.success() {
                let stdout_tail = captured_stdout
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).to_string())
                    .unwrap_or_else(|e| format!("(stdout unavailable: {e})"));
                let stderr_tail = captured_stderr
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).to_string())
                    .unwrap_or_else(|e| format!("(stderr unavailable: {e})"));
                return Err(format!(
                    "{} failed with exit code {:?}\nstdout: {}\nstderr: {}",
                    resolved.name,
                    transform_status.code(),
                    stdout_tail.trim(),
                    stderr_tail.trim(),
                ));
            }

            // Success: log a warning if drainers failed, but do not
            // fail the transform for that.
            if let Err(e) = &captured_stdout {
                warn!(
                    transform = %resolved.name,
                    error = %e,
                    "stdout drainer failed after successful transform",
                );
            }
            if let Err(e) = &captured_stderr {
                warn!(
                    transform = %resolved.name,
                    error = %e,
                    "stderr drainer failed after successful transform",
                );
            }

            progress_cb(&purgery_core::ProgressUpdate::new(
                "transform_finished",
                entry_index,
                entry_total,
                current_entry,
                &resolved.name,
            ));

            let expected = def
                .resolve_expected_outputs_for_target(work_path, target)
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

#[allow(clippy::too_many_arguments)]
pub fn apply_transform(
    resolved: &ResolvedTransform,
    work_path: &Utf8Path,
    destination_root: &Utf8Path,
    target_directory: &Utf8Path,
    progress_cb: &mut dyn FnMut(&purgery_core::ProgressUpdate),
    entry_index: usize,
    entry_total: usize,
    current_entry: &str,
) -> Result<Vec<Utf8PathBuf>, String> {
    let target = test_target(work_path, destination_root, target_directory);
    apply_transform_for_target(
        resolved,
        work_path,
        &target,
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
    destination_root: &Utf8Path,
    target_directory: &Utf8Path,
    heartbeat_interval: std::time::Duration,
    progress_cb: &mut dyn FnMut(&purgery_core::ProgressUpdate),
    entry_index: usize,
    entry_total: usize,
    current_entry: &str,
) -> Result<Vec<Utf8PathBuf>, String> {
    let target = test_target(work_path, destination_root, target_directory);
    apply_transform_for_target_with_heartbeat(
        resolved,
        work_path,
        &target,
        heartbeat_interval,
        progress_cb,
        entry_index,
        entry_total,
        current_entry,
    )
}

fn test_target(
    work_path: &Utf8Path,
    destination_root: &Utf8Path,
    target_directory: &Utf8Path,
) -> purgery_core::ResolvedDestinationPlan {
    purgery_core::ResolvedDestinationPlan {
        operand: purgery_core::DestinationPath::new(destination_root.to_owned()).unwrap(),
        target_path: target_directory.join(work_path.file_name().unwrap_or("output")),
        target_directory: target_directory.to_owned(),
        placement: purgery_core::DestinationPlacement::DirectoryTarget,
    }
}
