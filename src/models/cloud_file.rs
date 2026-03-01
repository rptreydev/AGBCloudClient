use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Mirrors the CloudFile entity from the NestJS API (materialized-path tree).
///
/// Uses `#[serde(default)]` liberally since different endpoints return
/// different subsets of fields (e.g., tree endpoints include `children`,
/// list endpoints may omit `id`, public endpoints omit relations).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudFile {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub ext: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub folder: bool,
    #[serde(default, rename = "protected")]
    pub is_protected: Option<bool>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default, rename = "hasGps")]
    pub has_gps: Option<bool>,
    #[serde(default, rename = "noFile")]
    pub no_file: Option<bool>,
    #[serde(default)]
    pub created: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
    #[serde(default)]
    pub deleted: Option<DateTime<Utc>>,
    #[serde(default)]
    pub children: Option<Vec<CloudFile>>,
}

/// Sync state for a local file
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SyncStatus {
    Synced,
    Uploading,
    Downloading,
    Pending,
    Error(String),
    Conflict,
}

/// Tracks the sync state of a file (local <-> remote)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncedFile {
    pub remote_uuid: String,
    pub local_path: String,
    pub remote_hash: Option<String>,
    pub local_hash: Option<String>,
    pub status: SyncStatus,
    pub last_synced: Option<DateTime<Utc>>,
    pub size: Option<i64>,
}

/// Sync policy for a selected folder
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SyncPolicy {
    /// Download once
    Copy,
    /// Keep synchronized periodically
    KeepSynced { interval_secs: u64 },
}

/// User selection for a folder to sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderSelection {
    pub uuid: String,
    pub name: String,
    /// Visual path: "ROOT / subfolder / child"
    pub path: String,
    pub policy: SyncPolicy,
    /// true if Copy already executed
    pub completed: bool,
}

/// WebSocket events from the server for real-time updates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudFileEvent {
    pub event_type: CloudFileEventType,
    pub file: CloudFile,
    pub user_name: Option<String>,
    pub folder_uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CloudFileEventType {
    Uploaded,
    Deleted,
    Moved,
    Renamed,
    FolderCreated,
}
