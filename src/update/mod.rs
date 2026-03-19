use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ── Types ─────────────────────────────────────────────────────────────────────

/// Update information returned by the server version endpoint or WS event.
///
/// Startup check: GET `{server_url}/app-distribution/check-update/agb-cloud-client`
/// WS event: `app.distribution.version_published` payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    #[serde(rename = "downloadUrl")]
    pub download_url: String,
    /// When `true` the app blocks until the update is installed.
    #[serde(default)]
    pub is_mandatory: bool,
}

// ── Client event reporting ────────────────────────────────────────────────────

/// Mirrors the backend `ClientEventType` enum.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClientEventType {
    Installed,
    Updated,
    Uninstalled,
}

/// Report an install / update / uninstall event to the backend.
/// Fire-and-forget — errors are logged but never propagated.
pub async fn register_client_event(
    server_url: &str,
    jwt: &str,
    event: ClientEventType,
    current_version: &str,
    previous_version: Option<&str>,
    http: &reqwest::Client,
) {
    let mut body = serde_json::json!({
        "appName":       "agb-cloud-client",
        "event":         event,
        "machineId":     get_machine_id(),
        "machineName":   get_machine_name(),
        "currentVersion": current_version,
    });
    if let Some(prev) = previous_version {
        body["previousVersion"] = serde_json::Value::String(prev.to_string());
    }

    let url = format!("{}/app-distribution/clients/event", server_url);
    match http
        .post(&url)
        .header("Cookie", format!("jwt={jwt}"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!("Registered client event {:?} for agb-cloud-client {current_version}", event);
        }
        Ok(r) => warn!("Client event registration failed: HTTP {}", r.status()),
        Err(e) => warn!("Client event registration failed: {e}"),
    }
}

/// Windows MACHINE_GUID from `HKLM\SOFTWARE\Microsoft\Cryptography`.
/// Falls back to `COMPUTERNAME` env var if the registry key is unavailable.
pub fn get_machine_id() -> String {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Ok(out) = std::process::Command::new("reg")
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\Cryptography",
                "/v",
                "MachineGuid",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let t = line.trim();
                if t.starts_with("MachineGuid") {
                    if let Some(guid) = t.split_whitespace().last() {
                        return guid.to_string();
                    }
                }
            }
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
}

