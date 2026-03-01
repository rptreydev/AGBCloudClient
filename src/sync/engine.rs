use anyhow::Result;
use std::path::Path;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::models::{CloudFile, FolderSelection, SyncPolicy};
use crate::sync::progress::{SharedProgress, SyncPhase};
use crate::sync::remote::RemoteClient;

/// Orchestrates sync between local filesystem and remote API.
pub struct SyncEngine {
    remote: RemoteClient,
    progress: SharedProgress,
}

impl SyncEngine {
    pub fn new(auth: AuthState, progress: SharedProgress) -> Self {
        let remote = RemoteClient::new(auth);
        Self { remote, progress }
    }

    /// Main sync loop — runs forever, reloading config from disk each cycle.
    pub async fn run(&self) {
        info!("Sync engine started");
        loop {
            let config = match AppConfig::load_or_create() {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to reload config: {e}, sleeping 30s");
                    sleep(Duration::from_secs(30)).await;
                    continue;
                }
            };

            // Check if sync is paused
            let paused = self.progress.lock().map(|p| p.paused).unwrap_or(false);
            if paused {
                debug!("Sync paused, skipping cycle");
                sleep(Duration::from_secs(5)).await;
                continue;
            }

            // Reset download counter for this cycle
            self.update_progress(|p| p.files_downloaded = 0);

            if let Err(e) = self.sync_all(&config).await {
                error!("Sync cycle error: {e}");
                self.set_phase(SyncPhase::Error(e.to_string()));
            }

            // Show notification if new files were downloaded
            let downloaded = self.progress.lock().map(|p| p.files_downloaded).unwrap_or(0);
            if downloaded > 0 && config.notifications_enabled {
                Self::show_sync_notification(downloaded);
            }

            self.set_phase(SyncPhase::Idle);
            sleep(Duration::from_secs(config.sync_interval_secs)).await;
        }
    }

    fn show_sync_notification(count: usize) {
        let body = if count == 1 {
            "1 new file synced".to_string()
        } else {
            format!("{count} new files synced")
        };
        if let Err(e) = notify_rust::Notification::new()
            .appname("AGB Cloud Client")
            .summary("Sync Complete")
            .body(&body)
            .timeout(notify_rust::Timeout::Milliseconds(5000))
            .show()
        {
            debug!("Notification failed: {e}");
        }
    }

    async fn sync_all(&self, config: &AppConfig) -> Result<()> {
        if config.selected_folders.is_empty() {
            debug!("No folders selected for sync");
            return Ok(());
        }
        self.selective_sync(config).await
    }

    /// Sync only the user-selected folders/files.
    async fn selective_sync(&self, config: &AppConfig) -> Result<()> {
        let selections = filter_root_selections(&config.selected_folders);
        info!("Syncing {} root selection(s)", selections.len());

        let mut completed_uuids: Vec<String> = Vec::new();

        for sel in &selections {
            // Skip completed one-time copies
            if sel.policy == SyncPolicy::Copy && sel.completed {
                debug!("Skipping completed copy: {}", sel.name);
                continue;
            }

            self.update_progress(|p| {
                p.phase = SyncPhase::Syncing;
                p.current_folder = sel.name.clone();
                p.current_file.clear();
                p.files_done = 0;
                p.files_total = 0;
            });

            info!("Syncing: {} ({})", sel.name, sel.path);

            let base_path = Path::new(&config.sync_folder);

            // Use recursive get_children approach (more reliable than get_tree
            // because the /folders/:uuid endpoint has depth limiting)
            let folder_path = base_path.join(&sel.name);
            if let Err(e) = tokio::fs::create_dir_all(&folder_path).await {
                error!("Failed to create dir {}: {e}", folder_path.display());
                continue;
            }

            match self.sync_folder_recursive(&sel.uuid, &sel.name, &folder_path).await {
                Ok(()) => {
                    if sel.policy == SyncPolicy::Copy {
                        completed_uuids.push(sel.uuid.clone());
                    }
                }
                Err(e) => {
                    error!("Failed to sync {}: {e}", sel.name);
                }
            }
        }

        // Mark completed Copy folders and persist to config
        if !completed_uuids.is_empty() {
            let mut updated_config = config.clone();
            for folder in &mut updated_config.selected_folders {
                if completed_uuids.contains(&folder.uuid) {
                    folder.completed = true;
                    info!("Marked copy as completed: {}", folder.name);
                }
            }
            if let Err(e) = updated_config.save() {
                error!("Failed to save config after marking copies complete: {e}");
            }
        }

        let done = self.progress.lock().map(|p| p.files_done).unwrap_or(0);
        info!("Sync cycle complete ({done} files processed)");
        Ok(())
    }

    /// Recursively sync a folder by fetching its children via API.
    /// Uses get_children (depth:1) at each level instead of get_tree,
    /// which avoids the backend's depth limiting.
    async fn sync_folder_recursive(
        &self,
        folder_uuid: &str,
        folder_name: &str,
        local_path: &Path,
    ) -> Result<()> {
        self.update_progress(|p| {
            p.current_folder = folder_name.to_string();
        });

        let children = match self.remote.get_children(folder_uuid).await {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to fetch children of {folder_name}: {e}");
                return Err(e);
            }
        };

        for child in &children {
            if child.folder {
                let child_path = local_path.join(&child.name);
                if let Err(e) = tokio::fs::create_dir_all(&child_path).await {
                    error!("Failed to create dir {}: {e}", child_path.display());
                    continue;
                }
                Box::pin(self.sync_folder_recursive(&child.uuid, &child.name, &child_path)).await?;
            } else {
                let file_path = local_path.join(&child.name);
                self.download_file_if_needed(child, &file_path).await?;
            }
        }

        Ok(())
    }

    /// Download a remote file if it doesn't exist locally yet.
    async fn download_file_if_needed(&self, file: &CloudFile, dest: &Path) -> Result<()> {
        self.update_progress(|p| {
            p.files_total += 1;
        });

        if dest.exists() {
            // TODO: compare hashes for change detection
            debug!("File exists, skipping: {}", dest.display());
            self.update_progress(|p| p.files_done += 1);
            return Ok(());
        }

        self.update_progress(|p| {
            p.current_file = file.name.clone();
        });

        info!("Downloading: {} -> {}", file.name, dest.display());
        let dest_str = dest.to_string_lossy().to_string();
        self.remote.download_file(&file.uuid, &dest_str).await?;

        self.update_progress(|p| {
            p.files_done += 1;
            p.files_downloaded += 1;
        });
        Ok(())
    }

    fn set_phase(&self, phase: SyncPhase) {
        if let Ok(mut p) = self.progress.lock() {
            p.phase = phase;
        }
    }

    fn update_progress(&self, f: impl FnOnce(&mut crate::sync::progress::SyncProgress)) {
        if let Ok(mut p) = self.progress.lock() {
            f(&mut p);
        }
    }
}

/// Filter out folder selections that are children of other selections.
///
/// If both "ROOT / Photos" and "ROOT / Photos / 2024" are selected,
/// only "ROOT / Photos" is kept since syncing it already covers the child.
fn filter_root_selections(selections: &[FolderSelection]) -> Vec<&FolderSelection> {
    let paths: Vec<&str> = selections.iter().map(|s| s.path.as_str()).collect();
    selections
        .iter()
        .filter(|sel| {
            !paths.iter().any(|other_path| {
                *other_path != sel.path
                    && sel.path.starts_with(other_path)
                    && sel.path[other_path.len()..].starts_with(" / ")
            })
        })
        .collect()
}
