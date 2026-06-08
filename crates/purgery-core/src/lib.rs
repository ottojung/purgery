use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use thiserror::Error;

// ── Error Types ──────────────────────────────────────────────────────

#[derive(Error, Debug, PartialEq, Eq)]
pub enum NicknameError {
    #[error("nickname is empty")]
    Empty,
    #[error("nickname contains invalid character: {0:?}")]
    InvalidCharacter(char),
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum RunIdError {
    #[error("run ID is empty")]
    Empty,
    #[error("run ID contains invalid character: {0:?}")]
    InvalidCharacter(char),
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum PathValidationError {
    #[error("path is not absolute")]
    NotAbsolute,
    #[error("path is not relative")]
    NotRelative,
    #[error("path contains '..' component")]
    ContainsDotDot,
    #[error("path contains empty component")]
    EmptyComponent,
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("failed to parse config TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("invalid nickname: {0}")]
    Nickname(#[from] NicknameError),
    #[error("invalid path: {0}")]
    Path(#[from] PathValidationError),
    #[error("invalid run ID: {0}")]
    RunId(#[from] RunIdError),
    #[error("missing field: {0}")]
    MissingField(&'static str),
}

#[derive(Error, Debug)]
pub enum ManifestError {
    #[error("failed to parse manifest TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("invalid run ID: {0}")]
    RunId(#[from] RunIdError),
    #[error("invalid nickname: {0}")]
    Nickname(#[from] NicknameError),
    #[error("manifest has no files")]
    NoFiles,
}

#[derive(Error, Debug)]
pub enum StatusError {
    #[error("failed to parse status TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("invalid run ID: {0}")]
    RunId(#[from] RunIdError),
    #[error("invalid nickname: {0}")]
    Nickname(#[from] NicknameError),
    #[error("unknown file status value: {0}")]
    UnknownFileStatus(String),
}

// ── Validated Newtypes ───────────────────────────────────────────────

/// A validated client or machine nickname.
///
/// # Invariants
/// * Non-empty.
/// * Contains only ASCII alphanumeric characters, hyphens, and underscores.
///
/// # Proof of invariants
/// * `Nickname::new(s)`: returns `NicknameError::Empty` if `s` is empty
///   and `NicknameError::InvalidCharacter` for any disallowed character.
/// * `Nickname::from_str(s)`: delegates to `new(s.to_owned())`.
/// * Custom `Deserialize` impl: delegates to `new()`.
/// * Custom `Serialize` impl: writes the inner `&str`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Nickname(String);

impl Nickname {
    pub fn new(s: String) -> Result<Self, NicknameError> {
        if s.is_empty() {
            return Err(NicknameError::Empty);
        }
        for ch in s.chars() {
            if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' {
                return Err(NicknameError::InvalidCharacter(ch));
            }
        }
        Ok(Nickname(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for Nickname {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Nickname {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Nickname::new(s).map_err(serde::de::Error::custom)
    }
}

impl FromStr for Nickname {
    type Err = NicknameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Nickname::new(s.to_owned())
    }
}

/// A validated run identifier.
///
/// # Invariants
/// * Non-empty.
/// * Contains only ASCII alphanumeric, hyphens, underscores, and dots.
///
/// # Proof of invariants
/// * `RunId::new(s)`: returns `RunIdError::Empty` or `RunIdError::InvalidCharacter`.
/// * Custom `Deserialize` impl: delegates to `new()`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(String);

impl RunId {
    pub fn new(s: String) -> Result<Self, RunIdError> {
        if s.is_empty() {
            return Err(RunIdError::Empty);
        }
        for ch in s.chars() {
            if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' && ch != '.' {
                return Err(RunIdError::InvalidCharacter(ch));
            }
        }
        Ok(RunId(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for RunId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        RunId::new(s).map_err(serde::de::Error::custom)
    }
}

impl FromStr for RunId {
    type Err = RunIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RunId::new(s.to_owned())
    }
}

/// An absolute server-side root path for final file storage.
///
/// # Invariants
/// * The path is absolute (starts with `/` on Unix).
///
/// # Proof of invariants
/// * `ServerRoot::new(p)`: returns `PathValidationError::NotAbsolute`
///   when `p.is_absolute()` is false.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRoot(Utf8PathBuf);

impl ServerRoot {
    pub fn new(path: Utf8PathBuf) -> Result<Self, PathValidationError> {
        if !path.is_absolute() {
            return Err(PathValidationError::NotAbsolute);
        }
        Ok(ServerRoot(path))
    }

    pub fn as_path(&self) -> &Utf8Path {
        &self.0
    }
}

impl Serialize for ServerRoot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ServerRoot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let p = Utf8PathBuf::deserialize(deserializer)?;
        ServerRoot::new(p).map_err(serde::de::Error::custom)
    }
}

/// An absolute server-side staging root for incoming uploads.
///
/// # Invariants
/// * The path is absolute.
///
/// # Proof of invariants
/// * `PurgeryRoot::new(p)`: validates `p.is_absolute()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeryRoot(Utf8PathBuf);

impl PurgeryRoot {
    pub fn new(path: Utf8PathBuf) -> Result<Self, PathValidationError> {
        if !path.is_absolute() {
            return Err(PathValidationError::NotAbsolute);
        }
        Ok(PurgeryRoot(path))
    }

    pub fn as_path(&self) -> &Utf8Path {
        &self.0
    }
}

impl Serialize for PurgeryRoot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PurgeryRoot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let p = Utf8PathBuf::deserialize(deserializer)?;
        PurgeryRoot::new(p).map_err(serde::de::Error::custom)
    }
}

/// A relative destination path within a sync mapping.
///
/// # Invariants
/// * Not an absolute path.
/// * Does not contain `..` components.
/// * No empty components.
///
/// # Proof of invariants
/// * `RelativeDestinationPath::new(p)`: validates all three invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelativeDestinationPath(Utf8PathBuf);

impl RelativeDestinationPath {
    pub fn new(path: Utf8PathBuf) -> Result<Self, PathValidationError> {
        if path.is_absolute() {
            return Err(PathValidationError::NotRelative);
        }
        validate_no_dotdot_or_empty(&path)?;
        Ok(RelativeDestinationPath(path))
    }

    pub fn as_path(&self) -> &Utf8Path {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Serialize for RelativeDestinationPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RelativeDestinationPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let p = Utf8PathBuf::deserialize(deserializer)?;
        RelativeDestinationPath::new(p).map_err(serde::de::Error::custom)
    }
}

/// A normalized relative path used for rule matching and final paths.
///
/// # Invariants
/// * Not absolute.
/// * No `..` components.
/// * No empty components.
///
/// # Proof of invariants
/// * `NormalizedRelativePath::new(p)`: validates all invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedRelativePath(Utf8PathBuf);

impl NormalizedRelativePath {
    pub fn new(path: Utf8PathBuf) -> Result<Self, PathValidationError> {
        if path.is_absolute() {
            return Err(PathValidationError::NotRelative);
        }
        validate_no_dotdot_or_empty(&path)?;
        Ok(NormalizedRelativePath(path))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn as_path(&self) -> &Utf8Path {
        &self.0
    }
}

impl Serialize for NormalizedRelativePath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for NormalizedRelativePath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let p = Utf8PathBuf::deserialize(deserializer)?;
        NormalizedRelativePath::new(p).map_err(serde::de::Error::custom)
    }
}

fn validate_no_dotdot_or_empty(path: &Utf8Path) -> Result<(), PathValidationError> {
    for component in path.components() {
        let comp = component.as_str();
        if comp == ".." {
            return Err(PathValidationError::ContainsDotDot);
        }
        if comp.is_empty() {
            return Err(PathValidationError::EmptyComponent);
        }
    }
    Ok(())
}

// ── Manifest File Identity ───────────────────────────────────────────

/// Identity of a local file at upload time.
///
/// # Invariants
/// * `size` is meaningful (no specific bounds enforced at construction).
/// * `sha256`, when present, is a hex-encoded SHA-256 string.
///
/// # Proof of invariants
/// * `sha256` is validated in `ManifestFileEntry` deserialization (caller).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFileIdentity {
    pub local_path: Utf8PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

// ── Manifest Types ───────────────────────────────────────────────────

/// A run manifest describing uploaded files and their metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub run_id: String,
    pub nickname: String,
    #[serde(default)]
    pub files: Vec<ManifestFileEntry>,
}

/// A single file entry within a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFileEntry {
    pub sync_name: String,
    pub local_path: String,
    pub staged_path: String,
    pub relative_path: String,
    pub size: u64,
    pub mtime_ns: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl ManifestFileEntry {
    pub fn identity(&self) -> ManifestFileIdentity {
        ManifestFileIdentity {
            local_path: Utf8PathBuf::from(&self.local_path),
            size: self.size,
            mtime_ns: self.mtime_ns,
            sha256: self.sha256.clone(),
        }
    }
}

// ── Status Types ─────────────────────────────────────────────────────

/// Processing status of an individual file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Imported,
    Failed,
    Skipped,
}

impl FileStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileStatus::Imported => "imported",
            FileStatus::Failed => "failed",
            FileStatus::Skipped => "skipped",
        }
    }
}

impl FromStr for FileStatus {
    type Err = StatusError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "imported" => Ok(FileStatus::Imported),
            "failed" => Ok(FileStatus::Failed),
            "skipped" => Ok(FileStatus::Skipped),
            other => Err(StatusError::UnknownFileStatus(other.to_owned())),
        }
    }
}

impl Serialize for FileStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_str().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FileStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        FileStatus::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Overall run state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    Done,
    Partial,
    Failed,
}

