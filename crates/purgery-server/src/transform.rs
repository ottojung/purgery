use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use tracing::info;

use crate::ResolvedTransform;

const MAX_TRANSFORM_OUTPUT_BYTES: usize = 64 * 1024;

/// Spawn a reader thread that drains a pipe and keeps a bounded tail.
fn bounded_output_reader(
    mut pipe: impl Read + Send + 'static,
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

/// Default heartbeat interval for subprocess progress updates (5 seconds).
const DEFAULT_HEARTBEAT_SECS: u64 = 5;

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
    apply_transform_with_heartbeat(
        resolved,
        work_path,
        destination_root,
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
    destination_root: &Utf8Path,
    target_directory: &Utf8Path,
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

            if !transform_status.success() {
                // Join drainers to capture stdout/stderr before building the error.
                let captured_stdout = stdout_handle
                    .and_then(|h| h.join().ok())
                    .unwrap_or(Ok(Vec::new()))
                    .unwrap_or_default();
                let captured_stderr = stderr_handle
                    .and_then(|h| h.join().ok())
                    .unwrap_or(Ok(Vec::new()))
                    .unwrap_or_default();
                let stdout_tail = String::from_utf8_lossy(&captured_stdout);
                let stderr_tail = String::from_utf8_lossy(&captured_stderr);
                return Err(format!(
                    "{} failed with exit code {:?}\nstdout: {}\nstderr: {}",
                    resolved.name,
                    transform_status.code(),
                    stdout_tail.trim(),
                    stderr_tail.trim(),
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
                .resolve_expected_outputs(work_path, destination_root, target_directory)
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
