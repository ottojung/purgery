use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::io;
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
pub enum SyncNameError {
    #[error("sync name is empty")]
    Empty,
    #[error("sync name contains invalid character: {0:?}")]
    InvalidCharacter(char),
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum RemoteHostError {
    #[error("remote host is empty")]
    Empty,
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum LocalSourcePathError {
    #[error("local source path is empty")]
    Empty,
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
    #[error("invalid sync name: {0}")]
    SyncName(#[from] SyncNameError),
    #[error("invalid host: {0}")]
    RemoteHost(#[from] RemoteHostError),
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
    #[error("failed to serialize manifest: {0}")]
    TomlSerialize(String),
    #[error("invalid run ID: {0}")]
    RunId(#[from] RunIdError),
    #[error("invalid nickname: {0}")]
    Nickname(#[from] NicknameError),
    #[error("invalid path: {0}")]
    Path(#[from] PathValidationError),
    #[error("manifest has no files")]
    NoFiles,
}

#[derive(Error, Debug)]
pub enum StatusError {
    #[error("failed to parse status TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("failed to serialize status: {0}")]
    TomlSerialize(String),
    #[error("invalid run ID: {0}")]
    RunId(#[from] RunIdError),
    #[error("invalid nickname: {0}")]
    Nickname(#[from] NicknameError),
    #[error("unknown file status value: {0}")]
    UnknownFileStatus(String),
    #[error("unknown run state value: {0}")]
    UnknownRunState(String),
}

#[derive(Error, Debug)]
pub enum IdentityVerificationError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("file not found: {0}")]
    NotFound(Utf8PathBuf),
    #[error("size mismatch: expected {expected}, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("SHA-256 mismatch")]
    Sha256Mismatch,
    #[error("mtime unavailable")]
    MtimeUnavailable,
}

// ── Run Phase ────────────────────────────────────────────────────────

/// Phase of a run in the purgery staging lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    Incoming,
    Ready,
    Processing,
    Done,
    Failed,
}

impl RunPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunPhase::Incoming => "incoming",
            RunPhase::Ready => "ready",
            RunPhase::Processing => "processing",
            RunPhase::Done => "done",
            RunPhase::Failed => "failed",
        }
    }
}

// ── Validated Newtypes ───────────────────────────────────────────────

/// A validated client or machine nickname.
///
/// # Invariants
/// * Non-empty.
/// * Contains only ASCII alphanumeric characters, hyphens, and underscores.
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

/// A validated sync mapping name.
///
/// # Invariants
/// * Non-empty.
/// * Contains only ASCII alphanumeric, hyphens, and underscores.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyncName(String);

impl SyncName {
    pub fn new(s: String) -> Result<Self, SyncNameError> {
        if s.is_empty() {
            return Err(SyncNameError::Empty);
        }
        for ch in s.chars() {
            if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' {
                return Err(SyncNameError::InvalidCharacter(ch));
            }
        }
        Ok(SyncName(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SyncName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SyncName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SyncName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        SyncName::new(s).map_err(serde::de::Error::custom)
    }
}

/// A validated SSH host name.
///
/// # Invariants
/// * Non-empty.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteHost(String);

impl RemoteHost {
    pub fn new(s: String) -> Result<Self, RemoteHostError> {
        if s.is_empty() {
            return Err(RemoteHostError::Empty);
        }
        Ok(RemoteHost(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for RemoteHost {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RemoteHost {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        RemoteHost::new(s).map_err(serde::de::Error::custom)
    }
}

/// A validated local source path.
///
/// # Invariants
/// * Non-empty.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalSourcePath(String);

impl LocalSourcePath {
    pub fn new(s: String) -> Result<Self, LocalSourcePathError> {
        if s.is_empty() {
            return Err(LocalSourcePathError::Empty);
        }
        Ok(LocalSourcePath(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for LocalSourcePath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LocalSourcePath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        LocalSourcePath::new(s).map_err(serde::de::Error::custom)
    }
}

/// A validated run identifier.
///
/// # Invariants
/// * Non-empty.
/// * Contains only ASCII alphanumeric, hyphens, underscores, and dots.
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

    pub fn generate() -> Self {
        let now = chrono_utc_now_formatted();
        let suffix = random_four_hex();
        RunId(format!("{now}-{suffix}"))
    }
}

fn chrono_utc_now_formatted() -> String {
    use std::time::SystemTime;
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    let mut y = 1970i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let months_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0;
    for (i, &md) in months_days.iter().enumerate() {
        if remaining < md {
            m = i + 1;
            break;
        }
        remaining -= md;
    }
    if m == 0 {
        m = 12;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z",
        y,
        m,
        remaining + 1,
        hours,
        minutes,
        seconds
    )
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn random_four_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:04x}", (nanos & 0xFFFF) as u16)
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

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Compute the final destination path for a file.
    ///
    /// Returns `root / nickname / sync_to / rel_path`.
    /// This does NOT verify escape safety; the caller should validate
    /// with `path_is_within_root` after resolution if needed.
    pub fn final_path(
        &self,
        nickname: &Nickname,
        sync_to: &RelativeDestinationPath,
        rel_path: &NormalizedRelativePath,
    ) -> Utf8PathBuf {
        self.0
            .join(nickname.as_str())
            .join(sync_to.as_path())
            .join(rel_path.as_path())
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

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn nickname_dir(&self, nickname: &Nickname) -> Utf8PathBuf {
        self.0.join(nickname.as_str())
    }

    pub fn run_dir(&self, nickname: &Nickname, run_id: &RunId, phase: RunPhase) -> Utf8PathBuf {
        self.nickname_dir(nickname)
            .join(phase.as_str())
            .join(run_id.as_str())
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
    pub run_id: RunId,
    pub nickname: Nickname,
    #[serde(default)]
    pub files: Vec<ManifestFileEntry>,
}

/// A single file entry within a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFileEntry {
    pub sync_name: String,
    pub local_path: String,
    pub staged_path: NormalizedRelativePath,
    pub relative_path: NormalizedRelativePath,
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

    pub fn verify_staged(&self, staged_path: &Utf8Path) -> Result<(), IdentityVerificationError> {
        let metadata = std::fs::metadata(staged_path.as_std_path()).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                IdentityVerificationError::NotFound(staged_path.to_owned())
            } else {
                IdentityVerificationError::Io(e)
            }
        })?;

        let actual_size = metadata.len();
        if actual_size != self.size {
            return Err(IdentityVerificationError::SizeMismatch {
                expected: self.size,
                actual: actual_size,
            });
        }

        if let Some(ref expected_sha) = self.sha256 {
            let actual_sha = compute_sha256(staged_path)?;
            if &actual_sha != expected_sha {
                return Err(IdentityVerificationError::Sha256Mismatch);
            }
        }

        Ok(())
    }
}

/// Compute SHA-256 hex string for a file.
pub fn compute_sha256(path: &Utf8Path) -> Result<String, io::Error> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path.as_std_path())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        use std::io::Read;
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Done => "done",
            RunState::Partial => "partial",
            RunState::Failed => "failed",
        }
    }
}

impl FromStr for RunState {
    type Err = StatusError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "done" => Ok(RunState::Done),
            "partial" => Ok(RunState::Partial),
            "failed" => Ok(RunState::Failed),
            other => Err(StatusError::UnknownRunState(other.to_owned())),
        }
    }
}

