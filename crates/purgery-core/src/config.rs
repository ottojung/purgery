use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::str::FromStr;

use crate::path::*;
use crate::transform::{TransformConfig, TransformDefinition};
use crate::ConfigError;

// ── Lease / GC Config ────────────────────────────────────────────────

const DEFAULT_INCOMING_LEASE_SECS: u64 = 1800;
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GCConfig {
    #[serde(default = "default_incoming_lease_secs")]
    pub incoming_lease_secs: u64,
    #[serde(default = "default_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
}

impl Default for GCConfig {
    fn default() -> Self {
        GCConfig {
            incoming_lease_secs: DEFAULT_INCOMING_LEASE_SECS,
            heartbeat_interval_secs: DEFAULT_HEARTBEAT_INTERVAL_SECS,
        }
    }
}

fn default_incoming_lease_secs() -> u64 {
    DEFAULT_INCOMING_LEASE_SECS
}

fn default_heartbeat_interval_secs() -> u64 {
    DEFAULT_HEARTBEAT_INTERVAL_SECS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseFile {
    pub protocol_version: u32,
    pub nickname: String,
    pub run_id: String,
    pub created_at_unix_secs: u64,
    pub last_heartbeat_unix_secs: u64,
    pub expires_at_unix_secs: u64,
}

// ── Config Types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub level: LogLevel,
    #[serde(default)]
    pub format: LogFormat,
    #[serde(default)]
    pub color: ColorMode,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig {
            level: LogLevel::Info,
            format: LogFormat::Pretty,
            color: ColorMode::Auto,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Error => write!(f, "error"),
            LogLevel::Warn => write!(f, "warn"),
            LogLevel::Info => write!(f, "info"),
            LogLevel::Debug => write!(f, "debug"),
            LogLevel::Trace => write!(f, "trace"),
        }
    }
}

impl FromStr for LogLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            other => Err(format!("unknown log level: {other}")),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Compact,
    Json,
}

impl FromStr for LogFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pretty" => Ok(LogFormat::Pretty),
            "compact" => Ok(LogFormat::Compact),
            "json" => Ok(LogFormat::Json),
            other => Err(format!("unknown log format: {other}")),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

impl FromStr for ColorMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(ColorMode::Auto),
            "always" => Ok(ColorMode::Always),
            "never" => Ok(ColorMode::Never),
            other => Err(format!("unknown color mode: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfigFile {
    pub work_dir: PurgeryRoot,
    #[serde(default)]
    pub transform: Vec<TransformDefinition>,
    #[serde(default)]
    pub gc: GCConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub work_dir: PurgeryRoot,
    pub transform: TransformConfig,
    pub gc: GCConfig,
    pub logging: LoggingConfig,
}

impl ServerConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: ServerConfigFile = toml::from_str(input)?;
        let transforms = config.transform;
        // Reject duplicate transform names.
        let mut seen = HashSet::new();
        for td in &transforms {
            if !seen.insert(&td.name) {
                return Err(ConfigError::TomlSerialize(format!(
                    "duplicate transform name: {}",
                    td.name
                )));
            }
        }
        Ok(Self {
            work_dir: config.work_dir,
            transform: TransformConfig { transforms },
            gc: config.gc,
            logging: config.logging,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub nickname: Nickname,
    pub destination: DestinationPath,
    #[serde(default)]
    pub delete_after_import: bool,
}

impl RunConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: RunConfig = toml::from_str(input)?;
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string(self).map_err(|e| ConfigError::TomlSerialize(e.to_string()))
    }
}

// ── Response types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeginRunResponse {
    pub protocol_version: u32,
    pub nickname: String,
    pub run_id: String,
    pub incoming_dir: String,
    pub files_dir: String,
    pub run_config_path: String,
    pub manifest_path: String,
    pub heartbeat_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRunResponse {
    pub protocol_version: u32,
    pub nickname: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

// ── Run State / Progress ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStateResponse {
    pub protocol_version: u32,
    pub nickname: String,
    pub run_id: String,
    pub phase: String,
    pub terminal: bool,
    pub message: String,
    pub updated_at_unix_secs: u64,
    pub observed_at_unix_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_total: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_entry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_transform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessingProgress {
    pub protocol_version: u32,
    pub nickname: String,
    pub run_id: String,
    pub phase: String,
    pub state: String,
    pub entry_index: usize,
    pub entry_total: usize,
    pub current_entry: String,
    pub current_transform: String,
    pub started_at_unix_secs: u64,
    pub updated_at_unix_secs: u64,
}

/// Progress update passed into transform callbacks.
/// Carries full entry context for progress reporting.
#[derive(Debug, Clone)]
pub struct ProgressUpdate<'a> {
    pub state: &'a str,
    pub entry_index: usize,
    pub entry_total: usize,
    pub current_entry: &'a str,
    pub current_transform: &'a str,
}

impl<'a> ProgressUpdate<'a> {
    pub fn new(
        state: &'a str,
        entry_index: usize,
        entry_total: usize,
        current_entry: &'a str,
        current_transform: &'a str,
    ) -> Self {
        ProgressUpdate {
            state,
            entry_index,
            entry_total,
            current_entry,
            current_transform,
        }
    }
}

/// Client-persisted phases for a transform run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientRunPhase {
    UploadCompleteFinishPending,
    WaitingForTerminalState,
    TerminalStatusSeen,
    CleanupComplete,
    Abandoned,
    Corrupt,
}

/// Client-persisted state for a transform run, used to resume
/// waiting and cleanup after a crash.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRunState {
    pub protocol_version: u32,
    pub nickname: String,
    pub run_id: String,
    pub host: String,
    pub server_command: String,
    pub manifest: String,
    pub run_config: String,
    pub phase: ClientRunPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_status: Option<String>,
}
