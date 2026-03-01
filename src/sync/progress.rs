use std::sync::{Arc, Mutex};

/// Current phase of the sync engine.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum SyncPhase {
    #[default]
    Idle,
    Syncing,
    Error(String),
}

/// Shared progress state updated by the sync engine, read by the tray and settings UI.
#[derive(Debug, Clone, Default)]
pub struct SyncProgress {
    pub phase: SyncPhase,
    pub current_folder: String,
    pub current_file: String,
    pub files_done: usize,
    pub files_total: usize,
    /// Files actually downloaded this cycle (not skipped)
    pub files_downloaded: usize,
    /// When true, the sync engine skips its cycle (toggled from settings UI)
    pub paused: bool,
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

pub type SharedProgress = Arc<Mutex<SyncProgress>>;
