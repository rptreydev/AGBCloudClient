use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::models::{CloudFile, FolderSelection, SyncPolicy};
use crate::sync::progress::{write_progress_file, SharedProgress, SyncPhase};
use crate::sync::remote::RemoteClient;
use crate::ui::common::NOTIFICATION_APP_ID;

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
    /// `sync_trigger` is a `Notify` that can interrupt the sleep between cycles
    /// so a newly-saved folder selection syncs immediately (no full interval wait).
    pub async fn run(&self, sync_trigger: Arc<Notify>) {
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

            // Reset download counters for this cycle
            self.update_progress(|p| {
                p.files_downloaded = 0;
                p.files_copied = 0;
                p.files_synced = 0;
                p.files_failed = 0;
                p.last_error = None;
            });

            if let Err(e) = self.sync_all(&config).await {
                error!("Sync cycle error: {e}");
                self.set_phase(SyncPhase::Error(e.to_string()));
            }

            // Show notification with breakdown if any files were downloaded
            let (copied, synced) = self.progress.lock()
                .map(|p| (p.files_copied, p.files_synced))
                .unwrap_or((0, 0));
            if (copied > 0 || synced > 0) && config.notifications_enabled {
                Self::show_sync_notification(copied, synced);
            }

            self.set_phase(SyncPhase::Idle);
            // Interruptible sleep: wakes up early when Manage Folders saves new selections.
            tokio::select! {
                _ = sleep(Duration::from_secs(config.sync_interval_secs)) => {}
                _ = sync_trigger.notified() => {
                    info!("Sync triggered early — new folder selections detected");
                }
            }
        }
    }

    fn show_sync_notification(copied: usize, synced: usize) {
        let mut lines = Vec::new();
        if copied > 0 {
            lines.push(format!(
                "{} file{} copied to your device",
                copied,
                if copied == 1 { "" } else { "s" }
            ));
        }
        if synced > 0 {
            lines.push(format!(
                "{} file{} kept up to date",
                synced,
                if synced == 1 { "" } else { "s" }
            ));
        }
        let body = lines.join("\n");
        if let Err(e) = notify_rust::Notification::new()
            .app_id(NOTIFICATION_APP_ID)
            .summary("Cloud Files — Sync Complete")
            .body(&body)
            .timeout(notify_rust::Timeout::Milliseconds(6000))
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
        // Guard: skip entire cycle if the sync folder is inaccessible
        // (network drive disconnected, USB removed, path deleted, etc.)
        if tokio::fs::metadata(&config.sync_folder).await.is_err() {
            warn!("Sync folder inaccessible: {} — skipping cycle", config.sync_folder);
            self.update_progress(|p| {
                p.last_error = Some(format!(
                    "Sync folder not found: {}",
                    config.sync_folder
                ));
            });
            return Ok(());
        }

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

            // Build the full local path preserving the remote tree structure.
            // sel.path is "ParentName / ChildName / ..." (built by build_path),
            // so split by " / " and join to get e.g. sync_folder/Photos/2024/.
            let folder_path = if !sel.path.is_empty() {
                sel.path.split(" / ").fold(base_path.to_path_buf(), |p, c| p.join(c.trim()))
            } else {
                base_path.join(sel.name.trim())
            };
            if let Err(e) = tokio::fs::create_dir_all(&folder_path).await {
                error!("Failed to create dir {}: {e}", folder_path.display());
                continue;
            }

            let before = self.progress.lock().map(|p| p.files_downloaded).unwrap_or(0);
            match self.sync_folder_recursive(&sel.uuid, &sel.name, &folder_path).await {
                Ok(()) => {
                    let delta = self.progress.lock()
                        .map(|p| p.files_downloaded.saturating_sub(before))
                        .unwrap_or(0);
                    match sel.policy {
                        SyncPolicy::Copy => {
                            self.update_progress(|p| p.files_copied += delta);
                            // Only mark as completed if files were actually downloaded.
                            // If delta == 0 (e.g. download failed or timed out), retry next cycle.
                            if delta > 0 {
                                completed_uuids.push(sel.uuid.clone());
                            }
                        }
                        SyncPolicy::KeepSynced { .. } => {
                            self.update_progress(|p| p.files_synced += delta);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to sync {}: {e}", sel.name);
                }
            }
        }

        // Mark completed Copy folders and persist to config.
        // IMPORTANT: reload a fresh copy from disk before saving so we don't overwrite
        // any settings changes (sync_folder, selected_folders, etc.) that the user may
        // have made via Settings or Manage Folders while this sync cycle was running.
        if !completed_uuids.is_empty() {
            match crate::config::AppConfig::load_or_create() {
                Ok(mut fresh_config) => {
                    for folder in &mut fresh_config.selected_folders {
                        if completed_uuids.contains(&folder.uuid) {
                            folder.completed = true;
                            info!("Marked copy as completed: {}", folder.name);
                        }
                    }
                    if let Err(e) = fresh_config.save() {
                        error!("Failed to save config after marking copies complete: {e}");
                    }
                }
                Err(e) => {
                    error!("Failed to reload config for marking copies complete: {e}");
                }
            }
        }

        let done = self.progress.lock().map(|p| p.files_done).unwrap_or(0);
        info!("Sync cycle complete ({done} files processed)");
        Ok(())
    }

    /// Recursively sync a folder by fetching its children via API.
    /// Uses get_children (depth:1) at each level instead of get_tree,
    /// which avoids the backend's depth limiting.
    ///
    /// If `folder_uuid` turns out to belong to a file (not a folder), the file
    /// is downloaded directly to `local_path` (after removing any empty directory
    /// placeholder that was created by `selective_sync`).
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
                self.update_progress(|p| {
                    p.files_failed += 1;
                    p.last_error = Some(format!("Listing '{folder_name}' failed: {e}"));
                });
                return Err(e);
            }
        };

        for child in &children {
            if child.folder {
                let child_path = local_path.join(child.name.trim());
                if let Err(e) = tokio::fs::create_dir_all(&child_path).await {
                    error!("Failed to create dir {}: {e}", child_path.display());
                    continue;
                }
                if let Err(e) = Box::pin(self.sync_folder_recursive(&child.uuid, &child.name, &child_path)).await {
                    error!("Failed to sync subfolder {}: {e}", child.name);
                    // Note: files_failed already incremented inside the recursive call
                }
            } else {
                let file_path = local_path.join(child.name.trim());
                if let Err(e) = self.download_file_if_needed(child, &file_path).await {
                    error!("Failed to download {}: {e}", child.name);
                    self.update_progress(|p| {
                        p.files_failed += 1;
                        p.last_error = Some(format!("{}: {e}", child.name));
                    });
                }
            }
        }

        Ok(())
    }

    /// Download a remote file if it doesn't exist locally yet.
    async fn download_file_if_needed(&self, file: &CloudFile, dest: &Path) -> Result<()> {
        // Skip virtual entries that have no actual file stored on the server.
        if file.no_file == Some(true) {
            debug!("Skipping no_file entry: {}", file.name);
            return Ok(());
        }

        self.update_progress(|p| {
            p.files_total += 1;
        });

        if dest.exists() {
            if dest.is_dir() {
                // A directory exists where a file should be — this happens when a file UUID
                // was accidentally saved as a sync target in a previous run, causing
                // `selective_sync` to call `create_dir_all` with the file's name.
                // Remove the empty directory so we can download the actual file.
                warn!("Directory found at file path — removing: {}", dest.display());
                if let Err(e) = tokio::fs::remove_dir(dest).await {
                    error!("Could not remove directory at {}: {e}", dest.display());
                    self.update_progress(|p| p.files_done += 1);
                    return Ok(()); // Can't overwrite a non-empty dir — skip for now
                }
                // Directory removed — fall through to download below
            } else {
                // Regular file already exists — skip (already synced)
                debug!("File exists, skipping: {}", dest.display());
                self.update_progress(|p| p.files_done += 1);
                return Ok(());
            }
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
            p.current_file.clear(); // clear so bar doesn't stay at half-credit after download
        });
        Ok(())
    }

    fn set_phase(&self, phase: SyncPhase) {
        if let Ok(mut p) = self.progress.lock() {
            p.phase = phase;
            write_progress_file(&p);
        }
    }

    fn update_progress(&self, f: impl FnOnce(&mut crate::sync::progress::SyncProgress)) {
        if let Ok(mut p) = self.progress.lock() {
            f(&mut p);
            // Write to disk so the status panel subprocess can read live progress.
            write_progress_file(&p);
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