/// Status of a processed run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStatus {
    pub run_id: String,
    pub nickname: String,
    pub state: String,
    #[serde(default)]
    pub files: Vec<FileStatusEntry>,
}

/// Per-file status entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStatusEntry {
    pub sync_name: String,
    pub local_path: String,
    pub relative_path: String,
    pub status: FileStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postprocess: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Config Types ─────────────────────────────────────────────────────

/// Server configuration, loaded from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub root: Utf8PathBuf,
    pub purgery_root: Utf8PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<Utf8PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<Utf8PathBuf>,
    #[serde(default)]
    pub postprocess: PostprocessConfig,
}

/// Postprocessing configuration (server-side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostprocessConfig {
    #[serde(default = "default_max_parallel_jobs")]
    pub max_parallel_jobs: u32,
    #[serde(default)]
    pub steps: std::collections::BTreeMap<String, PostprocessStepDefinition>,
}

impl Default for PostprocessConfig {
    fn default() -> Self {
        PostprocessConfig {
            max_parallel_jobs: 1,
            steps: std::collections::BTreeMap::new(),
        }
    }
}

fn default_max_parallel_jobs() -> u32 {
    1
}

/// Definition of a postprocessing step on the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostprocessStepDefinition {
    pub kind: String,
    pub command: String,
}

