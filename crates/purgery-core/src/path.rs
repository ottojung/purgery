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

// ── Server work directory ──────────────────────────────────────────────

/// The server's absolute working directory for all runtime data.
///
/// Every run (incoming, ready, processing, done, failed) is organised
/// under a sub-tree rooted at this path:
///
/// ```text
/// <work_dir>/<nickname>/<phase>/<run_id>/
/// ```
///
/// The absolute-path invariant is established at the config boundary
/// (`ServerConfig::from_toml`), which resolves relative TOML paths
/// against `$HOME` before calling `ServerWorkDir::new`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerWorkDir(Utf8PathBuf);

impl ServerWorkDir {
    pub fn new(path: Utf8PathBuf) -> Result<Self, PathValidationError> {
        if !path.is_absolute() {
            return Err(PathValidationError::NotAbsolute);
        }
        Ok(ServerWorkDir(path))
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

impl Serialize for ServerWorkDir {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ServerWorkDir {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let p = Utf8PathBuf::deserialize(deserializer)?;
        ServerWorkDir::new(p).map_err(serde::de::Error::custom)
    }
}

/// Lexical intent carried by an rsync destination operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationIntent {
    ExactOrExistingDirectory,
    Directory,
}

/// A validated rsync destination operand.
///
/// `path` is normalized at the CLI/protocol boundary while `intent` preserves
/// the trailing slash which rsync uses to force directory interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DestinationPath {
    path: Utf8PathBuf,
    intent: DestinationIntent,
}

impl DestinationPath {
    /// Validates a client-supplied final destination at the CLI/config boundary.
    ///
    /// The path may be absolute or relative. Dot-dot components are rejected so
    /// joining a validated manifest relative path cannot escape this destination.
    pub fn new(path: Utf8PathBuf) -> Result<Self, PathValidationError> {
        if path.as_str().is_empty() {
            return Err(PathValidationError::EmptyComponent);
        }

        let directory_intent = path.as_str().ends_with('/');
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
        Ok(Self {
            path: normalized,
            intent: if directory_intent {
                DestinationIntent::Directory
            } else {
                DestinationIntent::ExactOrExistingDirectory
            },
        })
    }

    pub fn as_path(&self) -> &Utf8Path {
        &self.path
    }

    pub fn as_str(&self) -> &str {
        self.path.as_str()
    }

    pub fn intent(&self) -> DestinationIntent {
        self.intent
    }

    /// Reconstructs the path portion of the rsync operand, including forced
    /// directory intent. Root already contains its slash.
    pub fn operand(&self) -> String {
        if self.intent == DestinationIntent::Directory && self.path.as_str() != "/" {
            format!("{}/", self.path)
        } else {
            self.path.to_string()
        }
    }

    pub fn join(&self, relative: &NormalizedRelativePath) -> Utf8PathBuf {
        self.path.join(relative.as_path())
    }

    pub fn is_absolute(&self) -> bool {
        self.path.is_absolute()
    }
}

impl Serialize for DestinationPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.operand().serialize(serializer)
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

/// Filesystem classification of the destination operand at planning time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationState {
    Missing,
    Directory,
    NonDirectory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationPlacement {
    ExactTarget,
    DirectoryTarget,
}

/// The immutable target decision used by a transform and its retries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedDestinationPlan {
    pub operand: DestinationPath,
    pub target_path: Utf8PathBuf,
    pub target_directory: Utf8PathBuf,
    pub placement: DestinationPlacement,
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum DestinationResolutionError {
    #[error("destination requires a directory but is an existing non-directory")]
    DirectoryRequired,
    #[error("a directory source cannot replace an existing non-directory destination")]
    DirectoryOntoNonDirectory,
    #[error("resolved target has no parent directory")]
    MissingParent,
}

