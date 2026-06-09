use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    #[error("failed to serialize config: {0}")]
    TomlSerialize(String),
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
    #[error("invalid sync name: {0}")]
    SyncName(#[from] SyncNameError),
    #[error("invalid local path: {0}")]
    LocalPath(#[from] LocalSourcePathError),
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
    #[error("invalid sync name: {0}")]
    SyncName(#[from] SyncNameError),
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

#[derive(Error, Debug)]
pub enum ExecutableError {
    #[error("empty program name")]
    EmptyProgram,
    #[error("program '{0}' not found")]
    NotFound(String),
    #[error("program '{0}' is not executable")]
    NotExecutable(String),
    #[error("program '{0}' absolute path not found")]
    AbsoluteNotFound(String),
    #[error("I/O error checking program '{0}': {1}")]
    Io(String, #[source] io::Error),
}

/// Result of resolving an executable.
pub struct ResolvedExecutable {
    pub path: Utf8PathBuf,
}

/// Resolve an executable program path.
///
/// Rules:
/// - Absolute path: follow symlinks, require target exists and is a regular file, require executable bit set.
/// - Relative name: search PATH, follow symlinks, require target is regular file, require executable bit set.
/// - Directories and broken symlinks are rejected.
///
/// Uses a single `metadata()` call per candidate to check both the file type
/// (following symlinks) and the executable permission bits on Unix.
pub fn resolve_executable(program: &str) -> Result<ResolvedExecutable, ExecutableError> {
    if program.is_empty() {
        return Err(ExecutableError::EmptyProgram);
    }

    fn check(path: &Utf8Path, program: &str) -> Result<ResolvedExecutable, ExecutableError> {
        let meta = std::fs::metadata(path.as_std_path())
            .map_err(|e| ExecutableError::Io(program.to_owned(), e))?;
        if !meta.file_type().is_file() {
            return Err(ExecutableError::NotExecutable(format!(
                "'{}' is not a regular file",
                path.as_str()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                return Err(ExecutableError::NotExecutable(format!(
                    "'{}' is not executable",
                    program
                )));
            }
        }
        Ok(ResolvedExecutable {
            path: path.to_owned(),
        })
    }

    let program_path = Utf8Path::new(program);

    if program_path.is_absolute() {
        if !program_path.exists() {
            return Err(ExecutableError::AbsoluteNotFound(program.to_owned()));
        }
        check(program_path, program)
    } else {
        let path_var = std::env::var("PATH").unwrap_or_default();
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Utf8Path::new(dir).join(program);
            if !candidate.exists() {
                continue;
            }
            if check(&candidate, program).is_ok() {
                return Ok(ResolvedExecutable { path: candidate });
            }
        }
        Err(ExecutableError::NotFound(program.to_owned()))
    }
}

// ── Lease / GC Config ────────────────────────────────────────────────

/// Default incoming lease time in seconds.
const DEFAULT_INCOMING_LEASE_SECS: u64 = 1800;

/// Default heartbeat interval in seconds.
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 60;

/// GC configuration.
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

/// Lease file written to incoming run directories.
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

/// A validated client-side local path.
///
/// # Invariants
/// * Non-empty.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientLocalPath(String);

impl ClientLocalPath {
    pub fn new(s: String) -> Result<Self, LocalSourcePathError> {
        if s.is_empty() {
            return Err(LocalSourcePathError::Empty);
        }
        Ok(ClientLocalPath(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for ClientLocalPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ClientLocalPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        ClientLocalPath::new(s).map_err(serde::de::Error::custom)
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
        let ulid = ulid::Ulid::new();
        RunId(ulid.to_string())
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

// ── Path Normalization ──────────────────────────────────────────────

/// Normalize a relative path: collapse `//`, remove `.`, reject `..` and absolute.
pub fn normalize_relative(path: &Utf8Path) -> Result<String, PathValidationError> {
    if path.is_absolute() {
        return Err(PathValidationError::NotRelative);
    }
    let mut result: Vec<&str> = Vec::new();
    for component in path.components() {
        let comp = component.as_str();
        match comp {
            ".." => return Err(PathValidationError::ContainsDotDot),
            "." => continue,
            "" => continue,
            _ => result.push(comp),
        }
    }
    if result.is_empty() {
        return Err(PathValidationError::EmptyComponent);
    }
    Ok(result.join("/"))
}

/// A relative destination path within a sync mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelativeDestinationPath(Utf8PathBuf);

impl RelativeDestinationPath {
    pub fn new(path: Utf8PathBuf) -> Result<Self, PathValidationError> {
        let normalized = normalize_relative(path.as_path())?;
        Ok(RelativeDestinationPath(Utf8PathBuf::from(normalized)))
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
        let normalized = normalize_relative(path.as_path())?;
        Ok(NormalizedRelativePath(Utf8PathBuf::from(normalized)))
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
    pub sync_name: SyncName,
    pub local_path: ClientLocalPath,
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
            local_path: Utf8PathBuf::from(self.local_path.as_str()),
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
    pub sync_name: SyncName,
    pub local_path: String,
    pub relative_path: String,
    pub status: FileStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub final_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postprocess: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Config Types ─────────────────────────────────────────────────────

/// Logging configuration.
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

/// Initialize the global tracing subscriber from a `LoggingConfig`.
///
/// Must be called at most once, near the binary entry point.
/// Logs go to stderr; stdout is reserved for machine-readable protocol output.
/// Uses the configured level directly — `RUST_LOG` environment variable is not
/// consulted. The caller is responsible for resolving precedence
/// (CLI > config > default) before calling this function.
pub fn init_logging(
    config: &LoggingConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let level_filter: tracing_subscriber::filter::LevelFilter = match config.level {
        LogLevel::Error => tracing_subscriber::filter::LevelFilter::ERROR,
        LogLevel::Warn => tracing_subscriber::filter::LevelFilter::WARN,
        LogLevel::Info => tracing_subscriber::filter::LevelFilter::INFO,
        LogLevel::Debug => tracing_subscriber::filter::LevelFilter::DEBUG,
        LogLevel::Trace => tracing_subscriber::filter::LevelFilter::TRACE,
    };

    let is_terminal = atty::is(atty::Stream::Stderr);
    let use_color = match config.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => is_terminal,
    };

    match config.format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_max_level(level_filter)
                .json()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .try_init()?;
        }
        LogFormat::Compact => {
            tracing_subscriber::fmt()
                .with_max_level(level_filter)
                .compact()
                .with_writer(std::io::stderr)
                .with_ansi(use_color)
                .try_init()?;
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .with_max_level(level_filter)
                .pretty()
                .with_writer(std::io::stderr)
                .with_ansi(use_color)
                .try_init()?;
        }
    }
    Ok(())
}

/// Server configuration, loaded from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub root: ServerRoot,
    pub purgery_root: PurgeryRoot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<Utf8PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<Utf8PathBuf>,
    #[serde(default)]
    pub postprocess: PostprocessConfig,
    #[serde(default)]
    pub gc: GCConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Postprocessing configuration (server-side).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// The kind of postprocessing step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostprocessKind {
    Subprocess,
}

impl<'de> Deserialize<'de> for PostprocessKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "subprocess" => Ok(PostprocessKind::Subprocess),
            other => Err(serde::de::Error::custom(format!(
                "unknown postprocess kind: {other}"
            ))),
        }
    }
}

impl Serialize for PostprocessKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = match self {
            PostprocessKind::Subprocess => "subprocess",
        };
        s.serialize(serializer)
    }
}

fn default_true() -> bool {
    true
}

/// Definition of a postprocessing step on the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostprocessStepDefinition {
    pub kind: PostprocessKind,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
    #[serde(default = "default_true")]
    pub keep_original: bool,
}

impl PostprocessStepDefinition {
    /// Resolve `{input}`, `{parent}`, `{file_name}`, `{file_stem}`, `{stem}` placeholders
    /// in a string using the given work path.
    /// `{stem}` is kept as a deprecated alias for `{file_stem}`.
    pub fn resolve_placeholders(&self, work_path: &Utf8Path, s: &str) -> String {
        let input = work_path.as_str();
        let parent = work_path.parent().map(|p| p.as_str()).unwrap_or("");
        let file_name = work_path.file_name().unwrap_or("");
        let file_stem = work_path.file_stem().unwrap_or("");
        s.replace("{input}", input)
            .replace("{parent}", parent)
            .replace("{file_name}", file_name)
            .replace("{file_stem}", file_stem)
            .replace("{stem}", file_stem)
    }

    /// Build the command arguments for this step given the work path.
    pub fn build_args(&self, work_path: &Utf8Path) -> Vec<String> {
        self.args
            .iter()
            .map(|a| self.resolve_placeholders(work_path, a))
            .collect()
    }

    /// Resolve expected output paths relative to the work parent directory.
    ///
    /// Only the file name portion of the resolved pattern is used; the output
    /// is always placed in the same directory as the input file.
    /// Each pattern is validated as a plain file name before resolution.
    pub fn resolve_expected_outputs(
        &self,
        work_path: &Utf8Path,
    ) -> Result<Vec<Utf8PathBuf>, String> {
        let parent = work_path
            .parent()
            .map(|p| p.to_owned())
            .unwrap_or_else(|| Utf8PathBuf::from("."));
        let mut results = Vec::with_capacity(self.expected_outputs.len());
        for pat in &self.expected_outputs {
            validate_expected_output_name(pat)?;
            let resolved = self.resolve_placeholders(work_path, pat);
            let p = Utf8Path::new(&resolved);
            let fname = p.file_name().unwrap_or(resolved.as_str());
            results.push(parent.join(fname));
        }
        Ok(results)
    }
}

/// Validate that an expected output name is a plain file name.
///
/// Rejects empty names, `.`, `..`, absolute paths, names containing
/// path separators (`/` or `\`), and placeholders that reference the
/// input path (`{input}`, `{parent}`). Only file-stem placeholders
/// (`{file_name}`, `{file_stem}`, `{stem}`) are allowed because
/// expected outputs are always placed next to the input file in the
/// work directory.
pub fn validate_expected_output_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("expected output name is empty".into());
    }
    if name == "." || name == ".." {
        return Err(format!("expected output name must not be '{name}'"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err("expected output name must not contain path separators".into());
    }
    if Utf8Path::new(name).is_absolute() {
        return Err("expected output name must not be absolute".into());
    }
    if name.contains("{input}") || name.contains("{parent}") {
        return Err(
            "expected output name must not use {{input}} or {{parent}} placeholders; \
             only {{file_name}}, {{file_stem}}, and {{stem}} are allowed"
                .into(),
        );
    }
    Ok(())
}

/// Client configuration, loaded from a TOML file.
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
}

