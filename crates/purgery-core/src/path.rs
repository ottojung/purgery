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
pub enum RootNameError {
    #[error("root name is empty")]
    Empty,
    #[error("root name contains invalid character: {0:?}")]
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

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ClientSourceError {
    #[error("source is empty")]
    Empty,
    #[error("invalid client root name: {0}")]
    InvalidRootName(#[from] RootNameError),
    #[error("source starts with '/': {0}")]
    AbsolutePath(String),
    #[error("source contains '..' component: {0}")]
    ContainsDotDot(String),
    #[error("source contains empty path component: {0}")]
    EmptyComponent(String),
    #[error("source is '.'")]
    IsDot,
    #[error("source starts with './': {0}")]
    StartsWithDotSlash(String),
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum SyncDestinationError {
    #[error("destination is empty")]
    Empty,
    #[error("invalid root name: {0}")]
    InvalidRootName(#[from] RootNameError),
    #[error("destination starts with '/': {0}")]
    AbsolutePath(String),
    #[error("destination contains '..' component: {0}")]
    ContainsDotDot(String),
    #[error("destination contains empty path component: {0}")]
    EmptyComponent(String),
    #[error("destination is '.'")]
    IsDot,
    #[error("destination starts with './': {0}")]
    StartsWithDotSlash(String),
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootName(String);

impl RootName {
    pub fn new(s: String) -> Result<Self, RootNameError> {
        if s.is_empty() {
            return Err(RootNameError::Empty);
        }
        for ch in s.chars() {
            if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' {
                return Err(RootNameError::InvalidCharacter(ch));
            }
        }
        Ok(RootName(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RootName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for RootName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RootName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        RootName::new(s).map_err(serde::de::Error::custom)
    }
}

impl FromStr for RootName {
    type Err = RootNameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RootName::new(s.to_owned())
    }
}

/// A validated client root name. Validation is established by `new` or serde.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientRootName(RootName);

impl ClientRootName {
    pub fn new(value: String) -> Result<Self, RootNameError> {
        RootName::new(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for ClientRootName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ClientRootName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_str().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ClientRootName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A root-qualified client source. Parsing proves the value is relative,
/// normalized, and starts with a valid client root name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSource {
    root_name: ClientRootName,
    path_under_root: Option<NormalizedRelativePath>,
}

impl ClientSource {
    pub fn parse(input: &str) -> Result<Self, ClientSourceError> {
        if input.is_empty() {
            return Err(ClientSourceError::Empty);
        }
        if input == "." {
            return Err(ClientSourceError::IsDot);
        }
        if input.starts_with('/') {
            return Err(ClientSourceError::AbsolutePath(input.to_owned()));
        }
        if input.starts_with("./") {
            return Err(ClientSourceError::StartsWithDotSlash(input.to_owned()));
        }
        for component in input.split('/') {
            if component == ".." {
                return Err(ClientSourceError::ContainsDotDot(input.to_owned()));
            }
            if component.is_empty() {
                return Err(ClientSourceError::EmptyComponent(input.to_owned()));
            }
        }
        let mut parts = input.splitn(2, '/');
        let root_name = ClientRootName::new(parts.next().expect("non-empty source").to_owned())?;
        let path_under_root = parts
            .next()
            .map(|rest| NormalizedRelativePath::new(Utf8PathBuf::from(rest)))
            .transpose()
            .map_err(|error| match error {
                PathValidationError::ContainsDotDot => {
                    ClientSourceError::ContainsDotDot(input.to_owned())
                }
                PathValidationError::EmptyComponent => {
                    ClientSourceError::EmptyComponent(input.to_owned())
                }
                PathValidationError::NotRelative | PathValidationError::NotAbsolute => {
                    ClientSourceError::AbsolutePath(input.to_owned())
                }
            })?;
        Ok(Self {
            root_name,
            path_under_root,
        })
    }

    pub fn root_name(&self) -> &ClientRootName {
        &self.root_name
    }

    pub fn path_under_root(&self) -> Option<&NormalizedRelativePath> {
        self.path_under_root.as_ref()
    }

    pub fn qualified_path(&self) -> Utf8PathBuf {
        let mut path = Utf8PathBuf::from(self.root_name.as_str());
        if let Some(rest) = &self.path_under_root {
            path.push(rest.as_path());
        }
        path
    }
}

impl Serialize for ClientSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.qualified_path().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ClientSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A named server root — an archive root path with a client-visible name.
///
/// The client sync `to` field references a named root by its first path component:
///
/// ```text
/// to = "univ/videos"
/// ```
///
/// means:
///
/// ```text
/// <server root named "univ"> / videos / <relative uploaded path>
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedRoot {
    pub name: RootName,
    pub path: ServerRoot,
}

/// A client sync destination parsed from the `to` field.
///
/// The first path component is the named server root; the remainder (if any) is
/// the path under that root. For example:
///
/// * `"univ/videos"` → root `"univ"`, path-under-root `"videos"`
/// * `"system"` → root `"system"`, no path-under-root
///
/// Final archive paths are built as:
///
/// ```text
/// <named root base path> / <path-under-root, if any> / <relative uploaded path>
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientDest {
    pub root_name: RootName,
    pub path_under_root: Option<NormalizedRelativePath>,
}

impl ClientDest {
    pub fn parse(input: &str) -> Result<Self, SyncDestinationError> {
        if input.is_empty() {
            return Err(SyncDestinationError::Empty);
        }
        if input == "." {
            return Err(SyncDestinationError::IsDot);
        }
        if input.starts_with('/') {
            return Err(SyncDestinationError::AbsolutePath(input.to_owned()));
        }
        if input.starts_with("./") {
            return Err(SyncDestinationError::StartsWithDotSlash(input.to_owned()));
        }

        for component in input.split('/') {
            if component == ".." {
                return Err(SyncDestinationError::ContainsDotDot(input.to_owned()));
            }
            if component.is_empty() {
                return Err(SyncDestinationError::EmptyComponent(input.to_owned()));
            }
        }

        let mut parts = input.splitn(2, '/');
        let root_name_str = parts.next().unwrap();
        let root_name = RootName::new(root_name_str.to_owned())?;

        let path_under_root = match parts.next() {
            None | Some("") => None,
            Some(rest) => Some(
                NormalizedRelativePath::new(Utf8PathBuf::from(rest)).map_err(|e| match e {
                    PathValidationError::NotRelative => {
                        SyncDestinationError::AbsolutePath(input.to_owned())
                    }
                    PathValidationError::ContainsDotDot => {
                        SyncDestinationError::ContainsDotDot(input.to_owned())
                    }
                    PathValidationError::EmptyComponent => {
                        SyncDestinationError::EmptyComponent(input.to_owned())
                    }
                    PathValidationError::NotAbsolute => {
                        SyncDestinationError::AbsolutePath(input.to_owned())
                    }
                })?,
            ),
        };

        Ok(ClientDest {
            root_name,
            path_under_root,
        })
    }

    pub fn root_name(&self) -> &RootName {
        &self.root_name
    }

    pub fn path_under_root(&self) -> Option<&NormalizedRelativePath> {
        self.path_under_root.as_ref()
    }

    /// Returns the root-qualified relative destination used in staging and status paths.
    pub fn qualified_path(&self) -> Utf8PathBuf {
        let mut path = Utf8PathBuf::from(self.root_name.as_str());
        if let Some(path_under_root) = &self.path_under_root {
            path.push(path_under_root.as_path());
        }
        path
    }

    pub fn status_path_for(&self, relative_path: &NormalizedRelativePath) -> String {
        self.qualified_path()
            .join(relative_path.as_path())
            .to_string()
    }
}

impl Serialize for ClientDest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.qualified_path().as_str().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ClientDest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        ClientDest::parse(&value).map_err(serde::de::Error::custom)
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

    /// Build the final archive path under a named root.
    ///
    /// The nickname is not part of the final archive path. The path is:
    ///
    /// ```text
    /// <root base path> / <path-under-root> / <relative uploaded path>
    /// ```
    ///
    /// If `path_under_root` is None, the entry lands directly under the root.
    pub fn final_path_under(
        &self,
        path_under_root: Option<&NormalizedRelativePath>,
        rel_path: &NormalizedRelativePath,
    ) -> Utf8PathBuf {
        let mut p = self.0.clone();
        if let Some(sub) = path_under_root {
            p = p.join(sub.as_path());
        }
        p.join(rel_path.as_path())
    }
}

impl NamedRoot {
    /// Resolve a final archive path under this named root.
    ///
    /// The nickname is deliberately absent from the path. The result is:
    ///
    /// ```text
    /// <this root's base path> / <path-under-root> / <relative path>
    /// ```
    pub fn final_path(
        &self,
        path_under_root: Option<&NormalizedRelativePath>,
        rel_path: &NormalizedRelativePath,
    ) -> Utf8PathBuf {
        self.path.final_path_under(path_under_root, rel_path)
    }

    /// Resolve a direct rsync destination for this named root.
    ///
    /// Returns the absolute path where the rsync destination should point,
    /// without any relative entry path appended.
    pub fn passthrough_destination(
        &self,
        path_under_root: Option<&NormalizedRelativePath>,
    ) -> Utf8PathBuf {
        if let Some(sub) = path_under_root {
            self.path.as_path().join(sub.as_path())
        } else {
            self.path.as_path().to_owned()
        }
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
    Covered,
}

// ── Path Safety ──────────────────────────────────────────────────────

pub fn path_is_within_root(resolved: &Utf8Path, root: &Utf8Path) -> bool {
    resolved.starts_with(root)
}

// ── Symlink Escape Hardening ────────────────────────────────────────

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

    // ── RootName tests ────────────────────────────────────────────────

    #[test]
    fn root_name_valid() {
        let n = RootName::new("univ".into()).unwrap();
        assert_eq!(n.as_str(), "univ");
    }

    #[test]
    fn root_name_with_hyphen() {
        let n = RootName::new("my-root".into()).unwrap();
        assert_eq!(n.as_str(), "my-root");
    }

    #[test]
    fn root_name_empty_rejected() {
        assert!(RootName::new("".into()).is_err());
    }

    #[test]
    fn root_name_rejects_slash() {
        assert!(RootName::new("bad/name".into()).is_err());
    }

    #[test]
    fn root_name_rejects_space() {
        assert!(RootName::new("my root".into()).is_err());
    }

    #[test]
    fn root_name_from_str() {
        let n: RootName = "system".parse().unwrap();
        assert_eq!(n.as_str(), "system");
    }

    #[test]
    fn root_name_serde_roundtrip() {
        let n = RootName::new("univ".into()).unwrap();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"univ\"");
        let back: RootName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
    }

    // ── ClientDest tests ─────────────────────────────────────────

    #[test]
    fn sync_dest_to_univ_videos_parses_root_and_path() {
        let d = ClientDest::parse("univ/videos").unwrap();
        assert_eq!(d.root_name.as_str(), "univ");
        assert_eq!(d.path_under_root.unwrap().as_str(), "videos");
    }

    #[test]
    fn sync_dest_to_system_parses_root_only() {
        let d = ClientDest::parse("system").unwrap();
        assert_eq!(d.root_name.as_str(), "system");
        assert!(d.path_under_root.is_none());
    }

    #[test]
    fn sync_dest_to_univ_a_b_c_parses_correctly() {
        let d = ClientDest::parse("univ/a/b/c").unwrap();
        assert_eq!(d.root_name.as_str(), "univ");
        assert_eq!(d.path_under_root.unwrap().as_str(), "a/b/c");
    }

    #[test]
    fn sync_dest_empty_rejected() {
        assert!(ClientDest::parse("").is_err());
    }

    #[test]
    fn sync_dest_absolute_rejected() {
        assert!(ClientDest::parse("/univ/videos").is_err());
    }

    #[test]
    fn sync_dest_dot_rejected() {
        assert!(ClientDest::parse(".").is_err());
    }

    #[test]
    fn sync_dest_dot_slash_rejected() {
        assert!(ClientDest::parse("./univ/videos").is_err());
    }

    #[test]
    fn sync_dest_dotdot_rejected() {
        assert!(ClientDest::parse("univ/../videos").is_err());
    }

    #[test]
    fn sync_dest_dotdot_bare_rejected() {
        assert!(ClientDest::parse("../univ/videos").is_err());
    }

    #[test]
    fn sync_dest_component_level_dotdot_not_triggered_by_substring() {
        let d = ClientDest::parse("univ/some..file").unwrap();
        assert_eq!(d.root_name.as_str(), "univ");
        assert_eq!(d.path_under_root.unwrap().as_str(), "some..file");
    }

    #[test]
    fn sync_dest_empty_component_rejected_via_double_slash() {
        assert!(ClientDest::parse("univ//videos").is_err());
    }

    // ── NamedRoot final path tests ────────────────────────────────────

    #[test]
    fn named_root_final_path_univ_videos_with_subpath() {
        let root = NamedRoot {
            name: RootName::new("univ".into()).unwrap(),
            path: ServerRoot::new(Utf8PathBuf::from("/universe/synced")).unwrap(),
        };
        let rel = NormalizedRelativePath::new("trips/a.mp4".into()).unwrap();
        let sub = NormalizedRelativePath::new("videos".into()).unwrap();
        let result = root.final_path(Some(&sub), &rel);
        assert_eq!(result.as_str(), "/universe/synced/videos/trips/a.mp4");
    }

    #[test]
    fn named_root_final_path_system_no_subpath() {
        let root = NamedRoot {
            name: RootName::new("system".into()).unwrap(),
            path: ServerRoot::new(Utf8PathBuf::from("/etc/system")).unwrap(),
        };
        let rel = NormalizedRelativePath::new("nginx/site.conf".into()).unwrap();
        let result = root.final_path(None, &rel);
        assert_eq!(result.as_str(), "/etc/system/nginx/site.conf");
    }

    // ── Passthrough destination tests ─────────────────────────────────

    #[test]
    fn named_root_passthrough_dest_univ_videos() {
        let root = NamedRoot {
            name: RootName::new("univ".into()).unwrap(),
            path: ServerRoot::new(Utf8PathBuf::from("/universe/synced")).unwrap(),
        };
        let sub = NormalizedRelativePath::new("videos".into()).unwrap();
        let dest = root.passthrough_destination(Some(&sub));
        assert_eq!(dest.as_str(), "/universe/synced/videos");
    }

    #[test]
    fn named_root_passthrough_dest_system_no_subpath() {
        let root = NamedRoot {
            name: RootName::new("system".into()).unwrap(),
            path: ServerRoot::new(Utf8PathBuf::from("/etc/system")).unwrap(),
        };
        let dest = root.passthrough_destination(None);
        assert_eq!(dest.as_str(), "/etc/system");
    }

    // ── ServerRoot::final_path_under (nickname-free) tests ────────────

    #[test]
    fn final_path_under_with_subpath() {
        let root = ServerRoot::new(Utf8PathBuf::from("/universe/synced")).unwrap();
        let sub = NormalizedRelativePath::new("videos".into()).unwrap();
        let rel = NormalizedRelativePath::new("trips/a.mp4".into()).unwrap();
        let result = root.final_path_under(Some(&sub), &rel);
        assert_eq!(result.as_str(), "/universe/synced/videos/trips/a.mp4");
    }

    #[test]
    fn final_path_under_without_subpath() {
        let root = ServerRoot::new(Utf8PathBuf::from("/etc/system")).unwrap();
        let rel = NormalizedRelativePath::new("nginx/site.conf".into()).unwrap();
        let result = root.final_path_under(None, &rel);
        assert_eq!(result.as_str(), "/etc/system/nginx/site.conf");
    }

    // ── server config parsing ──────────────────

    #[test]
    fn server_config_accepts_multiple_roots() {
        let toml = r#"
work_dir = "/var/lib/purgery/work"

[[root]]
name = "univ"
path = "/universe/synced"

[[root]]
name = "system"
path = "/etc/system"
"#;
        let _config = crate::ServerConfig::from_toml(toml).unwrap();
    }

    #[test]
    fn server_config_rejects_missing_roots() {
        let toml = r#"
work_dir = "/var/lib/purgery/work"
"#;
        let result = crate::ServerConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "server config requires at least one [[root]]"
        );
    }

    #[test]
    fn server_config_rejects_duplicate_root_names() {
        let toml = r#"
work_dir = "/var/lib/purgery/work"

[[root]]
name = "univ"
path = "/universe/synced"

[[root]]
name = "univ"
path = "/other/path"
"#;
        let result = crate::ServerConfig::from_toml(toml);
        assert!(result.is_err(), "duplicate root names must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("duplicate"),
            "error must mention duplicate names, got: {err_msg}"
        );
    }

    #[test]
    fn server_config_rejects_empty_root_name() {
        let toml = r#"
work_dir = "/var/lib/purgery/work"

[[root]]
name = ""
path = "/universe/synced"
"#;
        let result = crate::ServerConfig::from_toml(toml);
        assert!(result.is_err(), "empty root name must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("empty") || err_msg.contains("root name"),
            "error must mention empty name or root name, got: {err_msg}"
        );
    }

    #[test]
    fn server_config_rejects_invalid_root_name_chars() {
        let toml = r#"
work_dir = "/var/lib/purgery/work"

[[root]]
name = "bad/name"
path = "/universe/synced"
"#;
        let result = crate::ServerConfig::from_toml(toml);
        assert!(result.is_err(), "root name with slash must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("invalid character") || err_msg.contains("root name"),
            "error must mention invalid character or root name, got: {err_msg}"
        );
    }

    #[test]
    fn server_config_rejects_relative_root_path() {
        let toml = r#"
work_dir = "/var/lib/purgery/work"

[[root]]
name = "univ"
path = "relative/path"
"#;
        let result = crate::ServerConfig::from_toml(toml);
        assert!(result.is_err(), "relative root path must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("absolute") || err_msg.contains("not absolute"),
            "error must mention non-absolute path, got: {err_msg}"
        );
    }

    #[test]
    fn server_config_rejects_missing_work_dir() {
        let toml = r#"
[[root]]
name = "univ"
path = "/universe/synced"
"#;
        let result = crate::ServerConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "server config without work_dir must be rejected"
        );
    }

    #[test]
    fn server_config_rejects_top_level_root_field() {
        let toml = r#"
root = "/universe/synced"
work_dir = "/var/lib/purgery/work"
"#;
        let result = crate::ServerConfig::from_toml(toml);
        assert!(
            result.is_err(),
            "top-level 'root = ' must be rejected; use [[root]]"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("[[root]]")
                || err_msg.contains("named root")
                || err_msg.contains("root"),
            "error must reference named-root format, got: {err_msg}"
        );
    }

    #[test]
    fn server_config_keeps_work_dir() {
        let toml = r#"
work_dir = "/var/lib/purgery/work"

[[root]]
name = "univ"
path = "/universe/synced"
"#;
        let config = crate::ServerConfig::from_toml(toml).unwrap();
        assert!(!config.work_dir.as_str().is_empty());
    }

    // ── Client destination config tests ─────────────────────────────

    #[test]
    fn client_config_parses_two_root_qualified_sync_groups() {
        let toml = r#"
nickname = "laptop"
state_dir = "/var/lib/purgery"

[server]
host = "example.com"

[[root]]
name = "videos"
path = "/home/user/Videos"

[[sync]]
from = "videos"
to = "univ/videos"

[[root]]
name = "server-configs"
path = "/home/user/my/server-configs"

[[sync]]
from = "server-configs"
to = "system/server-configs"
"#;
        let config = crate::ClientConfig::from_toml(toml).unwrap();
        assert_eq!(config.sync.len(), 2);
        assert_eq!(config.sync[0].name.as_str(), "sync-0001");
        assert_eq!(config.sync[1].name.as_str(), "sync-0002");
    }

    #[test]
    fn client_config_rejects_to_empty() {
        let toml = r#"
nickname = "laptop"
state_dir = "/var/lib/purgery"

[server]
host = "example.com"

[[root]]
name = "videos"
path = "/home/user/Videos"

[[sync]]
from = "videos"
to = ""
"#;
        let result = crate::ClientConfig::from_toml(toml);
        assert!(result.is_err(), "empty to must be rejected");
    }

    #[test]
    fn client_config_rejects_to_absolute() {
        let toml = r#"
nickname = "laptop"
state_dir = "/var/lib/purgery"

[server]
host = "example.com"

[[root]]
name = "videos"
path = "/home/user/Videos"

[[sync]]
from = "videos"
to = "/univ/videos"
"#;
        let result = crate::ClientConfig::from_toml(toml);
        assert!(result.is_err(), "absolute to must be rejected");
    }

    #[test]
    fn client_config_rejects_to_with_dotdot() {
        let toml = r#"
nickname = "laptop"
state_dir = "/var/lib/purgery"

[server]
host = "example.com"

[[root]]
name = "videos"
path = "/home/user/Videos"

[[sync]]
from = "videos"
to = "univ/../videos"
"#;
        let result = crate::ClientConfig::from_toml(toml);
        assert!(result.is_err(), "to with .. must be rejected");
    }

    // ── Status final_paths tests ───────────────────────────────────

    #[test]
    fn run_status_final_paths_root_qualified_relative_no_nickname() {
        let toml = r#"
run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
nickname = "laptop"
state = "done"

[[entries]]
sync_name = "videos"
local_path = "/home/user/Videos/a.mp4"
relative_path = "a.mp4"
status = "imported"
final_paths = ["univ/videos/a.mp4"]
postprocess = ["compress-video"]
"#;
        let status = crate::RunStatus::from_toml(toml).unwrap();
        assert_eq!(status.entries.len(), 1);
        let fp = &status.entries[0].final_paths;
        assert!(!fp.is_empty());
        for p in fp {
            assert!(
                !p.contains("laptop"),
                "final_paths must not contain nickname: {p}"
            );
        }
        assert_eq!(fp[0], "univ/videos/a.mp4");
    }

    // ── Two clients same root tests ─────────────────────────────────

    #[test]
    fn two_clients_same_root_no_nickname_injection() {
        let root = NamedRoot {
            name: RootName::new("univ".into()).unwrap(),
            path: ServerRoot::new(Utf8PathBuf::from("/universe/synced")).unwrap(),
        };
        let sub = NormalizedRelativePath::new("videos".into()).unwrap();

        let rel1 = NormalizedRelativePath::new("trips/a.mp4".into()).unwrap();
        let p1 = root.final_path(Some(&sub), &rel1);

        let rel2 = NormalizedRelativePath::new("clips/cat.mp4".into()).unwrap();
        let p2 = root.final_path(Some(&sub), &rel2);

        assert_eq!(p1.as_str(), "/universe/synced/videos/trips/a.mp4");
        assert_eq!(p2.as_str(), "/universe/synced/videos/clips/cat.mp4");

        assert!(!p1.as_str().contains("laptop"));
        assert!(!p1.as_str().contains("phone-dump"));
        assert!(!p2.as_str().contains("laptop"));
        assert!(!p2.as_str().contains("phone-dump"));
    }
}
