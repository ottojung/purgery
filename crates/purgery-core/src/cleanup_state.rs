use serde::{Deserialize, Serialize};

use crate::ManifestEntryKind;

// ── Durable Cleanup State ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableCleanupState {
    pub nickname: String,
    pub operation_id: String,
    pub entries: Vec<CleanupEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupEntry {
    pub relative_path: String,
    pub local_path: String,
    #[serde(default)]
    pub kind: ManifestEntryKind,
    pub size: u64,
    pub mtime_ns: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default)]
    pub link_target: Option<String>,
    #[serde(default)]
    pub import_confirmed: bool,
    pub cleaned: bool,
}
