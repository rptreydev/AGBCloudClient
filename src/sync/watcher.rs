use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Represents a local file system change
#[derive(Debug, Clone)]
pub enum FileChangeEvent {
    Created(String),
    Modified(String),
    Deleted(String),
    Renamed { from: String, to: String },
}

/// Watches local file system for changes
pub struct FileWatcher {
    watcher: RecommendedWatcher,
}

impl FileWatcher {
    pub fn new(tx: mpsc::Sender<FileChangeEvent>) -> Result<Self> {
        let watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            match res {
                Ok(event) => {
                    let paths: Vec<String> = event
                        .paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect();

                    let change = match event.kind {
                        EventKind::Create(_) => paths.first().map(|p| FileChangeEvent::Created(p.clone())),
                        EventKind::Modify(_) => paths.first().map(|p| FileChangeEvent::Modified(p.clone())),
                        EventKind::Remove(_) => paths.first().map(|p| FileChangeEvent::Deleted(p.clone())),
                        _ => None,
                    };

                    if let Some(evt) = change {
                        debug!("File event: {:?}", evt);
                        let _ = tx.blocking_send(evt);
                    }
                }
                Err(e) => warn!("Watch error: {e}"),
            }
        })?;

        Ok(Self { watcher })
    }

    pub fn watch(&mut self, path: &str) -> Result<()> {
        self.watcher.watch(std::path::Path::new(path), RecursiveMode::Recursive)?;
        info!("Watching directory: {path}");
        Ok(())
    }

    pub fn unwatch(&mut self, path: &str) -> Result<()> {
        self.watcher.unwatch(std::path::Path::new(path))?;
        info!("Stopped watching: {path}");
        Ok(())
    }
}