/// Applies rsync's single-source destination decision after the caller has
/// inspected the source and destination filesystems.
pub fn resolve_destination(
    operand: &DestinationPath,
    entry_path: &NormalizedRelativePath,
    source_kind: ManifestEntryKind,
    source_directory_empty: bool,
    destination_state: DestinationState,
) -> Result<ResolvedDestinationPlan, DestinationResolutionError> {
    if operand.intent() == DestinationIntent::Directory
        && destination_state == DestinationState::NonDirectory
    {
        return Err(DestinationResolutionError::DirectoryRequired);
    }
    if source_kind == ManifestEntryKind::Directory
        && destination_state == DestinationState::NonDirectory
    {
        return Err(DestinationResolutionError::DirectoryOntoNonDirectory);
    }

    let directory_target = operand.intent() == DestinationIntent::Directory
        || destination_state == DestinationState::Directory
        || (source_kind == ManifestEntryKind::Directory && !source_directory_empty);
    let (target_path, placement) = if directory_target {
        (
            operand.join(entry_path),
            DestinationPlacement::DirectoryTarget,
        )
    } else {
        (
            operand.as_path().to_owned(),
            DestinationPlacement::ExactTarget,
        )
    };
    let target_directory = target_path
        .parent()
        .ok_or(DestinationResolutionError::MissingParent)?
        .to_owned();
    Ok(ResolvedDestinationPlan {
        operand: operand.clone(),
        target_path,
        target_directory,
        placement,
    })
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

#[cfg(test)]
mod destination_tests {
    use super::*;

    fn operand(value: &str) -> DestinationPath {
        DestinationPath::new(value.into()).unwrap()
    }

    fn entry(value: &str) -> NormalizedRelativePath {
        NormalizedRelativePath::new(value.into()).unwrap()
    }

    #[test]
    fn operand_preserves_directory_intent_through_toml() {
        #[derive(Serialize, Deserialize)]
        struct Envelope {
            destination: DestinationPath,
        }
        for value in ["archive/name.mkv", "archive/name.mkv/", "/", "./archive/"] {
            let original = Envelope {
                destination: operand(value),
            };
            let encoded = toml::to_string(&original).unwrap();
            let decoded: Envelope = toml::from_str(&encoded).unwrap();
            assert_eq!(
                decoded.destination.operand(),
                original.destination.operand()
            );
        }
    }

    #[test]
    fn regular_file_destination_matrix() {
        let item = entry("item");
        let cases = [
            (
                "/archive/name",
                DestinationState::Missing,
                "/archive/name",
                DestinationPlacement::ExactTarget,
            ),
            (
                "/archive/name/",
                DestinationState::Missing,
                "/archive/name/item",
                DestinationPlacement::DirectoryTarget,
            ),
            (
                "/archive",
                DestinationState::Directory,
                "/archive/item",
                DestinationPlacement::DirectoryTarget,
            ),
            (
                "/archive/name",
                DestinationState::NonDirectory,
                "/archive/name",
                DestinationPlacement::ExactTarget,
            ),
        ];
        for (raw, state, expected, placement) in cases {
            let plan = resolve_destination(
                &operand(raw),
                &item,
                ManifestEntryKind::RegularFile,
                false,
                state,
            )
            .unwrap();
            assert_eq!(plan.target_path, expected);
            assert_eq!(plan.placement, placement);
        }
        assert!(resolve_destination(
            &operand("/archive/name/"),
            &item,
            ManifestEntryKind::RegularFile,
            false,
            DestinationState::NonDirectory
        )
        .is_err());
    }

    #[test]
    fn directory_empty_and_nonempty_follow_distinct_rename_rules() {
        let item = entry("source");
        let empty = resolve_destination(
            &operand("/archive/new"),
            &item,
            ManifestEntryKind::Directory,
            true,
            DestinationState::Missing,
        )
        .unwrap();
        assert_eq!(empty.target_path, "/archive/new");
        let populated = resolve_destination(
            &operand("/archive/new"),
            &item,
            ManifestEntryKind::Directory,
            false,
            DestinationState::Missing,
        )
        .unwrap();
        assert_eq!(populated.target_path, "/archive/new/source");
        assert!(resolve_destination(
            &operand("/archive/file"),
            &item,
            ManifestEntryKind::Directory,
            true,
            DestinationState::NonDirectory
        )
        .is_err());
    }

    #[test]
    fn original_video_regression_uses_exact_target_parent() {
        let plan = resolve_destination(
            &operand("/universe/mainvolume/myspace/sync/myfilebrowser/data/_data/todo/pc-todos/2026-03-14_12-16-42.mkv"),
            &entry("2026-03-14_12-16-42.mkv"),
            ManifestEntryKind::RegularFile,
            false,
            DestinationState::Missing,
        ).unwrap();
        assert_eq!(plan.target_path, "/universe/mainvolume/myspace/sync/myfilebrowser/data/_data/todo/pc-todos/2026-03-14_12-16-42.mkv");
        assert_eq!(
            plan.target_directory,
            "/universe/mainvolume/myspace/sync/myfilebrowser/data/_data/todo/pc-todos"
        );
    }

    #[test]
    fn resolver_matches_installed_rsync_for_missing_destination() {
        use std::process::Command;
        let cases = [
            (ManifestEntryKind::RegularFile, false),
            (ManifestEntryKind::Directory, true),
            (ManifestEntryKind::Directory, false),
        ];
        for (kind, empty) in cases {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("item");
            let destination = temp.path().join("parents/missing");
            match kind {
                ManifestEntryKind::RegularFile => std::fs::write(&source, b"data").unwrap(),
                ManifestEntryKind::Directory => {
                    std::fs::create_dir(&source).unwrap();
                    if !empty {
                        std::fs::write(source.join("child"), b"data").unwrap();
                    }
                }
                ManifestEntryKind::Symlink => unreachable!(),
            }
            let status = Command::new("rsync")
                .args([
                    "--recursive",
                    "--partial",
                    "--inplace",
                    "--mkpath",
                    "--archive",
                    "--protect-args",
                    "--",
                ])
                .arg(&source)
                .arg(&destination)
                .status()
                .expect("installed rsync is required for the destination differential test");
            assert!(status.success());

            let operand =
                DestinationPath::new(Utf8PathBuf::from_path_buf(destination.clone()).unwrap())
                    .unwrap();
            let plan = resolve_destination(
                &operand,
                &entry("item"),
                kind,
                empty,
                DestinationState::Missing,
            )
            .unwrap();
            assert!(
                plan.target_path.exists(),
                "rsync did not create resolver target"
            );
        }
    }

    #[test]
    fn resolved_plan_round_trips_without_reclassification() {
        let plan = resolve_destination(
            &operand("/archive/renamed.mkv"),
            &entry("original.mkv"),
            ManifestEntryKind::RegularFile,
            false,
            DestinationState::Missing,
        )
        .unwrap();
        let encoded = toml::to_string(&plan).unwrap();
        let recovered: ResolvedDestinationPlan = toml::from_str(&encoded).unwrap();
        assert_eq!(recovered, plan);

        let transform = crate::TransformDefinition {
            name: "target-name".into(),
            kind: crate::TransformKind::Subprocess,
            program: "true".into(),
            args: vec!["{target_file_name}".into(), "{target_file_stem}".into()],
            expected_outputs: vec!["{target_file_stem}.Z.webm".into()],
        };
        assert_eq!(
            transform.build_args_for_target(Utf8Path::new("/work/original.mkv"), &recovered),
            ["renamed.mkv", "renamed"]
        );
        assert_eq!(
            transform
                .resolve_expected_outputs_for_target(
                    Utf8Path::new("/work/original.mkv"),
                    &recovered,
                )
                .unwrap(),
            [Utf8PathBuf::from("/archive/renamed.Z.webm")]
        );
    }
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