impl ClientConfig {
    pub fn find_sync(&self, name: &str) -> Option<&SyncMapping> {
        self.sync.iter().find(|s| s.name.as_str() == name)
    }
}

/// Server connection details for the client.
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

/// A single sync mapping from local path to server destination.
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

/// Client-side postprocessing rule configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPostprocessConfig {
    #[serde(default)]
    pub rules: Vec<PostprocessRule>,
}

/// A postprocessing rule: files matching `match` regex get the listed steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostprocessRule {
    #[serde(rename = "match")]
    pub pattern: String,
    pub steps: Vec<String>,
}

/// Server-relevant per-run configuration uploaded alongside files.
///
/// Unlike `ClientConfig`, this does not include server transport
/// details or local source paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub nickname: Nickname,
    #[serde(default)]
    pub sync: Vec<RunConfigSync>,
    #[serde(default)]
    pub postprocess: ClientPostprocessConfig,
}

/// A sync mapping within a `RunConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfigSync {
    pub name: SyncName,
    #[serde(rename = "to")]
    pub to_path: RelativeDestinationPath,
}

/// Response from `purgery-server begin-run`.
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

// ── Symlink Escape Hardening ────────────────────────────────────────

/// Check that no component of `final_path` (relative to `server_root`) is a symlink.
///
/// For each component of `final_path` relative to `server_root`, check if an existing
/// path at that component is a symlink. If any existing component is a symlink,
/// return an error.
pub fn check_symlink_in_path(final_path: &Utf8Path, server_root: &Utf8Path) -> Result<(), String> {
    let relative = final_path.strip_prefix(server_root).map_err(|_| {
        format!(
            "final path '{}' is not under server root '{}'",
            final_path.as_str(),
            server_root.as_str()
        )
    })?;

    let mut current = server_root.to_owned();
    for component in relative.components() {
        current = current.join(component.as_str());
        if current.exists() {
            let metadata = std::fs::symlink_metadata(current.as_std_path())
                .map_err(|e| format!("failed to read metadata for '{}': {e}", current.as_str()))?;
            if metadata.file_type().is_symlink() {
                return Err(format!("symlink detected in path: {}", current.as_str()));
            }
        }
    }
    Ok(())
}

