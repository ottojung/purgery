use purgery_core::{Nickname, RunId, RunPhase, RunStatus, ServerConfig};
use std::fmt;
use std::fs;
use tracing::{info, warn};

use crate::phases::write_run_failure;
use crate::process::process_processing_run;

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
        Ok(content) => match RunStatus::from_toml(&content) {
            Ok(status) => {
                if let Err(e) = purgery_core::require_compatible_purgery_version(
                    &status.purgery_version,
                    "status",
                ) {
                    let error = format!("incompatible status version: {e}");
                    warn!(
                        nickname = %nickname.as_str(),
                        run_id = %run_id.as_str(),
                        phase = "processing",
                        recovery_action = "refuse_incompatible_status",
                        error,
                        "processing run has incompatible status; leaving in place for operator inspection"
                    );
                    return Err(RecoveryError::IncompatibleStatus { message: error });
                }
                if status.nickname != *nickname || status.run_id != *run_id {
                    let error = "interrupted processing had mismatched status envelope";
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
                    return write_run_failure(&config.work_dir, nickname, run_id, error)
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
                crate::phases::finalize_processing_run(config, nickname, run_id, &status.state)?;
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
            Err(_) => {
                let error = "interrupted processing had malformed status";
                warn!(
                    nickname = %nickname.as_str(),
                    run_id = %run_id.as_str(),
                    phase = "processing",
                    run_status = "failed",
                    recovery_action = "replace_malformed_status",
                    error,
                    "processing run recovery failed"
                );
                write_run_failure(&config.work_dir, nickname, run_id, error)
                    .map_err(RecoveryError::Other)
            }
        },
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
