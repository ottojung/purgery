use serde::{Deserialize, Serialize};

// ── Durable Cleanup State ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableCleanupState {
    pub nickname: String,
    pub operation_id: String,
    pub entries: Vec<CleanupEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupEntry {
    pub sync_name: String,
    pub relative_path: String,
    pub local_path: String,
    pub size: u64,
    pub mtime_ns: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub rsync_succeeded: bool,
    pub cleaned: bool,
}