/// Human-readable machine name (`COMPUTERNAME` / `HOSTNAME`).
pub fn get_machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
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
///
/// `jwt` is sent as a `Cookie: jwt=<token>` header so the backend's
/// JwtAuthGuard can authenticate the request even when the reqwest cookie
/// jar has not been populated yet (e.g. JWT-fallback session restore path).
pub async fn check_for_update(url: &str, jwt: &str, http: &reqwest::Client) -> Option<UpdateInfo> {
    if url.is_empty() {
        return None; // Endpoint not configured yet — skip silently.
    }
    info!("Checking for updates at {url}");
    let mut req = http.get(url).timeout(std::time::Duration::from_secs(10));
    if !jwt.is_empty() {
        req = req.header("Cookie", format!("jwt={jwt}"));
    }
    let resp = match req.send().await {
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
        Ok(mut info) if is_newer_version(&info.version) => {
            // Backend may return a relative path like /app-distribution/versions/5/download.
            // Make it absolute using the configured server URL.
            if info.download_url.starts_with('/') {
                // Strip the /api/v2 suffix from server_url to get the base origin.
                let base = url
                    .split("/app-distribution")
                    .next()
                    .unwrap_or(url)
                    .trim_end_matches("/api/v2");
                info.download_url = format!("{}{}", base, info.download_url);
            }
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

// ── Post-install success flag ─────────────────────────────────────────────────
//
// Written just before exit(0) so the newly-installed process can show a
// "successfully updated" toast on its first startup.

/// Path of the just-updated flag file (`%TEMP%\agb_just_updated.json`).
pub fn just_updated_flag_path() -> std::path::PathBuf {
    std::env::temp_dir().join("agb_just_updated.json")
}

#[derive(Serialize, Deserialize)]
struct JustUpdatedInfo {
    new_version: String,
    previous_version: String,
}

/// Write both old and new version so the new process can report the UPDATE event.
pub fn write_just_updated_flag(new_version: &str, previous_version: &str) {
    let info = JustUpdatedInfo {
        new_version: new_version.to_string(),
        previous_version: previous_version.to_string(),
    };
    if let Ok(json) = serde_json::to_string(&info) {
        let _ = std::fs::write(just_updated_flag_path(), json);
    }
}

/// Read and immediately delete the flag.
/// Returns `(new_version, previous_version)` on the first run after an auto-update.
pub fn take_just_updated_flag() -> Option<(String, String)> {
    let path = just_updated_flag_path();
    let data = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    // Try JSON format first, fall back to legacy plain-text (single version string).
    if let Ok(info) = serde_json::from_str::<JustUpdatedInfo>(&data) {
        Some((info.new_version, info.previous_version))
    } else {
        Some((data.trim().to_string(), String::new()))
    }
}

// ── First-install flag ────────────────────────────────────────────────────────
//
// Written by the wizard on Finish so the tray startup can report a genuine
// INSTALLED event (not just a re-launch). Prevents false registrations when
// the user cancels the wizard and the uninstaller removes everything.

/// Path of the first-install flag file (`%TEMP%\agb_first_install.flag`).
pub fn first_install_flag_path() -> std::path::PathBuf {
    std::env::temp_dir().join("agb_first_install.flag")
}

/// Write the flag. Called from the wizard Finish block.
pub fn write_first_install_flag() {
    let _ = std::fs::write(first_install_flag_path(), env!("CARGO_PKG_VERSION"));
}

/// Read and immediately delete the flag.
/// Returns `true` if this is the first tray startup after a fresh install.
pub fn take_first_install_flag() -> bool {
    let path = first_install_flag_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
        true
    } else {
        false
    }
}

// ── Update toast notifications ────────────────────────────────────────────────

fn toast(summary: &str, body: &str, timeout_ms: u32) {
    use crate::ui::common::NOTIFICATION_APP_ID;
    let summary = summary.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        notify_rust::Notification::new()
            .app_id(NOTIFICATION_APP_ID)
            .summary(&summary)
            .body(&body)
            .timeout(notify_rust::Timeout::Milliseconds(timeout_ms))
            .show()
            .ok();
    });
}

/// Toast: "Downloading new version…"
pub fn notify_downloading(version: &str) {
    toast(
        "⬆ Downloading update",
        &format!("Downloading version {version}…"),
        5000,
    );
}

/// Toast: "Installing new version…"
pub fn notify_installing(version: &str) {
    toast(
        "⚙ Installing update",
        &format!("Installing version {version}. The app will restart automatically."),
        8000,
    );
}

/// Toast: "Successfully updated!" — called by the NEW process on first startup.
pub fn notify_update_success(version: &str) {
    toast(
        "✅ Update installed",
        &format!("Version {version} was installed successfully."),
        8000,
    );
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
    jwt: &str,
    progress: Arc<Mutex<DownloadProgress>>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let dest = std::env::temp_dir().join("AGBCloudClient-update.exe");
    info!("Downloading update {} → {:?}", info.version, dest);

    // ── Notify: downloading ───────────────────────────────────────────────────
    notify_downloading(&info.version);

    let client = reqwest::Client::new();
    let mut resp = client
        .get(&info.download_url)
        .header("Cookie", format!("jwt={jwt}"))
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let preview = &body[..body.len().min(200)];
        anyhow::bail!("Download failed: HTTP {status} — {preview}");
    }

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

    // ── Notify: installing ────────────────────────────────────────────────────
    notify_installing(&info.version);
    info!("Download complete — launching installer silently");

    // Write flag so the new process shows a success toast and reports the UPDATE event.
    write_just_updated_flag(&info.version, env!("CARGO_PKG_VERSION"));

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
