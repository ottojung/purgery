use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use thiserror::Error;

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

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ServerRootsError {
    #[error("at least one named root is required")]
    Empty,
    #[error("duplicate root name '{0}'")]
    Duplicate(RootName),
    #[error("invalid root name: {0}")]
    InvalidName(#[from] RootNameError),
    #[error("unknown server root '{0}'")]
    Unknown(RootName),
}

#[derive(Debug, Clone)]
pub struct ServerRoots(BTreeMap<RootName, ServerRoot>);

impl ServerRoots {
    pub fn single(name: &str, path: ServerRoot) -> Result<Self, ServerRootsError> {
        let name = RootName::new(name.to_owned())?;
        Self::new(vec![NamedRoot { name, path }])
    }

    pub fn new(roots: Vec<NamedRoot>) -> Result<Self, ServerRootsError> {
        if roots.is_empty() {
            return Err(ServerRootsError::Empty);
        }
        let mut names = BTreeSet::new();
        let mut by_name = BTreeMap::new();
        for root in roots {
            if !names.insert(root.name.clone()) {
                return Err(ServerRootsError::Duplicate(root.name));
            }
            by_name.insert(root.name, root.path);
        }
        Ok(Self(by_name))
    }

    pub fn get(&self, name: &RootName) -> Option<&ServerRoot> {
        self.0.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RootName, &ServerRoot)> {
        self.0.iter()
    }

    pub fn resolve_archive_dir(
        &self,
        dest: &ClientDest,
    ) -> Result<camino::Utf8PathBuf, ServerRootsError> {
        let root = self
            .get(dest.root_name())
            .ok_or_else(|| ServerRootsError::Unknown(dest.root_name().clone()))?;
        Ok(match dest.path_under_root() {
            Some(path) => root.as_path().join(path.as_path()),
            None => root.as_path().to_owned(),
        })
    }

    pub fn resolve_final_path(
        &self,
        dest: &ClientDest,
        relative_path: &NormalizedRelativePath,
    ) -> Result<camino::Utf8PathBuf, ServerRootsError> {
        Ok(self
            .resolve_archive_dir(dest)?
            .join(relative_path.as_path()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfigFile {
    pub work_dir: PurgeryRoot,
    #[serde(rename = "root")]
    pub roots: Vec<NamedRoot>,
    #[serde(default)]
    pub postprocess: PostprocessConfig,
    #[serde(default)]
    pub gc: GCConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub work_dir: PurgeryRoot,
    pub roots: ServerRoots,
    pub postprocess: PostprocessConfig,
    pub gc: GCConfig,
    pub logging: LoggingConfig,
}

impl ServerConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: ServerConfigFile = toml::from_str(input)?;
        let roots = ServerRoots::new(config.roots)?;
        Ok(Self {
            work_dir: config.work_dir,
            roots,
            postprocess: config.postprocess,
            gc: config.gc,
            logging: config.logging,
        })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClientRootsError {
    #[error("at least one named client root is required")]
    Empty,
    #[error("duplicate client root name '{0}'")]
    Duplicate(ClientRootName),
    #[error("client root '{name}' path must be absolute: {path}")]
    RelativePath { name: ClientRootName, path: String },
    #[error("unknown client root '{0}'")]
    Unknown(ClientRootName),
}

/// A named absolute client source tree. Serde validates the root name; the
/// `ClientRoots` collection establishes absolute-path and uniqueness proofs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRoot {
    pub name: ClientRootName,
    pub path: LocalSourcePath,
}

/// Validated client roots keyed by name.
#[derive(Debug, Clone)]
pub struct ClientRoots(BTreeMap<ClientRootName, LocalSourcePath>);

impl ClientRoots {
    pub fn new(roots: Vec<ClientRoot>) -> Result<Self, ClientRootsError> {
        if roots.is_empty() {
            return Err(ClientRootsError::Empty);
        }
        let mut by_name = BTreeMap::new();
        for root in roots {
            if !root.path.as_str().starts_with('/') {
                return Err(ClientRootsError::RelativePath {
                    name: root.name,
                    path: root.path.as_str().to_owned(),
                });
            }
            if by_name.insert(root.name.clone(), root.path).is_some() {
                return Err(ClientRootsError::Duplicate(root.name));
            }
        }
        Ok(Self(by_name))
    }

    pub fn resolve(&self, source: &ClientSource) -> Result<LocalSourcePath, ClientRootsError> {
        let root = self
            .0
            .get(source.root_name())
            .ok_or_else(|| ClientRootsError::Unknown(source.root_name().clone()))?;
        let path = match source.path_under_root() {
            Some(rest) => camino::Utf8Path::new(root.as_str()).join(rest.as_path()),
            None => camino::Utf8PathBuf::from(root.as_str()),
        };
        LocalSourcePath::new(path.into_string()).map_err(|_| ClientRootsError::RelativePath {
            name: source.root_name().clone(),
            path: root.as_str().to_owned(),
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ClientRootName, &LocalSourcePath)> {
        self.0.iter()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientConfigFile {
    nickname: Nickname,
    server: ServerConnection,
    #[serde(rename = "root")]
    roots: Vec<ClientRoot>,
    #[serde(default)]
    sync: Vec<SyncMappingFile>,
    #[serde(default)]
    logging: LoggingConfig,
    state_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncMappingFile {
    #[serde(rename = "from")]
    source: ClientSource,
    #[serde(rename = "to")]
    to_path: ClientDest,
    #[serde(rename = "match")]
    match_pattern: Option<String>,
    postprocess: Option<Vec<String>>,
    #[serde(default = "default_delete_after_import")]
    delete_after_import: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub nickname: Nickname,
    pub server: ServerConnection,
    pub roots: Vec<ClientRoot>,
    pub sync: Vec<SyncMapping>,
    #[serde(skip)]
    pub postprocess: ClientPostprocessConfig,
    pub logging: LoggingConfig,
    pub state_dir: String,
}

impl ClientConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let file: ClientConfigFile = toml::from_str(input)?;
        if file.state_dir.is_empty() {
            return Err(ConfigError::StateDir("must be non-empty".into()));
        }
        if !file.state_dir.starts_with('/') {
            return Err(ConfigError::StateDir("must be an absolute path".into()));
        }
        let roots = ClientRoots::new(file.roots.clone())?;
        let mut rules = Vec::new();
        let mut sync = Vec::with_capacity(file.sync.len());
        for (index, mapping) in file.sync.into_iter().enumerate() {
            if let Some(steps) = &mapping.postprocess {
                if steps.is_empty() {
                    return Err(ConfigError::PostprocessConfig(
                        "postprocess must be a non-empty list".into(),
                    ));
                }
                if !mapping.delete_after_import {
                    return Err(ConfigError::PostprocessConfig(
                        "postprocess requires delete_after_import = true".into(),
                    ));
                }
            }
            let name = SyncName::new(format!("sync-{:04}", index + 1))?;
            let from_path = roots.resolve(&mapping.source)?;
            if let Some(steps) = &mapping.postprocess {
                rules.push(PostprocessRule {
                    pattern: mapping.match_pattern.clone().unwrap_or_else(|| "**".into()),
                    steps: steps.clone(),
                    sync_names: Some(vec![name.clone()]),
                });
            }
            sync.push(SyncMapping {
                name,
                source: mapping.source,
                from_path,
                to_path: mapping.to_path,
                match_pattern: mapping.match_pattern,
                postprocess_steps: mapping.postprocess.unwrap_or_default(),
                delete_after_import: mapping.delete_after_import,
            });
        }
        Ok(Self {
            nickname: file.nickname,
            server: file.server,
            roots: file.roots,
            sync,
            postprocess: ClientPostprocessConfig { rules },
            logging: file.logging,
            state_dir: file.state_dir,
        })
    }

    pub fn find_sync(&self, name: &str) -> Option<&SyncMapping> {
        self.sync.iter().find(|sync| sync.name.as_str() == name)
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

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SyncMapping {
    pub name: SyncName,
    #[serde(rename = "from")]
    pub source: ClientSource,
    #[serde(skip)]
    pub from_path: LocalSourcePath,
    #[serde(rename = "to")]
    pub to_path: ClientDest,
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_pattern: Option<String>,
    #[serde(rename = "postprocess", skip_serializing_if = "Vec::is_empty")]
    pub postprocess_steps: Vec<String>,
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
            Some(names) => names.iter().any(|name| name.as_str() == sync_name),
        }
    }
}

pub fn applicable_rules<'a>(
    rules: &'a [PostprocessRule],
    sync_name: &str,
) -> Vec<&'a PostprocessRule> {
    rules
        .iter()
        .filter(|rule| rule.applies_to(sync_name))
        .collect()
}

impl ClientPostprocessConfig {
    pub fn validate(&self, sync_names: &[SyncName]) -> Result<(), String> {
        for rule in &self.rules {
            if rule.steps.is_empty() {
                return Err("postprocess selection has no steps".into());
            }
            if let Some(names) = &rule.sync_names {
                if names.is_empty() {
                    return Err("postprocess selection has no sync IDs".into());
                }
                for name in names {
                    if !sync_names.contains(name) {
                        return Err(format!(
                            "postprocess selection references unknown sync ID '{}'",
                            name
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn validate_delete_after_import(&self, syncs: &[SyncMapping]) -> Result<(), String> {
        for sync in syncs {
            if !sync.postprocess_steps.is_empty() && !sync.delete_after_import {
                return Err(format!(
                    "sync '{}' selects postprocess steps without cleanup",
                    sync.name
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
    _rules: &'a [PostprocessRule],
) -> Result<Vec<(SyncExecutionClass, &'a SyncMapping)>, String> {
    syncs
        .iter()
        .map(|sync| {
            if !sync.postprocess_steps.is_empty() && !sync.delete_after_import {
                return Err(format!(
                    "sync '{}' selects postprocess steps without cleanup",
                    sync.name
                ));
            }
            let class = if !sync.postprocess_steps.is_empty() {
                SyncExecutionClass::Purgatory
            } else if sync.delete_after_import {
                SyncExecutionClass::PassthroughDeleteAfterImport
            } else {
                SyncExecutionClass::PassthroughNoDelete
            };
            Ok((class, sync))
        })
        .collect()
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
    pub to_path: ClientDest,
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
            let has_postprocess = self
                .postprocess
                .rules
                .iter()
                .any(|rule| rule.applies_to(sync.name.as_str()));
            if has_postprocess && !sync.delete_after_import {
                return Err(format!(
                    "sync group '{}' selects postprocess steps but delete_after_import = false; postprocessing requires import-and-retire cleanup",
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
    pub current_step: Option<String>,
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
    pub current_step: String,
    pub started_at_unix_secs: u64,
    pub updated_at_unix_secs: u64,
}

/// Progress update passed into postprocess callbacks.
/// Carries full entry context for progress reporting.
#[derive(Debug, Clone)]
pub struct ProgressUpdate<'a> {
    pub state: &'a str,
    pub entry_index: usize,
    pub entry_total: usize,
    pub current_entry: &'a str,
    pub current_step: &'a str,
}

impl<'a> ProgressUpdate<'a> {
    pub fn new(
        state: &'a str,
        entry_index: usize,
        entry_total: usize,
        current_entry: &'a str,
        current_step: &'a str,
    ) -> Self {
        ProgressUpdate {
            state,
            entry_index,
            entry_total,
            current_entry,
            current_step,
        }
    }
}

/// Client-persisted phases for a postprocess run.
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

/// Client-persisted state for a postprocess run, used to resume
/// waiting and cleanup after a crash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRunState {
    pub protocol_version: u32,
    pub nickname: String,
    pub run_id: String,
    pub manifest: String,
    pub run_config: String,
    pub phase: ClientRunPhase,
}
