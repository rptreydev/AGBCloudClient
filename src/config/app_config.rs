use anyhow::Result;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tracing::info;

use crate::models::FolderSelection;

/// Application configuration persisted to disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// API server URL (e.g., "https://api.agbroadband.net/api/v2")
    pub server_url: String,

    /// WebSocket server URL (e.g., "https://api.agbroadband.net")
    pub websocket_url: String,

    /// Local folder to sync files to
    pub sync_folder: String,

    /// Sync interval in seconds
    pub sync_interval_secs: u64,

    /// Auto-start with Windows
    pub auto_start: bool,

    /// Show native notifications
    pub notifications_enabled: bool,

    /// Max concurrent uploads
    pub max_concurrent_uploads: usize,

    /// Max concurrent downloads
    pub max_concurrent_downloads: usize,

    /// Whether the initial setup wizard has been completed
    #[serde(default)]
    pub setup_complete: bool,

    /// Folders selected by the user for sync/copy
    #[serde(default)]
    pub selected_folders: Vec<FolderSelection>,
}

impl Default for AppConfig {
    fn default() -> Self {
        let sync_folder = dirs_default_sync_folder();
        Self {
            server_url: "https://api.agbroadband.net/api/v2".to_string(),
            websocket_url: "https://api.agbroadband.net".to_string(),
            sync_folder,
            sync_interval_secs: 30,
            auto_start: false,
            notifications_enabled: true,
            max_concurrent_uploads: 3,
            max_concurrent_downloads: 3,
            setup_complete: false,
            selected_folders: vec![],
        }
    }
}

impl AppConfig {
    /// Load config from disk or create default
    pub fn load_or_create() -> Result<Self> {
        let config_path = Self::config_path()?;

        if config_path.exists() {
            let content = fs::read_to_string(&config_path)?;
            let config: AppConfig = serde_json::from_str(&content)?;
            Ok(config)
        } else {
            let config = AppConfig::default();
            config.save()?;
            info!("Created default config at: {}", config_path.display());
            Ok(config)
        }
    }

    /// Save config to disk
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path()?;
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&config_path, content)?;
        Ok(())
    }

    /// Get the config file path
    /// Windows: %APPDATA%/AGBroadband/AGBCloudClient/config.json
    /// Linux: ~/.config/agb-cloud-client/config.json
    fn config_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("com", "AGBroadband", "AGBCloudClient")
            .ok_or_else(|| anyhow::anyhow!("Could not determine config directory"))?;
        Ok(dirs.config_dir().join("config.json"))
    }

    /// Get the data directory for sync state database
    pub fn data_dir() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("com", "AGBroadband", "AGBCloudClient")
            .ok_or_else(|| anyhow::anyhow!("Could not determine data directory"))?;
        let path = dirs.data_dir().to_path_buf();
        fs::create_dir_all(&path)?;
        Ok(path)
    }
}

/// Default sync folder location
fn dirs_default_sync_folder() -> String {
    if let Some(user_dirs) = directories::UserDirs::new() {
        let folder = user_dirs.home_dir().join("CloudFiles");
        return folder.to_string_lossy().to_string();
    }
    "CloudFiles".to_string()
}
