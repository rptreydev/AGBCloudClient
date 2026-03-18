use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ── Types ─────────────────────────────────────────────────────────────────────

/// Update information returned by the server version endpoint.
///
/// Endpoint (to be configured in `AppConfig::update_check_url`):
///   GET <update_check_url>
///   Response: { "version": "0.1.0-beta.2", "downloadUrl": "https://..." }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    #[serde(rename = "downloadUrl")]
    pub download_url: String,
}

// ── Version comparison ─────────────────────────────────────────────────────────

/// Returns `true` if `remote_version` is strictly newer than the running binary.
/// Uses semver comparison so pre-release ordering is correct
/// (e.g. `0.1.0-beta.2 > 0.1.0-beta.1`, `0.2.0 > 0.1.0-beta.1`).
pub fn is_newer_version(remote_version: &str) -> bool {
    let current = env!("CARGO_PKG_VERSION");
    match (
        semver::Version::parse(remote_version.trim()),
        semver::Version::parse(current.trim()),
    ) {
        (Ok(remote), Ok(local)) => remote > local,
        _ => {
            warn!(
                "Could not compare versions for update check: \
                 remote={remote_version} local={current}"
            );
            false
        }
    }
}

// ── Startup version check ─────────────────────────────────────────────────────

/// Poll the server version endpoint on startup.
///
/// Returns `Some(info)` when a newer version is available.
/// Returns `None` when:
///   - `url` is empty (endpoint not yet configured)
///   - Server is unreachable
///   - Version is the same or older
pub async fn check_for_update(url: &str, http: &reqwest::Client) -> Option<UpdateInfo> {
    if url.is_empty() {
        return None; // Endpoint not configured yet — skip silently.
    }
    info!("Checking for updates at {url}");
    let resp = match http
        .get(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            warn!("Update check: HTTP {}", r.status());
            return None;
        }
        Err(e) => {
            info!("Update check skipped (server unreachable): {e}");
            return None;
        }
    };

    match resp.json::<UpdateInfo>().await {
        Ok(info) if is_newer_version(&info.version) => {
            info!(
                "Update available: {} (current: {})",
                info.version,
                env!("CARGO_PKG_VERSION")
            );
            Some(info)
        }
        Ok(info) => {
            info!(
                "App is up to date: {} (remote: {})",
                env!("CARGO_PKG_VERSION"),
                info.version
            );
            None
        }
        Err(e) => {
            warn!("Could not parse version response: {e}");
            None
        }
    }
}

// ── Persistent update flag ────────────────────────────────────────────────────
//
// Written when a WS `app.update_available` event is received or when the
// startup check finds a newer version.  Read on every launch so the tray
// remembers that an update is required even after a restart.

/// Path of the update-pending flag file (`%TEMP%\agb_update_required.json`).
pub fn update_flag_path() -> std::path::PathBuf {
    std::env::temp_dir().join("agb_update_required.json")
}

/// Persist update info so the tray remembers across restarts.
pub fn write_update_flag(info: &UpdateInfo) {
    if let Ok(json) = serde_json::to_string(info) {
        let _ = std::fs::write(update_flag_path(), json);
    }
}

/// Read persisted update info (written by startup check or WS event).
pub fn read_update_flag() -> Option<UpdateInfo> {
    let data = std::fs::read_to_string(update_flag_path()).ok()?;
    serde_json::from_str(&data).ok()
}

/// Clear the flag after a successful installation or on clean startup with no update.
pub fn clear_update_flag() {
    let _ = std::fs::remove_file(update_flag_path());
}

// ── Download state ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum DownloadState {
    Idle,
    Downloading,
    Installing,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub state: DownloadState,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}

impl Default for DownloadProgress {
    fn default() -> Self {
        Self {
            state: DownloadState::Idle,
            downloaded_bytes: 0,
            total_bytes: 0,
        }
    }
}

// ── Installer download + launch ───────────────────────────────────────────────

/// Download the installer to `%TEMP%\AGBCloudClient-update.exe` and run it
/// silently with `/S` (NSIS silent-install flag).
///
/// Updates `progress` while downloading.
/// On success this function does NOT return — it calls `std::process::exit(0)`
/// after launching the installer so the new version can acquire file locks.
pub async fn download_and_install(
    info: &UpdateInfo,
    progress: Arc<Mutex<DownloadProgress>>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let dest = std::env::temp_dir().join("AGBCloudClient-update.exe");
    info!("Downloading update {} → {:?}", info.version, dest);

    let client = reqwest::Client::new();
    let mut resp = client
        .get(&info.download_url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await?;

    let total = resp.content_length().unwrap_or(0);
    {
        let mut p = progress.lock().unwrap();
        p.total_bytes = total;
        p.state = DownloadState::Downloading;
    }

    let mut file = tokio::fs::File::create(&dest).await?;
    let mut downloaded = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        let mut p = progress.lock().unwrap();
        p.downloaded_bytes = downloaded;
    }
    file.flush().await?;
    drop(file);

    {
        let mut p = progress.lock().unwrap();
        p.state = DownloadState::Installing;
    }

    info!("Download complete — launching installer silently");

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new(&dest)
            .arg("/S")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to launch installer: {e}"))?;
    }
    #[cfg(not(target_os = "windows"))]
    std::process::Command::new(&dest)
        .arg("/S")
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to launch installer: {e}"))?;

    // Short delay so the installer can acquire file locks before we exit.
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    clear_update_flag();
    std::process::exit(0);
}
