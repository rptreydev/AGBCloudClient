use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};

/// Maximum number of activity entries kept in the log.
const MAX_ACTIVITY_ENTRIES: usize = 20;

/// A single file event recorded by the sync engine for the Activity tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// Display name of the file (e.g. "photo.jpg").
    pub file_name: String,
    /// Relative folder path inside the sync root (e.g. "SHA / Building 3").
    pub folder: String,
    /// Human-readable action label (e.g. "Downloaded").
    pub action: String,
    /// Unix timestamp (seconds) when the action occurred.
    pub timestamp_secs: u64,
    /// File size in bytes after download.
    pub size_bytes: u64,
}

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
    /// When true, Windows toast notifications for file downloads are suppressed.
    /// Toggled from the tray context menu.
    #[serde(default)]
    pub notifications_muted: bool,
    /// Circular log of the last N downloaded files — shown in the Activity tab.
    #[serde(default)]
    pub activity_log: Vec<ActivityEntry>,
}

impl SyncProgress {
    pub fn tooltip(&self) -> String {
        match &self.phase {
            SyncPhase::Idle => {
                if self.files_done > 0 {
                    format!(
                        "AGB Cloud Client v{} — Up to date ({} files)",
                        env!("CARGO_PKG_VERSION"),
                        self.files_done
                    )
                } else {
                    format!("AGB Cloud Client v{} — Idle", env!("CARGO_PKG_VERSION"))
                }
            }
            SyncPhase::Syncing => {
                if !self.current_file.is_empty() {
                    format!(
                        "AGB Cloud Client v{} — Downloading: {} ({} done)",
                        env!("CARGO_PKG_VERSION"),
                        self.current_file,
                        self.files_done
                    )
                } else {
                    format!(
                        "AGB Cloud Client v{} — Syncing: {} ({} files)",
                        env!("CARGO_PKG_VERSION"),
                        self.current_folder,
                        self.files_done
                    )
                }
            }
            SyncPhase::Error(e) => {
                let short = if e.len() > 60 { &e[..60] } else { e };
                format!("AGB Cloud Client v{} — Error: {short}", env!("CARGO_PKG_VERSION"))
            }
        }
    }

    /// Prepend an entry to the activity log, keeping at most `MAX_ACTIVITY_ENTRIES`.
    /// Most-recent entry is always at index 0.
    pub fn push_activity(&mut self, entry: ActivityEntry) {
        self.activity_log.insert(0, entry);
        self.activity_log.truncate(MAX_ACTIVITY_ENTRIES);
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(name: &str, secs: u64) -> ActivityEntry {
        ActivityEntry {
            file_name: name.to_string(),
            folder: "SHA / Building 1".to_string(),
            action: "Downloaded".to_string(),
            timestamp_secs: secs,
            size_bytes: 1024,
        }
    }

    // ── push_activity ─────────────────────────────────────────────────────────

    #[test]
    fn push_activity_prepends_most_recent_first() {
        let mut p = SyncProgress::default();
        p.push_activity(make_entry("a.jpg", 1000));
        p.push_activity(make_entry("b.jpg", 2000));
        assert_eq!(p.activity_log[0].file_name, "b.jpg");
        assert_eq!(p.activity_log[1].file_name, "a.jpg");
    }

    #[test]
    fn push_activity_caps_at_max_entries() {
        let mut p = SyncProgress::default();
        for i in 0..=(MAX_ACTIVITY_ENTRIES + 5) {
            p.push_activity(make_entry(&format!("file{i}.jpg"), i as u64));
        }
        assert_eq!(p.activity_log.len(), MAX_ACTIVITY_ENTRIES);
    }

    #[test]
    fn push_activity_keeps_most_recent_when_capped() {
        let mut p = SyncProgress::default();
        for i in 0..=(MAX_ACTIVITY_ENTRIES + 2) {
            p.push_activity(make_entry(&format!("file{i}.jpg"), i as u64));
        }
        // Most recent (highest index) should be at position 0
        assert!(p.activity_log[0].file_name.contains(&format!("{}", MAX_ACTIVITY_ENTRIES + 2)));
    }

    // ── notifications_muted ───────────────────────────────────────────────────

    #[test]
    fn notifications_muted_defaults_to_false() {
        let p = SyncProgress::default();
        assert!(!p.notifications_muted);
    }

    #[test]
    fn notifications_muted_round_trips_through_json() {
        let mut p = SyncProgress::default();
        p.notifications_muted = true;
        let json = serde_json::to_string(&p).unwrap();
        let p2: SyncProgress = serde_json::from_str(&json).unwrap();
        assert!(p2.notifications_muted);
    }

    // ── ActivityEntry serialization ───────────────────────────────────────────

    #[test]
    fn activity_log_round_trips_through_json() {
        let mut p = SyncProgress::default();
        p.push_activity(make_entry("photo.jpg", 9999));
        let json = serde_json::to_string(&p).unwrap();
        let p2: SyncProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(p2.activity_log.len(), 1);
        assert_eq!(p2.activity_log[0].file_name, "photo.jpg");
        assert_eq!(p2.activity_log[0].timestamp_secs, 9999);
    }

    #[test]
    fn old_progress_json_without_new_fields_deserializes_with_defaults() {
        // Simulates reading a progress.json written by an older version
        // that did not have notifications_muted or activity_log.
        let json = r#"{"phase":"Idle","current_folder":"","current_file":"",
            "files_done":5,"files_total":5,"files_downloaded":2,"files_synced":2,
            "files_failed":0,"last_error":null,"paused":false,
            "quit_requested":false,"logout_requested":false,"sync_requested":false}"#;
        let p: SyncProgress = serde_json::from_str(json).unwrap();
        assert!(!p.notifications_muted);
        assert!(p.activity_log.is_empty());
        assert_eq!(p.files_done, 5);
    }
}
