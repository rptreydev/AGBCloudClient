// Phase 4: persistent sync-state tracking (not yet wired up).
#![allow(dead_code)]

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use tracing::debug;

use crate::config::AppConfig;
use crate::models::SyncedFile;

/// Persistent sync state — tracks which files are synced and their hashes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncState {
    pub files: HashMap<String, SyncedFile>,
    #[serde(default)]
    pub folder_last_sync: HashMap<String, i64>,
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            files: HashMap::new(),
            folder_last_sync: HashMap::new(),
        }
    }
}

impl SyncState {
    /// Load sync state from disk
    pub fn load() -> Result<Self> {
        let path = Self::state_path()?;
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            let state: SyncState = serde_json::from_str(&content)?;
            debug!("Loaded sync state: {} files tracked", state.files.len());
            Ok(state)
        } else {
            Ok(SyncState::default())
        }
    }

    /// Save sync state to disk
    pub fn save(&self) -> Result<()> {
        let path = Self::state_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }

    fn state_path() -> Result<std::path::PathBuf> {
        let data_dir = AppConfig::data_dir()?;
        Ok(data_dir.join("sync_state.json"))
    }
}
