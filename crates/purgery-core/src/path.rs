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
pub enum RemoteHostError {
    #[error("remote host is empty")]
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

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ClientLocalPathError {
    #[error("client local path is empty")]
    Empty,
}

// ── Run Phase ────────────────────────────────────────────────────────

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientLocalPath(String);

impl ClientLocalPath {
    pub fn new(s: String) -> Result<Self, ClientLocalPathError> {
        if s.is_empty() {
            return Err(ClientLocalPathError::Empty);
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

// ── Absolute path newtypes ───────────────────────────────────────────

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DestinationPath(Utf8PathBuf);

impl DestinationPath {
    /// Validates a client-supplied final destination at the CLI/config boundary.
    ///
    /// The path may be absolute or relative. Dot-dot components are rejected so
    /// joining a validated manifest relative path cannot escape this destination.
    pub fn new(path: Utf8PathBuf) -> Result<Self, PathValidationError> {
        if path.as_str().is_empty() {
            return Err(PathValidationError::EmptyComponent);
        }

        let absolute = path.is_absolute();
        let mut components = Vec::new();
        for component in path.components() {
            let value = component.as_str();
            match value {
                "/" if absolute => {}
                ".." => return Err(PathValidationError::ContainsDotDot),
                "." | "" => {}
                _ => components.push(value),
            }
        }

        let normalized = if absolute {
            if components.is_empty() {
                Utf8PathBuf::from("/")
            } else {
                Utf8PathBuf::from(format!("/{}", components.join("/")))
            }
        } else {
            if components.is_empty() {
                return Err(PathValidationError::EmptyComponent);
            }
            Utf8PathBuf::from(components.join("/"))
        };
        Ok(Self(normalized))
    }

    pub fn as_path(&self) -> &Utf8Path {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn join(&self, relative: &NormalizedRelativePath) -> Utf8PathBuf {
        self.0.join(relative.as_path())
    }

    pub fn is_absolute(&self) -> bool {
        self.0.is_absolute()
    }
}

impl Serialize for DestinationPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DestinationPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let path = Utf8PathBuf::deserialize(deserializer)?;
        Self::new(path).map_err(serde::de::Error::custom)
    }
}

// ── Path Normalization ──────────────────────────────────────────────

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

// ── Manifest Types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestEntryKind {
    Directory,
    #[default]
    RegularFile,
    Symlink,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestEntryMode {
    Passthrough,
    #[default]
    Postprocess,
}

// ── Path Safety ──────────────────────────────────────────────────────

pub fn path_is_within_root(resolved: &Utf8Path, root: &Utf8Path) -> bool {
    resolved.starts_with(root)
}

// ── Symlink Escape Hardening ────────────────────────────────────────

pub fn check_symlink_in_path(
    final_path: &Utf8Path,
    destination_root: &Utf8Path,
) -> Result<(), String> {
    let relative = final_path.strip_prefix(destination_root).map_err(|_| {
        format!(
            "final path '{}' is not under destination '{}'",
            final_path.as_str(),
            destination_root.as_str()
        )
    })?;

    let mut current = destination_root.to_owned();
    for component in relative.components() {
        current = current.join(component.as_str());
        match std::fs::symlink_metadata(current.as_std_path()) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("symlink detected in path: {}", current.as_str()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to read metadata for '{}': {error}",
                    current.as_str()
                ));
            }
        }
    }
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_joins_absolute_path_without_work_dir() {
        let destination = DestinationPath::new("/universe/synced/videos".into()).unwrap();
        let relative = NormalizedRelativePath::new("trip/a.mp4".into()).unwrap();
        assert_eq!(
            destination.join(&relative).as_str(),
            "/universe/synced/videos/trip/a.mp4"
        );
    }

    #[test]
    fn destination_joins_relative_path_without_rewriting_it() {
        let destination = DestinationPath::new("incoming/videos".into()).unwrap();
        let relative = NormalizedRelativePath::new("trip/a.mp4".into()).unwrap();
        assert_eq!(
            destination.join(&relative).as_str(),
            "incoming/videos/trip/a.mp4"
        );
    }

    // ── Server config parsing ──────────────────

    #[test]
    fn server_config_keeps_work_dir() {
        let toml = r#"
work_dir = "/var/lib/purgery/work"
"#;
        let config = crate::ServerConfig::from_toml(toml).unwrap();
        assert!(!config.work_dir.as_str().is_empty());
    }

    #[test]
    fn server_config_rejects_missing_work_dir() {
        let toml = r#"
"#;
        let result = crate::ServerConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "server config without work_dir must be rejected"
        );
    }
}