// ── Build Rsync Args ────────────────────────────────────────────────

/// Build the argument list for rsync.
///
/// Returns: `["--recursive", "--partial", "--archive", "--no-inc-recursive", "--protect-args", source, destination]`
/// Note: source will have trailing `/` added automatically.
pub fn build_rsync_args(source: &str, destination: &str) -> Vec<String> {
    vec![
        "--recursive".to_string(),
        "--partial".to_string(),
        "--archive".to_string(),
        "--no-inc-recursive".to_string(),
        "--protect-args".to_string(),
        format!("{}/", source),
        destination.to_string(),
    ]
}

// ── Work Area ────────────────────────────────────────────────────────

/// Compute the hidden work area path for postprocessing.
///
/// Returns: `<server_root>/.purgery-work/<nickname>/<run_id>/`
pub fn work_dir(server_root: &Utf8Path, nickname: &Nickname, run_id: &RunId) -> Utf8PathBuf {
    server_root
        .join(".purgery-work")
        .join(nickname.as_str())
        .join(run_id.as_str())
}

/// Build a temporary commit path in the same directory as `final_path`.
///
/// Returns `parent / .purgery-commit.<run_id>.<filename>.tmp`.
/// This path is on the same filesystem as `final_path`, so the final
/// rename is atomic.
pub fn commit_temp_path(final_path: &Utf8Path, run_id: &RunId) -> Utf8PathBuf {
    let filename = final_path.file_name().unwrap_or("unknown");
    let tmp_name = format!(".purgery-commit.{}.{}.tmp", run_id.as_str(), filename);
    final_path
        .parent()
        .map_or_else(|| Utf8PathBuf::from(&tmp_name), |p| p.join(&tmp_name))
}

// ── Envelope Validation ─────────────────────────────────────────────