impl Serialize for RunState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_str().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RunState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        RunState::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Status of a processed run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStatus {
    pub run_id: RunId,
    pub nickname: Nickname,
    pub state: RunState,
    #[serde(default)]
    pub files: Vec<FileStatusEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    pub root: ServerRoot,
    pub purgery_root: PurgeryRoot,
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
///
/// `args` may contain `{path}` which is substituted with the final file path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostprocessStepDefinition {
    pub kind: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Client configuration, loaded from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub nickname: Nickname,
    pub server: ServerConnection,
    #[serde(default)]
    pub sync: Vec<SyncMapping>,
    #[serde(default)]
    pub postprocess: ClientPostprocessConfig,
}

impl ClientConfig {
    pub fn find_sync(&self, name: &str) -> Option<&SyncMapping> {
        self.sync.iter().find(|s| s.name.as_str() == name)
    }
}

/// Server connection details for the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConnection {
    pub host: RemoteHost,
    pub purgery_root: PurgeryRoot,
}

/// A single sync mapping from local path to server destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Manifest {
    pub fn from_toml(input: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest = toml::from_str(input)?;
        if manifest.files.is_empty() {
            return Err(ManifestError::NoFiles);
        }
        Ok(manifest)
    }

    pub fn to_toml(&self) -> Result<String, ManifestError> {
        toml::to_string(self).map_err(|e| ManifestError::TomlSerialize(e.to_string()))
    }
}

