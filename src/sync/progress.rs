use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};

/// Current phase of the sync engine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum SyncPhase {
    #[default]
    Idle,
    Syncing,
    Error(String),
}

/// Shared progress state updated by the sync engine, read by the tray and settings UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncProgress {
    pub phase: SyncPhase,
    pub current_folder: String,
    pub current_file: String,
    pub files_done: usize,
    pub files_total: usize,
    /// Files actually downloaded this cycle (not skipped)
    pub files_downloaded: usize,
    /// Files synced (downloaded) this cycle
    pub files_synced: usize,
    /// Number of download failures this cycle
    pub files_failed: usize,
    /// Last download error message (for display)
    pub last_error: Option<String>,
    /// When true, the sync engine skips its cycle (toggled from settings UI)
    pub paused: bool,
    /// Set by status panel "Quit" — tray polls and exits when true
    #[serde(default)]
    pub quit_requested: bool,
    /// Set by status panel "Logout" — tray polls and performs logout when true
    #[serde(default)]
    pub logout_requested: bool,
    /// Set by Manage Folders after saving — tray notifies the engine to wake early
    #[serde(default)]
    pub sync_requested: bool,
}

impl SyncProgress {
    pub fn tooltip(&self) -> String {
        match &self.phase {
            SyncPhase::Idle => {
                if self.files_done > 0 {
                    format!("AGB Cloud Client — Up to date ({} files)", self.files_done)
                } else {
                    "AGB Cloud Client — Idle".to_string()
                }
            }
            SyncPhase::Syncing => {
                if !self.current_file.is_empty() {
                    format!(
                        "AGB Cloud Client — Downloading: {} ({} done)",
                        self.current_file, self.files_done
                    )
                } else {
                    format!(
                        "AGB Cloud Client — Syncing: {} ({} files)",
                        self.current_folder, self.files_done
                    )
                }
            }
            SyncPhase::Error(e) => {
                let short = if e.len() > 60 { &e[..60] } else { e };
                format!("AGB Cloud Client — Error: {short}")
            }
        }
    }

    /// Short status line for the settings window.
    pub fn status_line(&self) -> (String, bool) {
        if self.paused {
            return ("Sync paused".to_string(), false);
        }
        match &self.phase {
            SyncPhase::Idle => {
                if self.files_done > 0 {
                    (format!("Up to date — {} files synced", self.files_done), false)
                } else {
                    ("Idle — waiting for next sync cycle".to_string(), false)
                }
            }
            SyncPhase::Syncing => {
                let msg = if !self.current_file.is_empty() {
                    format!(
                        "Downloading: {} — {} files done",
                        self.current_file, self.files_done
                    )
                } else {
                    format!(
                        "Scanning: {} — {} files found",
                        self.current_folder, self.files_total
                    )
                };
                (msg, true) // true = is_active
            }
            SyncPhase::Error(e) => (format!("Error: {e}"), false),
        }
    }
}

// ── IPC: progress file for the status panel subprocess ───────────────────────

fn progress_file_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("APPDATA"))
        .unwrap_or_else(|_| ".".to_string());
    std::path::Path::new(&base)
        .join("AGBroadband")
        .join("AGBCloudClient")
        .join("progress.json")
}

/// Write the current progress state to disk so the status panel subprocess can read it.
pub fn write_progress_file(p: &SyncProgress) {
    if let Ok(json) = serde_json::to_string(p) {
        let path = progress_file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, json);
    }
}

/// Read the last persisted progress state. Returns default (Idle) if the file is missing or invalid.
pub fn read_progress_file() -> SyncProgress {
    let path = progress_file_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub type SharedProgress = Arc<Mutex<SyncProgress>>;
