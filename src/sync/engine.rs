use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::models::{CloudFile, FolderSelection};
use crate::sync::progress::{write_progress_file, SharedProgress, SyncPhase};
use crate::sync::remote::RemoteClient;

/// Orchestrates sync between local filesystem and remote API.
pub struct SyncEngine {
    remote: RemoteClient,
    progress: SharedProgress,
    /// Set to `true` from outside (e.g. when a company is removed while syncing)
    /// to abort the current cycle immediately. Reset at the start of each cycle.
    abort_flag: Arc<AtomicBool>,
}

impl SyncEngine {
    pub fn new(auth: AuthState, progress: SharedProgress) -> Self {
        let remote = RemoteClient::new(auth);
        Self { remote, progress, abort_flag: Arc::new(AtomicBool::new(false)) }
    }

    /// Returns a handle to the abort flag so external code can cancel a running cycle.
    pub fn abort_flag(&self) -> Arc<AtomicBool> {
        self.abort_flag.clone()
    }

    /// Main sync loop — runs forever, driven purely by WS events.
    /// On startup it performs one initial sync, then blocks on `sync_trigger`
    /// (notified by the WS client on every CLOUDFILE.* event).
    /// The fixed-interval timer has been removed: sync only runs when the
    /// server signals a change, keeping resource usage minimal.
    pub async fn run(&self, sync_trigger: Arc<Notify>) {
        info!("Sync engine started (event-driven mode — no interval timer)");

        // Initial sync on startup so local state matches server from the start.
        self.run_cycle().await;

        loop {
            // Block until the WS client fires notify_one().
            sync_trigger.notified().await;
            info!("Sync triggered by WS event");
            self.run_cycle().await;
        }
    }

    /// Run a single sync cycle: reload config, check pause, sync, set Idle.
    async fn run_cycle(&self) {
        let config = match AppConfig::load_or_create() {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to reload config: {e} — skipping cycle");
                return;
            }
        };

        // Check if sync is paused — consume the notification but do nothing.
        let paused = self.progress.lock().map(|p| p.paused).unwrap_or(false);
        if paused {
            debug!("Sync paused — skipping cycle");
            return;
        }

        // Reset abort flag and download counters for this cycle.
        self.abort_flag.store(false, Ordering::Relaxed);
        self.update_progress(|p| {
            p.files_downloaded = 0;
            p.files_synced = 0;
            p.files_failed = 0;
            p.last_error = None;
        });

        if let Err(e) = self.sync_all(&config).await {
            error!("Sync cycle error: {e}");
            self.set_phase(SyncPhase::Error(e.to_string()));
        }

        self.set_phase(SyncPhase::Idle);
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

        for sel in &selections {
            if self.abort_flag.load(Ordering::Relaxed) {
                warn!("Sync aborted — company removed during sync cycle");
                self.update_progress(|p| {
                    p.last_error = Some("Sync aborted: company access revoked".to_string());
                });
                return Ok(());
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
            match self.sync_folder_recursive(&sel.uuid, &sel.name, &folder_path, base_path).await {
                Ok(()) => {
                    let delta = self.progress.lock()
                        .map(|p| p.files_downloaded.saturating_sub(before))
                        .unwrap_or(0);
                    self.update_progress(|p| p.files_synced += delta);
                }
                Err(e) => {
                    error!("Failed to sync {}: {e}", sel.name);
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
        sync_folder: &Path,
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
            if self.abort_flag.load(Ordering::Relaxed) {
                info!("Sync aborted mid-folder '{folder_name}' — stopping recursive sync");
                return Ok(());
            }
            if child.folder {
                let child_path = local_path.join(child.name.trim());
                if let Err(e) = tokio::fs::create_dir_all(&child_path).await {
                    error!("Failed to create dir {}: {e}", child_path.display());
                    continue;
                }
                if let Err(e) = Box::pin(self.sync_folder_recursive(&child.uuid, &child.name, &child_path, sync_folder)).await {
                    error!("Failed to sync subfolder {}: {e}", child.name);
                    // Note: files_failed already incremented inside the recursive call
                }
            } else {
                let file_path = local_path.join(child.name.trim());
                if let Err(e) = self.download_file_if_needed(child, &file_path, sync_folder).await {
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
    async fn download_file_if_needed(&self, file: &CloudFile, dest: &Path, sync_folder: &Path) -> Result<()> {
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

        // Extract company / project / location from the path relative to sync_folder.
        // Path structure: sync_folder / Company / Project / Location / file.jpg
        // dest.parent() gives the folder that contains the file.
        let path_parts: Vec<String> = dest.parent()
            .and_then(|p| p.strip_prefix(sync_folder).ok())
            .map(|rel| rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect())
            .unwrap_or_default();
        let company  = path_parts.first().map(String::as_str);
        let project  = path_parts.get(1).map(String::as_str);
        let location = path_parts.get(2).map(String::as_str);

        // Notify only after a real download from a selected folder.
        crate::ui::common::show_download_notification(
            &file.name, file.mime.as_deref(), company, project, location,
        );

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