impl RunStatus {
    pub fn from_toml(input: &str) -> Result<Self, StatusError> {
        let status: RunStatus = toml::from_str(input)?;
        Ok(status)
    }

    pub fn to_toml(&self) -> Result<String, StatusError> {
        toml::to_string(self).map_err(|e| StatusError::TomlSerialize(e.to_string()))
    }
}

// ── Path Safety ──────────────────────────────────────────────────────

/// Check that a resolved path is within the root.
pub fn path_is_within_root(resolved: &Utf8Path, root: &Utf8Path) -> bool {
    resolved.starts_with(root)
}

// ── Envelope Validation ─────────────────────────────────────────────

/// Validate that the directory envelope matches the run's metadata.
///
/// Checks:
/// * `dir_nickname` == `config.nickname`
/// * `dir_nickname` == `manifest.nickname`
/// * `dir_run_id` == `manifest.run_id`
pub fn validate_envelope(
    dir_nickname: &Nickname,
    dir_run_id: &RunId,
    config: &ClientConfig,
    manifest: &Manifest,
) -> Result<(), String> {
    if config.nickname != *dir_nickname {
        return Err(format!(
            "directory nickname '{}' does not match config nickname '{}'",
            dir_nickname.as_str(),
            config.nickname.as_str()
        ));
    }
    if manifest.nickname != *dir_nickname {
        return Err(format!(
            "manifest nickname '{}' does not match directory nickname '{}'",
            manifest.nickname.as_str(),
            dir_nickname.as_str()
        ));
    }
    if manifest.run_id != *dir_run_id {
        return Err(format!(
            "manifest run_id '{}' does not match directory run_id '{}'",
            manifest.run_id.as_str(),
            dir_run_id.as_str()
        ));
    }
    Ok(())
}

// ── Shell Escaping (shared by client and server) ─────────────────────