/// Validate that the directory envelope matches the run's metadata.
///
/// Checks:
/// * `run_config.nickname` == `dir_nickname`
/// * `manifest.nickname` == `dir_nickname`
/// * `manifest.run_id` == `dir_run_id`
pub fn validate_envelope(
    dir_nickname: &Nickname,
    dir_run_id: &RunId,
    run_config: &RunConfig,
    manifest: &Manifest,
) -> Result<(), String> {
    if run_config.nickname != *dir_nickname {
        return Err(format!(
            "directory nickname '{}' does not match run config nickname '{}'",
            dir_nickname.as_str(),
            run_config.nickname.as_str()
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

/// Parse a `RunConfig` from TOML.
impl RunConfig {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: RunConfig = toml::from_str(input)?;
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string(self).map_err(|e| ConfigError::TomlSerialize(e.to_string()))
    }

    /// Build a lookup map from sync name to sync.
    pub fn sync_map(&self) -> BTreeMap<&str, &RunConfigSync> {
        self.sync.iter().map(|s| (s.name.as_str(), s)).collect()
    }
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

    #[test]
    fn sync_name_serde_roundtrip() {
        let n = SyncName::new("videos".into()).unwrap();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"videos\"");
        let back: SyncName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
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

    // ── ClientLocalPath tests ──

    #[test]
    fn client_local_path_valid() {
        let p = ClientLocalPath::new("/home/user/file.mp4".into()).unwrap();
        assert_eq!(p.as_str(), "/home/user/file.mp4");
    }

    #[test]
    fn client_local_path_empty_is_error() {
        assert!(ClientLocalPath::new("".into()).is_err());
    }

    #[test]
    fn client_local_path_serde_roundtrip() {
        let p = ClientLocalPath::new("/tmp/test.mp4".into()).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"/tmp/test.mp4\"");
        let back: ClientLocalPath = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    // ── RunId tests ──

    #[test]
    fn run_id_valid() {
        let r = RunId::new("01ARZ3NDEKTSV4RRFFQ69G5FAV".into()).unwrap();
        assert_eq!(r.as_str(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
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
        // ULIDs are 26 characters of Crockford base32
        assert_eq!(id.as_str().len(), 26);
        // Generated ULID should pass validation
        assert!(RunId::new(id.as_str().to_owned()).is_ok());
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
        let p = Utf8PathBuf::from("tmp/purgy");
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
    fn relative_dest_collapses_consecutive_separators() {
        let p = Utf8PathBuf::from("a//b");
        let r = RelativeDestinationPath::new(p).unwrap();
        assert_eq!(r.as_str(), "a/b");
    }

    #[test]
    fn relative_dest_removes_dot_components() {
        let p = Utf8PathBuf::from("a/./b");
        let r = RelativeDestinationPath::new(p).unwrap();
        assert_eq!(r.as_str(), "a/b");
    }

    /// `to = "../escape"` must be rejected at config parse time.
    #[test]
    fn sync_to_rejects_escape() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"

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

    #[test]
    fn normalized_path_collapses_separators() {
        let p = Utf8PathBuf::from("a//b");
        let n = NormalizedRelativePath::new(p).unwrap();
        assert_eq!(n.as_str(), "a/b");
    }

    #[test]
    fn normalized_path_removes_dot() {
        let p = Utf8PathBuf::from("a/./b/c");
        let n = NormalizedRelativePath::new(p).unwrap();
        assert_eq!(n.as_str(), "a/b/c");
    }

    #[test]
    fn normalized_path_rejects_absolute() {
        let p = Utf8PathBuf::from("/etc/passwd");
        assert_eq!(
            NormalizedRelativePath::new(p),
            Err(PathValidationError::NotRelative)
        );
    }

    #[test]
    fn normalized_path_rejects_empty_after_normalization() {
        let p = Utf8PathBuf::from("./.");
        assert_eq!(
            NormalizedRelativePath::new(p),
            Err(PathValidationError::EmptyComponent)
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
kind = "subprocess"
program = "my-compress-video"
args = ["--input", "{input}"]
expected_outputs = ["{stem}.Z.webm"]
keep_original = true
"#;
        let config = ServerConfig::from_toml(toml).unwrap();
        assert_eq!(config.root.as_str(), "/universe/synced");
        assert_eq!(config.state_dir.unwrap().as_str(), "/var/lib/purgery");
        assert_eq!(config.postprocess.max_parallel_jobs, 2);
        let step = config.postprocess.steps.get("compress-video").unwrap();
        assert_eq!(step.kind, PostprocessKind::Subprocess);
        assert_eq!(step.program, "my-compress-video");
        assert!(step.keep_original);
    }

    #[test]
    fn parse_server_config_subprocess_kind() {
        let toml = r#"
root = "/universe/synced"
purgery_root = "/universe/tmp/purgery"

[postprocess.steps.compress-video]
kind = "subprocess"
program = "my-compress-video"
args = ["--input", "{input}"]
expected_outputs = ["{stem}.Z.webm"]
"#;
        let config = ServerConfig::from_toml(toml).unwrap();
        let step = config.postprocess.steps.get("compress-video").unwrap();
        assert_eq!(step.kind, PostprocessKind::Subprocess);
        assert_eq!(step.program, "my-compress-video");
        assert!(step.keep_original);
    }

    #[test]
    fn postprocess_step_rejects_old_command_format() {
        let toml = r#"
kind = "subprocess"
command = "my-compress-video"
"#;
        let result: Result<PostprocessStepDefinition, _> = toml::from_str(toml);
        assert!(result.is_err(), "old 'command' field must be rejected");
    }

    #[test]
    fn postprocess_step_rejects_old_compress_video_kind() {
        let toml = r#"
kind = "compress-video"
program = "my-compress-video"
"#;
        let result: Result<PostprocessStepDefinition, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "old 'compress-video' kind must be rejected"
        );
    }

    #[test]
    fn postprocess_step_rejects_unknown_kind() {
        let toml = r#"
kind = "builtin"
"#;
        let result: Result<PostprocessStepDefinition, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown kind must be rejected");
    }

    #[test]
    fn parse_client_config_minimal() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"
"#;
        let config = ClientConfig::from_toml(toml).unwrap();
        assert_eq!(config.nickname.as_str(), "laptop");
        assert_eq!(config.server.host.as_str(), "example.com");
        assert_eq!(config.server.command, "purgery-server");
        assert!(config.sync.is_empty());
    }

    #[test]
    fn parse_client_config_full() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"
command = "purgery-server --config /etc/purgery/server.toml"

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
        assert_eq!(
            config.server.command,
            "purgery-server --config /etc/purgery/server.toml"
        );
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
"#;
        let result = ClientConfig::from_toml(toml);
        assert!(result.is_err());
    }

    // ── Manifest tests ──

    #[test]
    fn parse_manifest() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
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
        assert_eq!(manifest.run_id.as_str(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].sync_name.as_str(), "videos");
        assert_eq!(
            manifest.files[0].local_path.as_str(),
            "/home/vitalik/Videos/a.mp4"
        );
        assert_eq!(manifest.files[0].sha256.as_deref(), Some("abcd1234"));
    }

    #[test]
    fn parse_manifest_rejects_invalid_sync_name() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[files]]
sync_name = ""
local_path = "/home/user/file.mp4"
staged_path = "files/file.mp4"
relative_path = "file.mp4"
size = 100
mtime_ns = 0
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err(), "empty sync_name must be rejected");
    }

    #[test]
    fn parse_manifest_rejects_invalid_local_path() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"

[[files]]
sync_name = "videos"
local_path = ""
staged_path = "files/file.mp4"
relative_path = "file.mp4"
size = 100
mtime_ns = 0
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err(), "empty local_path must be rejected");
    }

    #[test]
    fn parse_manifest_without_sha256() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
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
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
"#;
        let result = Manifest::from_toml(toml);
        assert!(result.is_err());
    }

    // ── Status tests ──

    #[test]
    fn parse_status_with_sync_name() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[files]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
