use purgery_core::{Nickname, RunId, RunPhase, RunStatus, ServerConfig};
use std::fmt;
use std::fs;
use tracing::{info, warn};

use crate::phases::write_run_failure;
use crate::process::{process_processing_run, ProcessingError};

/// Outcome of attempting to recover a processing run.
#[derive(Debug)]
pub enum RecoveryError {
    /// The processing directory has a status.toml with an incompatible
    /// purgery_version. The run must be left in place for operator
    /// inspection — no automatic migration, replacement, or move to failed.
    IncompatibleStatus { message: String },
    /// A real error occurred during recovery (IO, malformed current
    /// status, failed replay, etc.). The outer loop should write a
    /// failure status as usual.
    Other(anyhow::Error),
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryError::IncompatibleStatus { message } => write!(f, "{message}"),
            RecoveryError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RecoveryError::IncompatibleStatus { .. } => None,
            RecoveryError::Other(e) => Some(e.as_ref()),
        }
    }
}

impl From<anyhow::Error> for RecoveryError {
    fn from(e: anyhow::Error) -> Self {
        RecoveryError::Other(e)
    }
}

impl From<ProcessingError> for RecoveryError {
    fn from(e: crate::process::ProcessingError) -> Self {
        match e {
            crate::process::ProcessingError::Incompatible { message, .. } => {
                RecoveryError::IncompatibleStatus { message }
            }
            crate::process::ProcessingError::Other(e) => RecoveryError::Other(e),
        }
    }
}

pub fn recover_or_process_processing_run(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
) -> Result<(), RecoveryError> {
    let processing_path = config
        .work_dir
        .run_dir(nickname, run_id, RunPhase::Processing);
    let status_path = processing_path.join("status.toml");

    match fs::read_to_string(&status_path) {
        Ok(content) => {
            // Probe purgery_version from raw TOML before full
            // deserialization so we can distinguish old/incompatible
            // state from malformed current state.
            match purgery_core::probe_purgery_version_from_toml(&content) {
                Err(purgery_core::VersionProbeError::MissingVersion) => {
                    let msg = format!(
                        "status file '{}' is missing purgery_version (too old); \
                         leaving in place for operator inspection",
                        status_path.as_str(),
                    );
                    warn!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        status_path = %status_path.as_str(),
                        "processing status missing purgery_version (too old); skipping",
                    );
                    Err(RecoveryError::IncompatibleStatus { message: msg })
                }
                Err(purgery_core::VersionProbeError::InvalidToml(_)) => {
                    // Invalid TOML — could be old corruption or malformed
                    // current. Try parsing as current domain object; if
                    // that fails too, treat as malformed current state.
                    recover_malformed_status(config, nickname, run_id, &status_path)
                }
                Ok(version) => {
                    let version_ok =
                        purgery_core::require_compatible_purgery_version(&version, "status")
                            .is_ok();
                    if !version_ok {
                        let msg = format!(
                            "status file '{}' has incompatible purgery_version: \
                             producer {version}, current {}; leaving in place for \
                             operator inspection",
                            status_path.as_str(),
                            purgery_core::current_purgery_version(),
                        );
                        warn!(
                            nickname = %nickname.as_str(),
                            run_id = %run_id.as_str(),
                            status_path = %status_path.as_str(),
                            version = %version,
                            current_version = %purgery_core::current_purgery_version(),
                            "processing status has incompatible purgery_version; skipping",
                        );
                        Err(RecoveryError::IncompatibleStatus { message: msg })
                    } else {
                        match RunStatus::from_toml(&content) {
                            Ok(status) => {
                                if status.nickname != *nickname || status.run_id != *run_id {
                                    let error =
                                        "interrupted processing had mismatched status envelope";
                                    warn!(
                                        nickname = %nickname.as_str(),
                                        run_id = %run_id.as_str(),
                                        status_nickname = %status.nickname.as_str(),
                                        status_run_id = %status.run_id.as_str(),
                                        phase = "processing",
                                        run_status = "failed",
                                        recovery_action = "replace_mismatched_status",
                                        error,
                                        "processing run recovery failed"
                                    );
                                    return write_run_failure(
                                        &config.work_dir,
                                        nickname,
                                        run_id,
                                        error,
                                    )
                                    .map_err(RecoveryError::Other);
                                }
                                info!(
                                    nickname = %nickname.as_str(),
                                    run_id = %run_id.as_str(),
                                    phase = "processing",
                                    run_status = %status.state.as_str(),
                                    recovery_action = "finalize_terminal_move",
                                    "processing run had valid status, finalizing terminal move"
                                );
                                crate::phases::finalize_processing_run(
                                    config,
                                    nickname,
                                    run_id,
                                    &status.state,
                                )?;
                                info!(
                                    nickname = %nickname.as_str(),
                                    run_id = %run_id.as_str(),
                                    phase = "processing",
                                    run_status = %status.state.as_str(),
                                    recovery_action = "terminal_move_complete",
                                    "processing run recovered"
                                );
                                Ok(())
                            }
                            Err(e) => {
                                let msg = format!(
                                    "interrupted processing had malformed status file '{}': {e}",
                                    status_path.as_str(),
                                );
                                warn!(
                                    nickname = %nickname.as_str(),
                                    run_id = %run_id.as_str(),
                                    phase = "processing",
                                    run_status = "failed",
                                    recovery_action = "replace_malformed_status",
                                    error = %msg,
                                    "processing run recovery failed"
                                );
                                write_run_failure(&config.work_dir, nickname, run_id, &msg)
                                    .map_err(RecoveryError::Other)
                            }
                        }
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = "processing",
                recovery_action = "replay_staged_files",
                "processing run interrupted, replaying from staged files"
            );
            process_processing_run(config, nickname, run_id)?;
            info!(
                nickname = %nickname.as_str(),
                run_id = %run_id.as_str(),
                phase = "processing",
                recovery_action = "replay_complete",
                "processing run recovered"
            );
            Ok(())
        }
        Err(error) => Err(RecoveryError::Other(anyhow::anyhow!(
            "failed to read processing status: {}: {error}",
            status_path.as_str()
        ))),
    }
}

/// Handle a malformed (but version-compatible or unparseable) status file
/// by writing a failure status.
fn recover_malformed_status(
    config: &ServerConfig,
    nickname: &Nickname,
    run_id: &RunId,
    status_path: &camino::Utf8Path,
) -> Result<(), RecoveryError> {
    let msg = format!(
        "interrupted processing had malformed status file '{}'",
        status_path.as_str(),
    );
    warn!(
        nickname = %nickname.as_str(),
        run_id = %run_id.as_str(),
        phase = "processing",
        run_status = "failed",
        recovery_action = "replace_malformed_status",
        error = %msg,
        "processing run recovery failed"
    );
    write_run_failure(&config.work_dir, nickname, run_id, &msg).map_err(RecoveryError::Other)
}