/// Escape a string for use as a single-quoted shell argument.
///
/// Wraps the string in single quotes and handles embedded single quotes
/// according to POSIX shell rules: `'` → `'\''`.
pub fn shell_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len() + 2);
    escaped.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── SyncName tests ──

    #[test]
    fn sync_name_valid() {
        let n = SyncName::new("videos".into()).unwrap();
        assert_eq!(n.as_str(), "videos");
    }

    #[test]
    fn sync_name_empty_is_error() {
        assert!(SyncName::new("".into()).is_err());
    }

    #[test]
    fn sync_name_rejects_slash() {
        assert!(SyncName::new("bad/name".into()).is_err());
    }

    // ── RemoteHost tests ──

    #[test]
    fn remote_host_valid() {
        let h = RemoteHost::new("example.com".into()).unwrap();
        assert_eq!(h.as_str(), "example.com");
    }

    #[test]
    fn remote_host_empty_is_error() {
        assert!(RemoteHost::new("".into()).is_err());
    }

    // ── LocalSourcePath tests ──

    #[test]
    fn local_source_path_valid() {
        let p = LocalSourcePath::new("/home/user/Videos".into()).unwrap();
        assert_eq!(p.as_str(), "/home/user/Videos");
    }

    #[test]
    fn local_source_path_empty_is_error() {
        assert!(LocalSourcePath::new("".into()).is_err());
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

    #[test]
    fn run_id_generates_valid() {
        let id = RunId::generate();
        assert!(!id.as_str().is_empty());
        assert!(id.as_str().contains('T'));
        assert!(id.as_str().contains('Z'));
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

    #[test]
    fn server_root_final_path() {
        let root = ServerRoot::new("/universe/synced".into()).unwrap();
        let nick = Nickname::new("laptop".into()).unwrap();
        let dest = RelativeDestinationPath::new("videos".into()).unwrap();
        let rel = NormalizedRelativePath::new("a.mp4".into()).unwrap();
        assert_eq!(
            root.final_path(&nick, &dest, &rel),
            Utf8PathBuf::from("/universe/synced/laptop/videos/a.mp4")
        );
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

    #[test]
    fn purgery_root_path_helpers() {
        let root = PurgeryRoot::new("/tmp/purgery".into()).unwrap();
        let nick = Nickname::new("laptop".into()).unwrap();
        let run = RunId::new("run1".into()).unwrap();
        assert_eq!(
            root.nickname_dir(&nick),
            Utf8PathBuf::from("/tmp/purgery/laptop")
        );
        assert_eq!(
            root.run_dir(&nick, &run, RunPhase::Incoming),
            Utf8PathBuf::from("/tmp/purgery/laptop/incoming/run1")
        );
        assert_eq!(
            root.run_dir(&nick, &run, RunPhase::Ready),
            Utf8PathBuf::from("/tmp/purgery/laptop/ready/run1")
        );
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
        let p = Utf8PathBuf::from("a//b");
        assert!(RelativeDestinationPath::new(p).is_ok());
    }

    /// `to = "../escape"` must be rejected at config parse time.
    #[test]
    fn sync_to_rejects_escape() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"
purgery_root = "/tmp/purgery"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "../escape"
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(result.is_err(), "sync to='../escape' must be rejected");
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

    // ── RunState tests ──

    #[test]
    fn run_state_serde_roundtrip() {
        for state in &[RunState::Done, RunState::Partial, RunState::Failed] {
            let json = serde_json::to_string(state).unwrap();
            let back: RunState = serde_json::from_str(&json).unwrap();
            assert_eq!(*state, back);
        }
    }

    #[test]
    fn run_state_from_str() {
        assert_eq!("done".parse::<RunState>().unwrap(), RunState::Done);
        assert_eq!("partial".parse::<RunState>().unwrap(), RunState::Partial);
        assert_eq!("failed".parse::<RunState>().unwrap(), RunState::Failed);
        assert!("unknown".parse::<RunState>().is_err());
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
program = "my-compress-video"
args = ["--input", "{path}"]
"#;
        let config = ServerConfig::from_toml(toml).unwrap();
        assert_eq!(config.root.as_str(), "/universe/synced");
        assert_eq!(config.state_dir.unwrap().as_str(), "/var/lib/purgery");
        assert_eq!(config.postprocess.max_parallel_jobs, 2);
        let step = config.postprocess.steps.get("compress-video").unwrap();
        assert_eq!(step.kind, "builtin");
        assert_eq!(step.program, "my-compress-video");
        assert_eq!(step.args, vec!["--input", "{path}"]);
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
        assert_eq!(config.nickname.as_str(), "laptop");
        assert_eq!(config.server.host.as_str(), "example.com");
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
        assert_eq!(config.sync[0].name.as_str(), "videos");
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

    #[test]
    fn server_config_rejects_relative_root() {
        let toml = r#"
root = "relative/path"
purgery_root = "/universe/tmp/purgery"
"#;
        let result = ServerConfig::from_toml(toml);
        assert!(result.is_err());
    }

    #[test]
    fn client_config_rejects_invalid_nickname() {
        let toml = r#"
nickname = ""

[server]
host = "example.com"
purgery_root = "/universe/tmp/purgery"
"#;
        let result = ClientConfig::from_toml(toml);
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
        let manifest = Manifest::from_toml(toml).unwrap();
        assert_eq!(manifest.run_id.as_str(), "2026-06-08T18-45-12Z-9f03");
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
        let manifest = Manifest::from_toml(toml).unwrap();
        assert!(manifest.files[0].sha256.is_none());
    }

    #[test]
    fn manifest_empty_files_is_error() {
        let toml = r#"
run_id = "2026-06-08T18-45-12Z-9f03"
nickname = "laptop"
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err());
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
        let status = RunStatus::from_toml(toml).unwrap();
        assert_eq!(status.state, RunState::Done);
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
    fn parse_status_with_run_error() {
        let toml = r#"
run_id = "2026-06-08T18-45-12Z-9f03"
nickname = "laptop"
state = "failed"
error = "failed to parse manifest.toml"
files = []
"#;
        let status = RunStatus::from_toml(toml).unwrap();
        assert_eq!(status.state, RunState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("failed to parse manifest.toml")
        );
        assert!(status.files.is_empty());
    }

    // ── Identity tests ──

    #[test]
    fn identity_from_entry() {
        let entry = ManifestFileEntry {
            sync_name: "videos".into(),
            local_path: "/home/vitalik/Videos/a.mp4".into(),
            staged_path: NormalizedRelativePath::new("files/videos/a.mp4".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
            size: 100,
            mtime_ns: 200,
            sha256: Some("abc".into()),
        };
        let identity = entry.identity();
        assert_eq!(identity.size, 100);
        assert_eq!(identity.mtime_ns, 200);
        assert_eq!(identity.sha256, Some("abc".into()));
    }

    #[test]
    fn run_phase_as_str() {
        assert_eq!(RunPhase::Incoming.as_str(), "incoming");
        assert_eq!(RunPhase::Ready.as_str(), "ready");
        assert_eq!(RunPhase::Processing.as_str(), "processing");
        assert_eq!(RunPhase::Done.as_str(), "done");
        assert_eq!(RunPhase::Failed.as_str(), "failed");
    }

    // ── Path safety tests ──

    #[test]
    fn path_is_within_root_positive() {
        let root = Utf8Path::new("/data");
        let resolved = Utf8Path::new("/data/laptop/videos/a.mp4");
        assert!(path_is_within_root(resolved, root));
    }

    #[test]
    fn path_is_within_root_negative() {
        let root = Utf8Path::new("/data");
        let resolved = Utf8Path::new("/etc/passwd");
        assert!(!path_is_within_root(resolved, root));
    }

    // ── Manifest/Status roundtrip tests ──

    #[test]
    fn manifest_toml_roundtrip() {
        let manifest = Manifest {
            run_id: RunId::new("test-123".into()).unwrap(),
            nickname: Nickname::new("testbox".into()).unwrap(),
            files: vec![ManifestFileEntry {
                sync_name: "videos".into(),
                local_path: "/tmp/test.mp4".into(),
                staged_path: NormalizedRelativePath::new("files/videos/test.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("test.mp4".into()).unwrap(),
                size: 100,
                mtime_ns: 200,
                sha256: Some("abcdef".into()),
            }],
        };
        let toml = manifest.to_toml().unwrap();
        let parsed = Manifest::from_toml(&toml).unwrap();
        assert_eq!(parsed.run_id, manifest.run_id);
        assert_eq!(parsed.nickname, manifest.nickname);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].sha256, Some("abcdef".into()));
    }

    #[test]
    fn status_toml_roundtrip() {
        let status = RunStatus {
            run_id: RunId::new("test-123".into()).unwrap(),
            nickname: Nickname::new("testbox".into()).unwrap(),
            state: RunState::Done,
            files: vec![FileStatusEntry {
                sync_name: "videos".into(),
                local_path: "/tmp/test.mp4".into(),
                relative_path: "test.mp4".into(),
                status: FileStatus::Imported,
                final_path: Some("laptop/videos/test.mp4".into()),
                postprocess: Some(vec!["compress-video".into()]),
                error: None,
            }],
            error: None,
        };
        let toml = status.to_toml().unwrap();
        let parsed = RunStatus::from_toml(&toml).unwrap();
        assert_eq!(parsed.state, RunState::Done);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].status, FileStatus::Imported);
    }

    // ── Envelope validation tests ──

    #[test]
    fn validate_envelope_ok() {
        let nick = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let config = ClientConfig {
            nickname: nick.clone(),
            server: ServerConnection {
                host: RemoteHost::new("example.com".into()).unwrap(),
                purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            },
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nick.clone(),
            files: vec![ManifestFileEntry {
                sync_name: "videos".into(),
                local_path: "/tmp/a.mp4".into(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
            }],
        };
        assert!(validate_envelope(&nick, &run_id, &config, &manifest).is_ok());
    }

    #[test]
    fn validate_envelope_rejects_nickname_mismatch() {
        let dir_nick = Nickname::new("laptop".into()).unwrap();
        let other_nick = Nickname::new("desktop".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let config = ClientConfig {
            nickname: other_nick.clone(),
            server: ServerConnection {
                host: RemoteHost::new("example.com".into()).unwrap(),
                purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            },
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: other_nick,
            files: vec![ManifestFileEntry {
                sync_name: "videos".into(),
                local_path: "/tmp/a.mp4".into(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
            }],
        };
        assert!(validate_envelope(&dir_nick, &run_id, &config, &manifest).is_err());
    }

    #[test]
    fn validate_envelope_rejects_manifest_nickname_mismatch() {
        let nick = Nickname::new("laptop".into()).unwrap();
        let other = Nickname::new("server".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let config = ClientConfig {
            nickname: nick.clone(),
            server: ServerConnection {
                host: RemoteHost::new("example.com".into()).unwrap(),
                purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            },
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: other,
            files: vec![ManifestFileEntry {
                sync_name: "videos".into(),
                local_path: "/tmp/a.mp4".into(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
            }],
        };
        assert!(validate_envelope(&nick, &run_id, &config, &manifest).is_err());
    }

    #[test]
    fn validate_envelope_rejects_run_id_mismatch() {
        let nick = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let wrong_run_id = RunId::new("run-2".into()).unwrap();
        let config = ClientConfig {
            nickname: nick.clone(),
            server: ServerConnection {
                host: RemoteHost::new("example.com".into()).unwrap(),
                purgery_root: PurgeryRoot::new("/tmp/purgery".into()).unwrap(),
            },
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: wrong_run_id,
            nickname: nick.clone(),
            files: vec![ManifestFileEntry {
                sync_name: "videos".into(),
                local_path: "/tmp/a.mp4".into(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
            }],
        };
        assert!(validate_envelope(&nick, &run_id, &config, &manifest).is_err());
    }

    // ── verify_staged tests ──

    #[test]
    fn verify_staged_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Utf8PathBuf::from_path_buf(dir.path().join("f.bin")).unwrap();
        std::fs::write(staged.as_std_path(), b"hello").unwrap();

        let entry = ManifestFileEntry {
            sync_name: "videos".into(),
            local_path: "/x".into(),
            staged_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            size: 999, // wrong size
            mtime_ns: 0,
            sha256: None,
        };

        let result = entry.verify_staged(&staged);
        assert!(matches!(
            result,
            Err(IdentityVerificationError::SizeMismatch { .. })
        ));
    }

    #[test]
    fn verify_staged_sha_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Utf8PathBuf::from_path_buf(dir.path().join("f.bin")).unwrap();
        std::fs::write(staged.as_std_path(), b"hello").unwrap();

        let entry = ManifestFileEntry {
            sync_name: "videos".into(),
            local_path: "/x".into(),
            staged_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            size: 5,
            mtime_ns: 0,
            sha256: Some("badbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbad1".into()),
        };

        let result = entry.verify_staged(&staged);
        assert!(matches!(
            result,
            Err(IdentityVerificationError::Sha256Mismatch)
        ));
    }

    #[test]
    fn verify_staged_size_ok_no_sha() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Utf8PathBuf::from_path_buf(dir.path().join("f.bin")).unwrap();
        std::fs::write(staged.as_std_path(), b"hello").unwrap();

        let entry = ManifestFileEntry {
            sync_name: "videos".into(),
            local_path: "/x".into(),
            staged_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            size: 5,
            mtime_ns: 0,
            sha256: None,
        };

        assert!(entry.verify_staged(&staged).is_ok());
    }

    #[test]
    fn verify_staged_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Utf8PathBuf::from_path_buf(dir.path().join("nonexistent")).unwrap();

        let entry = ManifestFileEntry {
            sync_name: "videos".into(),
            local_path: "/x".into(),
            staged_path: NormalizedRelativePath::new("nonexistent".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("nonexistent".into()).unwrap(),
            size: 5,
            mtime_ns: 0,
            sha256: None,
        };

        let result = entry.verify_staged(&staged);
        assert!(matches!(
            result,
            Err(IdentityVerificationError::NotFound(_))
        ));
    }

    // ── PostprocessStepDefinition tests ──

    #[test]
    fn parse_postprocess_step_new_format() {
        let toml = r#"
kind = "builtin"
program = "ffmpeg"
args = ["-i", "{path}", "out.mp4"]
"#;
        let step: PostprocessStepDefinition = toml::from_str(toml).unwrap();
        assert_eq!(step.program, "ffmpeg");
        assert_eq!(step.args, vec!["-i", "{path}", "out.mp4"]);
    }
}
