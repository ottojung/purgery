use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::str::FromStr;

use crate::path::*;
use crate::postprocess::PostprocessConfig;
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
pub struct ServerConfig {
    pub root: ServerRoot,
    pub purgery_root: PurgeryRoot,
    #[serde(default)]
    pub postprocess: PostprocessConfig,
    #[serde(default)]
    pub gc: GCConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl ServerConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: ServerConfig = toml::from_str(input)?;
        Ok(config)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub nickname: Nickname,
    pub server: ServerConnection,
    #[serde(default)]
    pub sync: Vec<SyncMapping>,
    #[serde(default)]
    pub postprocess: ClientPostprocessConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub state_dir: String,
}

impl ClientConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: ClientConfig = toml::from_str(input)?;
        if config.state_dir.is_empty() {
            return Err(ConfigError::StateDir("must be non-empty".into()));
        }
        if !config.state_dir.starts_with('/') {
            return Err(ConfigError::StateDir("must be an absolute path".into()));
        }
        let sync_names: Vec<SyncName> = config.sync.iter().map(|s| s.name.clone()).collect();
        config
            .postprocess
            .validate(&sync_names)
            .map_err(ConfigError::PostprocessConfig)?;
        config
            .postprocess
            .validate_delete_after_import(&config.sync)
            .map_err(ConfigError::PostprocessConfig)?;
        Ok(config)
    }

    pub fn find_sync(&self, name: &str) -> Option<&SyncMapping> {
        self.sync.iter().find(|s| s.name.as_str() == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConnection {
    pub host: RemoteHost,
    #[serde(default = "default_server_command")]
    pub command: String,
}

fn default_server_command() -> String {
    "purgery-server".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncMapping {
    pub name: SyncName,
    #[serde(rename = "from")]
    pub from_path: LocalSourcePath,
    #[serde(rename = "to")]
    pub to_path: RelativeDestinationPath,
    #[serde(default = "default_delete_after_import")]
    pub delete_after_import: bool,
}

fn default_delete_after_import() -> bool {
    false
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPostprocessConfig {
    #[serde(default)]
    pub rules: Vec<PostprocessRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostprocessRule {
    #[serde(rename = "match")]
    pub pattern: String,
    pub steps: Vec<String>,
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub sync_names: Option<Vec<SyncName>>,
}

impl PostprocessRule {
    pub fn applies_to(&self, sync_name: &str) -> bool {
        match &self.sync_names {
            None => true,
            Some(names) => names.iter().any(|n| n.as_str() == sync_name),
        }
    }
}

pub fn applicable_rules<'a>(
    rules: &'a [PostprocessRule],
    sync_name: &str,
) -> Vec<&'a PostprocessRule> {
    rules.iter().filter(|r| r.applies_to(sync_name)).collect()
}

impl ClientPostprocessConfig {
    pub fn validate(&self, sync_names: &[SyncName]) -> Result<(), String> {
        for rule in &self.rules {
            if let Some(ref names) = rule.sync_names {
                if names.is_empty() {
                    return Err("postprocess rule has empty for list".into());
                }
                for name in names {
                    if !sync_names.iter().any(|s| s == name) {
                        return Err(format!(
                            "postprocess rule references unknown sync name '{}' in for",
                            name.as_str()
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn validate_delete_after_import(&self, syncs: &[SyncMapping]) -> Result<(), String> {
        for sync in syncs {
            let applicable = applicable_rules(&self.rules, sync.name.as_str());
            if !applicable.is_empty() && !sync.delete_after_import {
                return Err(format!(
                    "sync group '{}' has applicable postprocess rules but \
                     delete_after_import is false; \
                     postprocessing transforms the original and the server does not retain \
                     indefinite source metadata, so confirmed originals must be retired \
                     locally (import-and-retire)",
                    sync.name.as_str()
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncExecutionClass {
    PassthroughNoDelete,
    PassthroughDeleteAfterImport,
    Purgatory,
}

pub fn classify_sync_groups<'a>(
    syncs: &'a [SyncMapping],
    rules: &'a [PostprocessRule],
) -> Result<Vec<(SyncExecutionClass, &'a SyncMapping)>, String> {
    let mut result = Vec::with_capacity(syncs.len());
    for sync in syncs {
        let applicable = applicable_rules(rules, sync.name.as_str());
        if !applicable.is_empty() && !sync.delete_after_import {
            return Err(format!(
                "sync group '{}' has applicable postprocess rules but \
                 delete_after_import is false; \
                 postprocessing transforms the original and the server does not retain \
                 indefinite source metadata, so confirmed originals must be retired \
                 locally (import-and-retire)",
                sync.name.as_str()
            ));
        }
        let class = if !applicable.is_empty() {
            SyncExecutionClass::Purgatory
        } else if sync.delete_after_import {
            SyncExecutionClass::PassthroughDeleteAfterImport
        } else {
            SyncExecutionClass::PassthroughNoDelete
        };
        result.push((class, sync));
    }
    Ok(result)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub nickname: Nickname,
    #[serde(default)]
    pub sync: Vec<RunConfigSync>,
    #[serde(default)]
    pub postprocess: ClientPostprocessConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfigSync {
    pub name: SyncName,
    #[serde(rename = "to")]
    pub to_path: RelativeDestinationPath,
    #[serde(default = "default_delete_after_import")]
    pub delete_after_import: bool,
}

impl RunConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: RunConfig = toml::from_str(input)?;
        let sync_names: Vec<SyncName> = config.sync.iter().map(|s| s.name.clone()).collect();
        config
            .postprocess
            .validate(&sync_names)
            .map_err(ConfigError::PostprocessConfig)?;
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string(self).map_err(|e| ConfigError::TomlSerialize(e.to_string()))
    }

    pub fn sync_map(&self) -> BTreeMap<&str, &RunConfigSync> {
        self.sync.iter().map(|s| (s.name.as_str(), s)).collect()
    }

    pub fn validate_sync_scoped_rules(&self) -> Result<(), String> {
        let sync_names: Vec<SyncName> = self.sync.iter().map(|s| s.name.clone()).collect();
        self.postprocess.validate(&sync_names)
    }

    pub fn validate_uploaded_purgatory_run(&self) -> Result<(), String> {
        self.validate_sync_scoped_rules()?;
        for sync in &self.sync {
            if !sync.delete_after_import {
                return Err(format!(
                    "sync group '{}' in purgatory run config has delete_after_import = false; \
                     postprocessing transforms the original and the server does not retain \
                     indefinite source metadata, so confirmed originals must be retired \
                     locally (import-and-retire)",
                    sync.name.as_str()
                ));
            }
        }
        Ok(())
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
pub struct SyncDestination {
    pub sync_name: String,
    pub passthrough_dest: String,
    pub purgatory_dest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRunResponse {
    pub protocol_version: u32,
    pub nickname: String,
    pub run_id: String,
    pub destinations: Vec<SyncDestination>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveDestinationsResponse {
    pub protocol_version: u32,
    pub nickname: String,
    pub destinations: Vec<SyncPassthroughDestination>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPassthroughDestination {
    pub sync_name: String,
    pub passthrough_dest: String,
}