relative_path = "a.mp4"
status = "imported"
"#;
        let status = RunStatus::from_toml(toml).unwrap();
        assert_eq!(status.files[0].sync_name.as_str(), "videos");
        assert_eq!(status.files[0].local_path, "/home/vitalik/Videos/a.mp4");
    }

    #[test]
    fn parse_status() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[files]]
sync_name = "videos"
local_path = "/home/vitalik/Videos/a.mp4"
relative_path = "a.mp4"
status = "imported"
final_paths = ["laptop/videos/a.mp4"]
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
        assert_eq!(status.files[0].final_paths, vec!["laptop/videos/a.mp4"]);
        assert_eq!(status.files[0].sync_name.as_str(), "videos");
        assert_eq!(status.files[1].status, FileStatus::Failed);
        assert_eq!(
            status.files[1].error.as_deref(),
            Some("compress-video failed")
        );
    }

    #[test]
    fn parse_status_with_run_error() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
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
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/home/vitalik/Videos/a.mp4".into()).unwrap(),
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
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/test.mp4".into()).unwrap(),
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
        assert_eq!(parsed.files[0].sync_name.as_str(), "videos");
    }

    #[test]
    fn status_toml_roundtrip() {
        let status = RunStatus {
            run_id: RunId::new("test-123".into()).unwrap(),
            nickname: Nickname::new("testbox".into()).unwrap(),
            state: RunState::Done,
            files: vec![FileStatusEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: "/tmp/test.mp4".into(),
                relative_path: "test.mp4".into(),
                status: FileStatus::Imported,
                final_paths: vec!["laptop/videos/test.mp4".into()],
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
        assert_eq!(parsed.files[0].sync_name.as_str(), "videos");
        assert_eq!(parsed.files[0].final_paths, vec!["laptop/videos/test.mp4"]);
    }

    // ── Envelope validation tests ──

    #[test]
    fn validate_envelope_ok() {
        let nick = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let run_config = RunConfig {
            nickname: nick.clone(),
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: nick.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
            }],
        };
        assert!(validate_envelope(&nick, &run_id, &run_config, &manifest).is_ok());
    }

    #[test]
    fn validate_envelope_rejects_nickname_mismatch() {
        let dir_nick = Nickname::new("laptop".into()).unwrap();
        let other_nick = Nickname::new("desktop".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let run_config = RunConfig {
            nickname: other_nick.clone(),
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: other_nick,
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
            }],
        };
        assert!(validate_envelope(&dir_nick, &run_id, &run_config, &manifest).is_err());
    }

    #[test]
    fn validate_envelope_rejects_manifest_nickname_mismatch() {
        let nick = Nickname::new("laptop".into()).unwrap();
        let other = Nickname::new("server".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let run_config = RunConfig {
            nickname: nick.clone(),
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: run_id.clone(),
            nickname: other,
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
            }],
        };
        assert!(validate_envelope(&nick, &run_id, &run_config, &manifest).is_err());
    }

    #[test]
    fn validate_envelope_rejects_run_id_mismatch() {
        let nick = Nickname::new("laptop".into()).unwrap();
        let run_id = RunId::new("run-1".into()).unwrap();
        let wrong_run_id = RunId::new("run-2".into()).unwrap();
        let run_config = RunConfig {
            nickname: nick.clone(),
            sync: vec![],
            postprocess: ClientPostprocessConfig::default(),
        };
        let manifest = Manifest {
            run_id: wrong_run_id,
            nickname: nick.clone(),
            files: vec![ManifestFileEntry {
                sync_name: SyncName::new("videos".into()).unwrap(),
                local_path: ClientLocalPath::new("/tmp/a.mp4".into()).unwrap(),
                staged_path: NormalizedRelativePath::new("files/a.mp4".into()).unwrap(),
                relative_path: NormalizedRelativePath::new("a.mp4".into()).unwrap(),
                size: 10,
                mtime_ns: 100,
                sha256: None,
            }],
        };
        assert!(validate_envelope(&nick, &run_id, &run_config, &manifest).is_err());
    }

    // ── verify_staged tests ──

    #[test]
    fn verify_staged_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let staged = Utf8PathBuf::from_path_buf(dir.path().join("f.bin")).unwrap();
        std::fs::write(staged.as_std_path(), b"hello").unwrap();

        let entry = ManifestFileEntry {
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/x".into()).unwrap(),
            staged_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            relative_path: NormalizedRelativePath::new("f.bin".into()).unwrap(),
            size: 999,
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
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/x".into()).unwrap(),
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
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/x".into()).unwrap(),
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
            sync_name: SyncName::new("videos".into()).unwrap(),
            local_path: ClientLocalPath::new("/x".into()).unwrap(),
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

    // ── Normalize relative tests ──

    #[test]
    fn normalize_relative_basic() {
        assert_eq!(normalize_relative(Utf8Path::new("a/b/c")).unwrap(), "a/b/c");
    }

    #[test]
    fn normalize_relative_collapses_double_slash() {
        assert_eq!(normalize_relative(Utf8Path::new("a//b")).unwrap(), "a/b");
    }

    #[test]
    fn normalize_relative_removes_dot() {
        assert_eq!(normalize_relative(Utf8Path::new("a/./b")).unwrap(), "a/b");
    }

    #[test]
    fn normalize_relative_removes_all_dots() {
        assert_eq!(
            normalize_relative(Utf8Path::new("./a/./b/.")).unwrap(),
            "a/b"
        );
    }

    #[test]
    fn normalize_relative_rejects_dotdot() {
        assert!(normalize_relative(Utf8Path::new("a/../b")).is_err());
    }

    #[test]
    fn normalize_relative_rejects_absolute() {
        assert!(normalize_relative(Utf8Path::new("/a/b")).is_err());
    }

    #[test]
    fn normalize_relative_rejects_empty_result() {
        assert!(normalize_relative(Utf8Path::new("./.")).is_err());
    }

    // ── Build Rsync Args tests ──

    #[test]
    fn build_rsync_args_basic() {
        let args = build_rsync_args("/home/user/Videos", "example.com:/remote/path");
        assert_eq!(
            args,
            vec![
                "--recursive",
                "--partial",
                "--archive",
                "--no-inc-recursive",
                "--protect-args",
                "/home/user/Videos/",
                "example.com:/remote/path",
            ]
        );
    }

    #[test]
    fn build_rsync_args_includes_protect_args() {
        let args = build_rsync_args("/src", "host:/dst");
        assert!(args.contains(&"--protect-args".to_string()));
    }

    #[test]
    fn build_rsync_args_with_spaces_in_source() {
        let args = build_rsync_args("/home/user/My Videos", "host:/dst");
        assert_eq!(args[5], "/home/user/My Videos/");
    }

    // ── Symlink Check tests ──

    #[test]
    fn check_symlink_in_path_no_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let final_path = sub.join("file.txt");
        std::fs::write(&final_path, b"hello").unwrap();

        assert!(check_symlink_in_path(&final_path, &root).is_ok());
    }

    #[test]
    fn check_symlink_in_path_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = root.join("link_to_target");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let final_path = link.join("file.txt");
        std::fs::write(&final_path, b"hello").unwrap();

        let result = check_symlink_in_path(&final_path, &root);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("symlink detected"));
    }

    #[test]
    fn check_symlink_in_path_component_is_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let real_dir = root.join("realdir");
        std::fs::create_dir_all(&real_dir).unwrap();
        let link = root.join("linked");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        let final_path = link.join("sub/file.txt");
        std::fs::create_dir_all(real_dir.join("sub")).unwrap();
        std::fs::write(&final_path, b"data").unwrap();

        let result = check_symlink_in_path(&final_path, &root);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("symlink detected"));
    }

    // ── Commit temp path tests ──

    #[test]
    fn commit_temp_path_basic() {
        let final_path = Utf8Path::new("/data/laptop/videos/a.mp4");
        let run_id = RunId::new("01ARZ3NDEKTSV4RRFFQ69G5FAV".into()).unwrap();
        let tmp = commit_temp_path(final_path, &run_id);
        assert_eq!(
            tmp.as_str(),
            "/data/laptop/videos/.purgery-commit.01ARZ3NDEKTSV4RRFFQ69G5FAV.a.mp4.tmp"
        );
    }

    #[test]
    fn commit_temp_path_same_filesystem_as_parent() {
        let final_path = Utf8Path::new("/root/sub/file.txt");
        let run_id = RunId::new("run-1".into()).unwrap();
        let tmp = commit_temp_path(final_path, &run_id);
        assert_eq!(tmp.parent(), final_path.parent());
    }

    // ── Work Dir tests ──

    #[test]
    fn work_dir_returns_correct_path() {
        let root = Utf8Path::new("/data");
        let nick = Nickname::new("laptop".into()).unwrap();
        let run = RunId::new("run1".into()).unwrap();
        let wd = work_dir(root, &nick, &run);
        assert_eq!(wd.as_str(), "/data/.purgery-work/laptop/run1");
    }

    // ── PostprocessKind serde tests ──

    #[test]
    fn postprocess_kind_subprocess() {
        let kind: PostprocessKind = serde_json::from_str("\"subprocess\"").unwrap();
        assert_eq!(kind, PostprocessKind::Subprocess);
    }

    #[test]
    fn postprocess_kind_rejects_unknown() {
        let result: Result<PostprocessKind, _> = serde_json::from_str("\"builtin\"");
        assert!(result.is_err());
    }

    #[test]
    fn postprocess_kind_rejects_old_compress_video() {
        let result: Result<PostprocessKind, _> = serde_json::from_str("\"compress-video\"");
        assert!(result.is_err());
    }

    // ── Example config parsing tests ──

    #[test]
    fn parse_example_client_config() {
        let toml = include_str!("../../../examples/client.toml");
        let config = ClientConfig::from_toml(toml).unwrap();
        assert_eq!(config.nickname.as_str(), "laptop");
        assert_eq!(config.sync.len(), 2);
    }

    #[test]
    fn parse_example_server_config() {
        let toml = include_str!("../../../examples/server.toml");
        let config = ServerConfig::from_toml(toml).unwrap();
        assert_eq!(config.root.as_str(), "/universe/synced");
        let step = config.postprocess.steps.get("compress-video").unwrap();
        assert_eq!(step.kind, PostprocessKind::Subprocess);
        assert_eq!(step.program, "my-compress-video");
        assert!(step.keep_original);
    }

    // ── normalize_relative helper tests ──

    #[test]
    fn test_normalize_relative() {
        assert_eq!(normalize_relative(Utf8Path::new("a/b/c")).unwrap(), "a/b/c");
        assert_eq!(normalize_relative(Utf8Path::new("a//b")).unwrap(), "a/b");
        assert_eq!(normalize_relative(Utf8Path::new("a/./b")).unwrap(), "a/b");
        assert!(normalize_relative(Utf8Path::new("a/../b")).is_err());
        assert!(normalize_relative(Utf8Path::new("/abs")).is_err());
        assert!(normalize_relative(Utf8Path::new("./.")).is_err());
    }

    // ── RunId ULID validation test ──

    #[test]
    fn generated_ulid_validates() {
        for _ in 0..10 {
            let id = RunId::generate();
            assert!(RunId::new(id.as_str().to_owned()).is_ok());
            assert_eq!(id.as_str().len(), 26);
        }
    }

    // ── RunConfig tests ──

    #[test]
    fn parse_run_config() {
        let toml = r#"
nickname = "laptop"

[[sync]]
name = "videos"
to = "videos"

[[postprocess.rules]]
match = '^videos/.*\.(mp4|mov|mkv|webm)$'
steps = ["compress-video"]
"#;
        let config = RunConfig::from_toml(toml).unwrap();
        assert_eq!(config.nickname.as_str(), "laptop");
        assert_eq!(config.sync.len(), 1);
        assert_eq!(config.sync[0].name.as_str(), "videos");
        assert_eq!(config.postprocess.rules.len(), 1);
    }

    #[test]
    fn run_config_roundtrip() {
        let config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![RunConfigSync {
                name: SyncName::new("videos".into()).unwrap(),
                to_path: RelativeDestinationPath::new("videos".into()).unwrap(),
            }],
            postprocess: ClientPostprocessConfig::default(),
        };
        let toml = config.to_toml().unwrap();
        let parsed = RunConfig::from_toml(&toml).unwrap();
        assert_eq!(parsed.nickname.as_str(), "laptop");
        assert_eq!(parsed.sync.len(), 1);
    }

    #[test]
    fn run_config_rejects_server_section() {
        let toml = r#"
nickname = "laptop"

[server]
host = "example.com"
"#;
        let result = RunConfig::from_toml(toml);
        assert!(result.is_err(), "RunConfig must reject [server] section");
    }

    #[test]
    fn run_config_rejects_from_path() {
        let toml = r#"
nickname = "laptop"

[[sync]]
name = "videos"
from = "/home/user/Videos"
to = "videos"
"#;
        let result = RunConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "RunConfig must reject 'from' field in sync"
        );
    }

    // ── Placeholder resolution tests ──

    #[test]
    fn resolve_input_placeholder() {
        let step = PostprocessStepDefinition {
            kind: PostprocessKind::Subprocess,
            program: "ffmpeg".into(),
            args: vec!["--input".into(), "{input}".into()],
            expected_outputs: vec!["{stem}.Z.webm".into()],
            keep_original: true,
        };
        let work_path = Utf8Path::new("/work/videos/video.mp4");
        let args = step.build_args(work_path);
        assert_eq!(args, vec!["--input", "/work/videos/video.mp4"]);
    }

    #[test]
    fn resolve_stem_placeholder() {
        let step = PostprocessStepDefinition {
            kind: PostprocessKind::Subprocess,
            program: "ffmpeg".into(),
            args: vec![],
            expected_outputs: vec!["{stem}.Z.webm".into()],
            keep_original: false,
        };
        let work_path = Utf8Path::new("/work/videos/video.mp4");
        let outputs = step.resolve_expected_outputs(work_path).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].as_str(), "/work/videos/video.Z.webm");
    }

    #[test]
    fn resolve_parent_placeholder() {
        let step = PostprocessStepDefinition {
            kind: PostprocessKind::Subprocess,
            program: "ffmpeg".into(),
            args: vec!["--output-dir".into(), "{parent}".into()],
            expected_outputs: vec![],
            keep_original: false,
        };
        let work_path = Utf8Path::new("/work/videos/video.mp4");
        let args = step.build_args(work_path);
        assert_eq!(args, vec!["--output-dir", "/work/videos"]);
    }

    #[test]
    fn run_config_sync_map() {
        let config = RunConfig {
            nickname: Nickname::new("laptop".into()).unwrap(),
            sync: vec![
                RunConfigSync {
                    name: SyncName::new("videos".into()).unwrap(),
                    to_path: RelativeDestinationPath::new("videos".into()).unwrap(),
                },
                RunConfigSync {
                    name: SyncName::new("pictures".into()).unwrap(),
                    to_path: RelativeDestinationPath::new("pictures".into()).unwrap(),
                },
            ],
            postprocess: ClientPostprocessConfig::default(),
        };
        let map = config.sync_map();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("videos").unwrap().to_path.as_str(), "videos");
        assert_eq!(map.get("pictures").unwrap().to_path.as_str(), "pictures");
        assert!(!map.contains_key("music"));
    }

    // ── Logging config tests ──

    #[test]
    fn logging_config_defaults() {
        let config = LoggingConfig::default();
        assert!(matches!(config.level, LogLevel::Info));
        assert!(matches!(config.format, LogFormat::Pretty));
        assert!(matches!(config.color, ColorMode::Auto));
    }

    #[test]
    fn logging_config_rejects_unknown_fields() {
        let toml = r#"unknown_field = "value""#;
        let result: Result<LoggingConfig, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown logging fields must be rejected");
    }

    #[test]
    fn logging_config_parses_explicit_values() {
        let toml = r#"
level = "debug"
format = "json"
color = "never"
"#;
        let config: LoggingConfig = toml::from_str(toml).unwrap();
        assert!(matches!(config.level, LogLevel::Debug));
        assert!(matches!(config.format, LogFormat::Json));
        assert!(matches!(config.color, ColorMode::Never));
    }

    // ── Expected output validation tests ──

    #[test]
    fn validate_expected_output_accepts_file_stem_webm() {
        assert!(validate_expected_output_name("{file_stem}.Z.webm").is_ok());
    }

    #[test]
    fn validate_expected_output_accepts_file_name() {
        assert!(validate_expected_output_name("{file_name}").is_ok());
    }

    #[test]
    fn validate_expected_output_accepts_stem_placeholder() {
        assert!(validate_expected_output_name("{stem}.out").is_ok());
    }

    #[test]
    fn validate_expected_output_rejects_input_placeholder() {
        let err = validate_expected_output_name("{input}").unwrap_err();
        assert!(
            err.contains("{input}"),
            "error must mention {{input}}: {err}"
        );
    }

    #[test]
    fn validate_expected_output_rejects_parent_placeholder() {
        let err = validate_expected_output_name("{parent}/out").unwrap_err();
        assert!(
            err.contains("{parent}") || err.contains("separator"),
            "error must mention {{parent}} or separator: {err}"
        );
    }

    #[test]
    fn validate_expected_output_rejects_empty() {
        assert!(validate_expected_output_name("").is_err());
    }

    #[test]
    fn validate_expected_output_rejects_dot() {
        assert!(validate_expected_output_name(".").is_err());
    }

    #[test]
    fn validate_expected_output_rejects_dotdot() {
        assert!(validate_expected_output_name("..").is_err());
    }

    #[test]
    fn validate_expected_output_rejects_absolute() {
        assert!(validate_expected_output_name("/tmp/out.webm").is_err());
    }

    #[test]
    fn validate_expected_output_rejects_path_separator() {
        assert!(validate_expected_output_name("sub/out.webm").is_err());
    }
}
