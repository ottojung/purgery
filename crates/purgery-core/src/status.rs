use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::path::*;
use crate::StatusError;

// ── Status Types ─────────────────────────────────────────────────────

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStatus {
    pub run_id: RunId,
    pub nickname: Nickname,
    pub state: RunState,
    #[serde(default)]
    pub entries: Vec<EntryStatusEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryStatusEntry {
    #[serde(default)]
    pub kind: ManifestEntryKind,
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

impl RunStatus {
    pub fn from_toml(input: &str) -> Result<Self, StatusError> {
        let status: RunStatus = toml::from_str(input)?;
        Ok(status)
    }

    pub fn to_toml(&self) -> Result<String, StatusError> {
        toml::to_string(self).map_err(|e| StatusError::TomlSerialize(e.to_string()))
    }
}