/// Client configuration, loaded from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub nickname: String,
    pub server: ServerConnection,
    #[serde(default)]
    pub sync: Vec<SyncMapping>,
    #[serde(default)]
    pub postprocess: ClientPostprocessConfig,
}

/// Server connection details for the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConnection {
    pub host: String,
    pub purgery_root: String,
}

/// A single sync mapping from local path to server destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncMapping {
    pub name: String,
    #[serde(rename = "from")]
    pub from_path: String,
    #[serde(rename = "to")]
    pub to_path: String,
    #[serde(default = "default_delete_after_import")]
    pub delete_after_import: bool,
}

fn default_delete_after_import() -> bool {
    false
}

/// Client-side postprocessing rule configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientPostprocessConfig {
    #[serde(default)]
    pub rules: Vec<PostprocessRule>,
}

/// A postprocessing rule: files matching `match` regex get the listed steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostprocessRule {
    #[serde(rename = "match")]
    pub pattern: String,
    pub steps: Vec<String>,
}

// ── Parsing Helpers ──────────────────────────────────────────────────

impl ServerConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: ServerConfig = toml::from_str(input)?;
        Ok(config)
    }
}

impl ClientConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: ClientConfig = toml::from_str(input)?;
        Ok(config)
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Nickname tests ──

    #[test]
    fn nickname_valid() {
        let n = Nickname::new("laptop".into()).unwrap();
        assert_eq!(n.as_str(), "laptop");
    }

    #[test]
    fn nickname_valid_with_hyphen_underscore() {
        let n = Nickname::new("my-laptop_2".into()).unwrap();
        assert_eq!(n.as_str(), "my-laptop_2");
    }

    #[test]
    fn nickname_empty_is_error() {
        assert_eq!(Nickname::new("".into()), Err(NicknameError::Empty));
    }

    #[test]
    fn nickname_rejects_slash() {
        assert!(Nickname::new("lap/top".into()).is_err());
    }

    #[test]
    fn nickname_rejects_space() {
        assert!(Nickname::new("my laptop".into()).is_err());
    }

    #[test]
    fn nickname_serde_roundtrip() {
        let n = Nickname::new("desktop".into()).unwrap();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"desktop\"");
        let back: Nickname = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn nickname_serde_rejects_invalid() {
        let result: Result<Nickname, _> = serde_json::from_str("\"\"");
        assert!(result.is_err());
    }

    #[test]
    fn nickname_from_str() {
        let n: Nickname = "phone".parse().unwrap();
        assert_eq!(n.as_str(), "phone");
    }

    // ── RunId tests ──

    #[test]
    fn run_id_valid() {
        let r = RunId::new("2026-06-08T18-45-12Z-9f03".into()).unwrap();
        assert_eq!(r.as_str(), "2026-06-08T18-45-12Z-9f03");
    }

    #[test]
    fn run_id_empty_is_error() {
        assert_eq!(RunId::new("".into()), Err(RunIdError::Empty));
    }

    #[test]
    fn run_id_rejects_slash() {
        assert!(RunId::new("run/id".into()).is_err());
    }

    #[test]
    fn run_id_rejects_space() {
        assert!(RunId::new("run id".into()).is_err());
    }

    #[test]
    fn run_id_serde_roundtrip() {
        let r = RunId::new("abc-123.def".into()).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: RunId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    // ── ServerRoot tests ──

    #[test]
    fn server_root_absolute_valid() {
        let p = Utf8PathBuf::from("/universe/synced");
        let r = ServerRoot::new(p.clone()).unwrap();
        assert_eq!(r.as_path(), &p);
    }

    #[test]
    fn server_root_relative_is_error() {
        let p = Utf8PathBuf::from("relative/path");
        assert_eq!(ServerRoot::new(p), Err(PathValidationError::NotAbsolute));
    }

    #[test]
    fn server_root_serde_roundtrip() {
        let r = ServerRoot::new("/data".into()).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: ServerRoot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    // ── PurgeryRoot tests ──

    #[test]
    fn purgery_root_absolute_valid() {
        let p = Utf8PathBuf::from("/universe/tmp/purgery");
        let r = PurgeryRoot::new(p.clone()).unwrap();
        assert_eq!(r.as_path(), &p);
    }

    #[test]
    fn purgery_root_relative_is_error() {
        let p = Utf8PathBuf::from("tmp/purgery");
        assert_eq!(PurgeryRoot::new(p), Err(PathValidationError::NotAbsolute));
    }

    // ── RelativeDestinationPath tests ──

    #[test]
    fn relative_dest_valid() {
        let p = Utf8PathBuf::from("videos");
        let r = RelativeDestinationPath::new(p.clone()).unwrap();
        assert_eq!(r.as_path(), &p);
    }

    #[test]
    fn relative_dest_with_subdir() {
        let p = Utf8PathBuf::from("media/videos");
        RelativeDestinationPath::new(p).unwrap();
    }

    #[test]
    fn relative_dest_absolute_is_error() {
        let p = Utf8PathBuf::from("/absolute/path");
        assert_eq!(
            RelativeDestinationPath::new(p),
            Err(PathValidationError::NotRelative)
        );
    }

    #[test]
    fn relative_dest_dotdot_is_error() {
        let p = Utf8PathBuf::from("../escape");
        assert_eq!(
            RelativeDestinationPath::new(p),
            Err(PathValidationError::ContainsDotDot)
        );
    }

    #[test]
    fn relative_dest_allows_consecutive_separators() {
        // camino treats consecutive separators as one, so "a//b" is just ["a", "b"]
        let p = Utf8PathBuf::from("a//b");
        assert!(RelativeDestinationPath::new(p).is_ok());
    }

    // ── NormalizedRelativePath tests ──

    #[test]
    fn normalized_path_valid() {
        let p = Utf8PathBuf::from("videos/a.mp4");
        let n = NormalizedRelativePath::new(p.clone()).unwrap();
        assert_eq!(n.as_str(), "videos/a.mp4");
    }

    #[test]
    fn normalized_path_dotdot_is_error() {
        let p = Utf8PathBuf::from("videos/../../etc");
        assert_eq!(
            NormalizedRelativePath::new(p),
            Err(PathValidationError::ContainsDotDot)
        );
    }

    // ── FileStatus tests ──

    #[test]
    fn file_status_imported() {
        assert_eq!(
            FileStatus::from_str("imported").unwrap(),
            FileStatus::Imported
        );
        assert_eq!(FileStatus::Imported.as_str(), "imported");
    }

    #[test]
    fn file_status_failed() {
        assert_eq!(FileStatus::from_str("failed").unwrap(), FileStatus::Failed);
        assert_eq!(FileStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn file_status_skipped() {
        assert_eq!(
            FileStatus::from_str("skipped").unwrap(),
            FileStatus::Skipped
        );
        assert_eq!(FileStatus::Skipped.as_str(), "skipped");
    }

    #[test]
    fn file_status_unknown_is_error() {
        let result = FileStatus::from_str("unknown-status");
        assert!(result.is_err());
    }

    #[test]
    fn file_status_serde_roundtrip() {
        let s = FileStatus::Imported;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"imported\"");
        let back: FileStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, FileStatus::Imported);
    }

    // ── Config parsing tests ──

    #[test]
    fn parse_server_config_minimal() {
        let toml = r#"
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"
"#;
        let config = ServerConfig::from_toml(toml).unwrap();
        assert_eq!(config.root.as_str(), "/universe/synced");
        assert_eq!(config.purgery_root.as_str(), "/universe/tmp/purgery");
        assert_eq!(config.postprocess.max_parallel_jobs, 1);
        assert!(config.state_dir.is_none());
        assert!(config.log_dir.is_none());
    }

    #[test]
    fn parse_server_config_full() {
        let toml = r#"
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"
state_dir = "/var/lib/purgery"
log_dir = "/var/log/purgery"

[postprocess]
max_parallel_jobs = 2

[postprocess.steps.compress-video]
kind = "builtin"
command = "my-compress-video"
"#;
        let config = ServerConfig::from_toml(toml).unwrap();
        assert_eq!(config.root.as_str(), "/universe/synced");
        assert_eq!(config.state_dir.unwrap().as_str(), "/var/lib/purgery");
        assert_eq!(config.postprocess.max_parallel_jobs, 2);
        let step = config.postprocess.steps.get("compress-video").unwrap();
        assert_eq!(step.kind, "builtin");
        assert_eq!(step.command, "my-compress-video");
    }

    #[test]
    fn parse_client_config_minimal() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/universe/tmp/purgery"
"#;
        let config = ClientConfig::from_toml(toml).unwrap();
        assert_eq!(config.nickname, "laptop");
        assert_eq!(config.server.host, "example.com");
        assert!(config.sync.is_empty());
    }

    #[test]
    fn parse_client_config_full() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/universe/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/vitalik/Videos"
to = "videos"
delete_after_import = true

[[sync]]
name = "pictures"
from = "/home/vitalik/Pictures"
to = "pictures"

[[postprocess.rules]]
match = '^videos/.*\.(mp4|mov|mkv|webm)$'
steps = ["compress-video"]
"#;
        let config = ClientConfig::from_toml(toml).unwrap();
        assert_eq!(config.sync.len(), 2);
        assert_eq!(config.sync[0].name, "videos");
        assert!(config.sync[0].delete_after_import);
        assert!(!config.sync[1].delete_after_import);
        assert_eq!(config.postprocess.rules.len(), 1);
        assert_eq!(config.postprocess.rules[0].steps, vec!["compress-video"]);
    }

    #[test]
    fn parse_invalid_toml_is_error() {
        let result = ServerConfig::from_toml("not valid toml {{{");
        assert!(result.is_err());
    }

    // ── Manifest tests ──

    #[test]
    fn parse_manifest() {
        let toml = r#"
run_id = "2026-06-08T18-45-12Z-9f03"
nickname = "laptop"

[[files]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
staged_path = "files/videos/a.mp4"
relative_path = "a.mp4"
size = 123456789
mtime_ns = 1780944312000000000
sha256 = "abcd1234"
"#;
        let manifest: Manifest = toml::from_str(toml).unwrap();
        assert_eq!(manifest.run_id, "2026-06-08T18-45-12Z-9f03");
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].sha256.as_deref(), Some("abcd1234"));
    }

    #[test]
    fn parse_manifest_without_sha256() {
        let toml = r#"
run_id = "2026-06-08T18-45-12Z-9f03"
nickname = "laptop"

[[files]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
staged_path = "files/videos/a.mp4"
relative_path = "a.mp4"
size = 123456789
mtime_ns = 1780944312000000000
"#;
        let manifest: Manifest = toml::from_str(toml).unwrap();
        assert!(manifest.files[0].sha256.is_none());
    }

    // ── Status tests ──

    #[test]
    fn parse_status() {
        let toml = r#"
run_id = "2026-06-08T18-45-12Z-9f03"
nickname = "laptop"
state = "done"

[[files]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
relative_path = "a.mp4"
status = "imported"
final_path = "laptop/videos/a.mp4"
postprocess = ["compress-video"]

[[files]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/b.mp4"
relative_path = "b.mp4"
status = "failed"
error = "compress-video failed"
"#;
        let status: RunStatus = toml::from_str(toml).unwrap();
        assert_eq!(status.state, "done");
        assert_eq!(status.files.len(), 2);
        assert_eq!(status.files[0].status, FileStatus::Imported);
        assert_eq!(
            status.files[0].final_path.as_deref(),
            Some("laptop/videos/a.mp4")
        );
        assert_eq!(status.files[1].status, FileStatus::Failed);
        assert_eq!(
            status.files[1].error.as_deref(),
            Some("compress-video failed")
        );
    }

    #[test]
    fn identity_from_entry() {
        let entry = ManifestFileEntry {
            sync_name: "videos".into(),
            local_path: "/home/vitalik/Videos/a.mp4".into(),
            staged_path: "files/videos/a.mp4".into(),
            relative_path: "a.mp4".into(),
            size: 100,
            mtime_ns: 200,
            sha256: Some("abc".into()),
        };
        let identity = entry.identity();
        assert_eq!(identity.size, 100);
        assert_eq!(identity.mtime_ns, 200);
        assert_eq!(identity.sha256, Some("abc".into()));
    }
}
