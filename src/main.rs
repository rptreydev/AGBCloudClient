// In release builds hide the console window (Windows GUI app).
// In debug builds keep the console so `cargo run` shows live logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod config;
mod models;
mod sync;
mod tray;
mod ui;
mod update;
mod ws;

use std::env;
use std::sync::mpsc;
use std::sync::atomic::Ordering;
use single_instance::SingleInstance;
use tracing::{error, info, warn};
use update::{check_for_update, read_update_flag, write_update_flag};
use ws::events::TreePatchAction;
use tracing_subscriber::{fmt, EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

fn main() {
    // Initialize structured logging to file (%APPDATA%/AGBroadband/AGBCloudClient/logs/)
    let log_dir = directories::ProjectDirs::from("com", "AGBroadband", "AGBCloudClient")
        .map(|dirs| dirs.data_dir().join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "agb-cloud-client.log");

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("agb_cloud_client=info"));

    // In debug builds also print to stdout so `cargo run` shows live logs.
    #[cfg(debug_assertions)]
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(file_appender))
        .with(fmt::layer().with_writer(std::io::stdout))
        .init();

    #[cfg(not(debug_assertions))]
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(file_appender))
        .init();

    // Set panic hook to log panics to the same log file
    std::panic::set_hook(Box::new(|panic_info| {
        tracing::error!("PANIC: {panic_info}");
    }));

    info!("AGB Cloud Client v{} starting", env!("CARGO_PKG_VERSION"));

    // Register Windows AppUserModelID so toast notifications show our icon
    // (not the PowerShell / generic icon).  Safe to call on every launch.
    ui::common::register_notification_app_id();
    // Clear stale shutdown flag from a previous crash so subprocesses don't
    // exit immediately on the next launch.
    ui::common::clear_shutdown_flag();

    // Parse CLI arguments
    let args: Vec<String> = env::args().collect();
    let custom_server = args
        .iter()
        .position(|a| a == "--server")
        .and_then(|i| args.get(i + 1).cloned());
    let force_setup = args.contains(&"--setup".to_string());
    let manage_folders_mode = args.contains(&"--manage-folders".to_string());
    let settings_mode = args.contains(&"--settings".to_string());
    // Opens the iCloud-style live status panel (spawned from tray).
    let status_mode = args.contains(&"--status".to_string());
    // Spawned by the tray after logout — shows login without requiring setup wizard.
    // Also bypasses the SingleInstance check to avoid a race condition where the
    // new process starts before the old tray process has released the mutex lock.
    let login_mode = args.contains(&"--login".to_string());
    // Called by the NSIS uninstaller before removing files — cleans up shortcuts.
    let uninstall_mode = args.contains(&"--uninstall".to_string());
    // Spawned by the tray when an update is available. Shows a mandatory update
    // window with a download progress bar. The window cannot be closed without
    // installing the update. Args: --update --version <ver> --url <download_url>
    let update_mode = args.contains(&"--update".to_string());
    // Spawned by the setup wizard on completion.  Skips SingleInstance because the
    // wizard process (which holds the mutex) sleeps 800ms then exits — without this
    // flag the fresh tray would detect "another instance running" and exit immediately,
    // causing the tray icon to never appear after first-time setup.
    let from_wizard = args.contains(&"--from-wizard".to_string());
    // Passed by the wizard when it registered the Explorer sidebar, so that
    // --from-wizard restarts Explorer AFTER killing the old tray.  This defers
    // the blocking restart_explorer() call out of eframe's UI thread.
    let restart_explorer_after_setup = args.contains(&"--restart-explorer".to_string());

    // Single-instance check
    //
    // Normal launch  → acquire mutex immediately; exit if another tray is running.
    // --from-wizard  → kill any other running instance first (process enumeration,
    //                  not mutex — mutex may be abandoned after abnormal exit),
    //                  then acquire the mutex.
    // Subprocesses   → no lock needed (short-lived UI helpers).
    let _instance = if !force_setup && !manage_folders_mode && !settings_mode
        && !status_mode && !login_mode && !uninstall_mode && !from_wizard && !update_mode
    {
        let inst = SingleInstance::new("agb-cloud-client-agbroadband")
            .expect("Failed to create instance lock");
        if !inst.is_single() {
            error!("Another instance is already running");
            std::process::exit(1);
        }
        Some(inst)
    } else if from_wizard {
        // ── Kill every other running instance FIRST (by process name, not mutex).
        // The mutex may be abandoned (released by OS) even when the process is
        // still alive, so we use process enumeration to detect and kill reliably.
        // Phase 1: graceful quit (tray calls NIM_DELETE → clean icon removal).
        // Phase 2: taskkill /F fallback + 2 s for Windows to sweep ghost icons.
        // We do this BEFORE acquiring our own mutex so that restart_explorer()
        // (called below) always starts with a clean notification area.
        //
        // All child commands use CREATE_NO_WINDOW (0x0800_0000) to avoid flashing
        // a console window, since this binary runs as a Windows-subsystem app.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;

            let cur_pid = std::process::id();
            if let Ok(exe) = std::env::current_exe() {
                let exe_name = exe.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("agb-cloud-client.exe")
                    .to_owned();

                // Phase 1: graceful quit flag — old tray calls drop(tray_icon) then exits.
                let quit_flag = std::env::temp_dir().join("agb_tray_quit.flag");
                let _ = std::fs::write(&quit_flag, b"quit");
                info!("--from-wizard: sent graceful-quit signal, waiting up to 2 s");

                let mut others_alive = false;
                for _ in 0..10u32 {           // 10 × 200 ms = 2 s
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let out = std::process::Command::new("tasklist")
                        .args(["/FI", &format!("IMAGENAME eq {}", exe_name),
                               "/FI", &format!("PID ne {}", cur_pid), "/NH"])
                        .creation_flags(CREATE_NO_WINDOW)
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
                        .unwrap_or_default();
                    if out.contains(&exe_name.to_lowercase()) {
                        others_alive = true;   // still alive — keep waiting
                    } else {
                        info!("--from-wizard: all other instances exited gracefully");
                        others_alive = false;
                        break;
                    }
                }
                let _ = std::fs::remove_file(&quit_flag);

                // Phase 2: force-kill if graceful quit timed out.
                if others_alive {
                    warn!("--from-wizard: graceful quit timed out — force-killing");
                    let _ = std::process::Command::new("taskkill")
                        .args(["/F", "/IM", &exe_name,
                               "/FI", &format!("PID ne {}", cur_pid)])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .creation_flags(CREATE_NO_WINDOW)
                        .status();
                    // Give Windows 2 s to sweep the ghost notification-area entry.
                    std::thread::sleep(std::time::Duration::from_millis(2000));
                }
            }
        }

        // ── Clear Windows notification-area icon cache BEFORE restarting Explorer.
        // Windows stores cached tray icon states in the registry under TrayNotify.
        // When Explorer restarts it restores these cached entries — including ghost
        // icons from processes that no longer exist — which appear as duplicate icons
        // alongside the new tray icon.  Deleting the cache forces Explorer to start
        // with a clean notification area.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;

            let key = r"HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\TrayNotify";
            for value in &["IconStreams", "PastIconsStream"] {
                let _ = std::process::Command::new("reg")
                    .args(["delete", key, "/v", value, "/f"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW)
                    .status();
            }
            info!("--from-wizard: notification-area icon cache cleared");
        }

        // ── Restart Explorer now (AFTER old tray is dead + cache cleared) if the
        // wizard registered the sidebar.  No old tray is alive to respond to
        // WM_TASKBARCREATED, and the cache is clean, so only our new process will
        // register an icon.
        if restart_explorer_after_setup {
            info!("--from-wizard: restarting Explorer to apply sidebar registration");
            if let Err(e) = ui::common::restart_explorer() {
                error!("Explorer restart failed: {e}");
            }
        }

        // ── Acquire the SingleInstance mutex (old tray is dead, should succeed).
        let inst = SingleInstance::new("agb-cloud-client-agbroadband")
            .expect("Failed to create instance lock");
        if !inst.is_single() {
            warn!("--from-wizard: could not acquire mutex — another tray still running. Exiting.");
            std::process::exit(0);
        }
        info!("--from-wizard: SingleInstance mutex acquired");
        Some(inst)
    } else {
        None // Subprocess modes (manage-folders, settings, status, login) — no lock
    };

    // Load or create config
    let mut config = config::AppConfig::load_or_create().unwrap_or_else(|e| {
        warn!("Failed to load config: {e}, using defaults");
        config::AppConfig::default()
    });

    if let Some(server) = custom_server {
        info!("Using custom server: {server}");
        config.server_url = server;
    }

    // Handle --uninstall (called by NSIS before removing files).
    // Removes shortcuts, Explorer nav entry, auto-start, and all saved credentials.
    // The NSIS uninstaller then deletes %APPDATA%/%LOCALAPPDATA% data directories.
    if uninstall_mode {
        info!("Uninstall cleanup — removing shortcuts and credentials");

        // Only restart Explorer if the sidebar CLSID was actually registered.
        // If the user cancelled setup before the wizard completed, the sidebar
        // was never added and there's no reason to restart Explorer.
        let clsid = r"HKCU\Software\Classes\CLSID\{2CC5E37B-3737-4C89-A1E7-23A99F4C0E00}";
        let sidebar_was_registered = std::process::Command::new("reg")
            .args(["query", clsid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        ui::common::remove_explorer_sidebar(&config.sync_folder).ok();
        ui::common::remove_desktop_shortcut().ok();
        ui::common::set_auto_start(false).ok();

        // Delete all credentials from Windows Credential Manager so no tokens remain
        // after uninstall.  clear_all() deletes JWT, refresh token, password, and
        // the last-username entry.  Errors are non-fatal — NSIS removes the files
        // regardless.
        match auth::store::CredentialStore::clear_all() {
            Ok(()) => info!("Credentials cleared from Windows Credential Manager"),
            Err(e) => warn!("Could not clear credentials (non-fatal): {e}"),
        }

        if sidebar_was_registered {
            info!("Explorer sidebar was registered — restarting Explorer");
            ui::common::restart_explorer().ok();
        } else {
            info!("Explorer sidebar was not registered — skipping Explorer restart");
        }

        info!("Uninstall cleanup complete");
        return;
    }

    // Create auth state (shared HTTP client with cookie jar)
    let auth_state = auth::AuthState::new(config.clone());

    // Build tokio runtime for async operations
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // Handle --logout flag
    if args.contains(&"--logout".to_string()) {
        rt.block_on(auth_state.logout()).ok();
        info!("Logged out. Exiting.");
        return;
    }

    // Handle --update (spawned by tray when a new version is available).
    // Shows a mandatory update window — the user cannot proceed without installing.
    // Args: --update --version <ver> --url <download_url>
    if update_mode {
        let version = args.iter()
            .position(|a| a == "--version")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_default();
        let url = args.iter()
            .position(|a| a == "--url")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_default();

        if version.is_empty() || url.is_empty() {
            // Fallback: read from flag file (written by previous WS event).
            if let Some(info) = read_update_flag() {
                info!("Update mode — showing window for v{}", info.version);
                ui::show_update_window(info, &rt);
            } else {
                warn!("--update: no version/url provided and no flag file found");
            }
        } else {
            let info = update::UpdateInfo { version, download_url: url, is_mandatory: true };
            info!("Update mode — showing window for v{}", info.version);
            ui::show_update_window(info, &rt);
        }
        std::process::exit(0);
    }

    // Handle --login (spawned by tray after logout)
    // Shows the login window; on success spawns a fresh tray process INSIDE
    // eframe before GPU teardown, then exits via exit(0).
    if login_mode {
        info!("Post-logout login mode — showing login window");
        ui::show_login_window(&auth_state, &rt, Some(vec![]));
        // Always exit — fresh tray spawned inside eframe on success (or cancelled).
        std::process::exit(0);
    }

    // Handle --manage-folders (subprocess launched from tray)
    if manage_folders_mode {
        info!("Manage folders subprocess started");
        // Single-instance guard: if already open, bring existing window to front
        let _mf_lock = SingleInstance::new("agb-cloud-manage-folders-v1")
            .expect("Failed to create manage-folders lock");
        if !_mf_lock.is_single() {
            info!("Manage Folders already open — activating existing window");
            ui::common::activate_window_by_title("AGB Cloud Client - Select Folders");
            std::process::exit(0);
        }
        let restored = rt.block_on(auth_state.try_restore_session());
        if !restored {
            info!("Session restore failed — showing login");
            notify_rust::Notification::new()
                .app_id(ui::common::NOTIFICATION_APP_ID)
                .summary("AGB Cloud Client — Sign in Required")
                .body("Please sign in to manage your Cloud Files folders.")
                .timeout(notify_rust::Timeout::Milliseconds(4000))
                .show()
                .ok();
            // On success: spawn a fresh --manage-folders process (credentials now stored).
            // On cancel: exit. Either way, we exit — can't call show_file_browser after
            // eframe::run_native (winit EventLoop is single-use per process).
            ui::show_login_window(&auth_state, &rt, Some(vec!["--manage-folders".to_string()]));
            std::process::exit(0);
        }
        ui::show_file_browser(&auth_state, &mut config, &rt);
        std::process::exit(0);
    }

    // Handle --settings (subprocess launched from tray)
    if settings_mode {
        info!("Settings subprocess started");
        // Single-instance guard: if already open, bring existing window to front
        let _st_lock = SingleInstance::new("agb-cloud-settings-v1")
            .expect("Failed to create settings lock");
        if !_st_lock.is_single() {
            info!("Settings already open — activating existing window");
            ui::common::activate_window_by_title("AGB Cloud Client - Settings");
            std::process::exit(0);
        }
        let restored = rt.block_on(auth_state.try_restore_session());
        if !restored {
            info!("Session restore failed — showing login");
            notify_rust::Notification::new()
                .app_id(ui::common::NOTIFICATION_APP_ID)
                .summary("AGB Cloud Client — Sign in Required")
                .body("Please sign in to open Settings.")
                .timeout(notify_rust::Timeout::Milliseconds(4000))
                .show()
                .ok();
            // On success: spawn a fresh --settings process (credentials now stored).
            ui::show_login_window(&auth_state, &rt, Some(vec!["--settings".to_string()]));
            std::process::exit(0);
        }
        let progress = std::sync::Arc::new(std::sync::Mutex::new(
            sync::SyncProgress::default(),
        ));
        ui::show_settings_window(&mut config, &auth_state, &rt, &progress);
        std::process::exit(0);
    }

    // Handle --status (status panel subprocess launched from tray)
    if status_mode {
        info!("Status panel subprocess started");
        // Single-instance guard: if already open, bring existing window to front
        let _sp_lock = SingleInstance::new("agb-cloud-status-panel-v1")
            .expect("Failed to create status-panel lock");
        if !_sp_lock.is_single() {
            info!("Status panel already open — activating existing window");
            ui::common::activate_window_by_title("AGB Cloud Client \u{2014} Status");
            std::process::exit(0);
        }
        let restored = rt.block_on(auth_state.try_restore_session());
        if !restored {
            info!("Session restore failed — showing login");
            notify_rust::Notification::new()
                .app_id(ui::common::NOTIFICATION_APP_ID)
                .summary("AGB Cloud Client — Sign in Required")
                .body("Please sign in to view sync status.")
                .timeout(notify_rust::Timeout::Milliseconds(4000))
                .show()
                .ok();
            // On success: spawn a fresh --status process (credentials now stored).
            ui::show_login_window(&auth_state, &rt, Some(vec!["--status".to_string()]));
            std::process::exit(0);
        }
        ui::show_status_panel(&auth_state, &rt);
        // show_status_panel always calls exit(0), so this line is unreachable.
        std::process::exit(0);
    }

    // Determine if we need the setup wizard
    let needs_wizard = force_setup || !config.setup_complete;

    let progress = if needs_wizard {
        // Show unified setup wizard. This function never returns —
        // on completion it relaunches the app in tray mode and calls exit(0).
        // On cancel it also calls exit(0).
        info!("Showing setup wizard (force={force_setup}, setup_complete={})", config.setup_complete);
        ui::show_setup_wizard(&auth_state, &mut config, &rt);
        // Should never reach here, but just in case:
        info!("Wizard returned unexpectedly");
        return;
    } else {
        // Try to restore previous session from stored credentials
        let session_restored = rt.block_on(auth_state.try_restore_session());

        if session_restored {
            info!("Session restored successfully");
        } else {
            // No valid session — show the login window.
            //
            // We use spawn_args=Some(vec![]) so that on successful login, a fresh
            // tray process is spawned INSIDE eframe's update() loop (before GPU
            // teardown).  After show_login_window returns we always exit(0):
            //   • Login succeeded → fresh tray already running, safe to exit.
            //   • User cancelled  → exit cleanly.
            //
            // Rationale: calling eframe::run_native and then continuing in the same
            // process is unsafe on Windows — the GPU destructor can crash the
            // process before the tray ever starts (GPU teardown crash pattern).
            info!("No valid session — showing login window (will spawn fresh tray on success)");
            ui::show_login_window(&auth_state, &rt, Some(vec![]));
            // Always exit — fresh tray spawned inside eframe on success.
            std::process::exit(0);
        }

        std::sync::Arc::new(std::sync::Mutex::new(
            sync::SyncProgress::default(),
        ))
    };

    // ── Startup update check ──────────────────────────────────────────────────
    // 1. If a previous WS event left a flag file, an update is still pending.
    // 2. Otherwise, poll the version endpoint (derived from server_url when
    //    update_check_url is empty, or use the configured URL).
    // If an update is found → spawn the mandatory update window + block the
    // tray in update-mode (sync disabled, menu limited).
    {
        let check_url = if config.update_check_url.is_empty() {
            format!(
                "{}/app-distribution/check-update/AGBCloudClient",
                config.server_url
            )
        } else {
            config.update_check_url.clone()
        };
        let pending = read_update_flag().or_else(|| {
            let http = auth_state.client();
            rt.block_on(check_for_update(&check_url, http))
        });
        if let Some(info) = pending {
            write_update_flag(&info);
            info!("Update required on startup: v{}", info.version);
            spawn_update_subprocess(&info);
            // Run tray in update mode (limited menu, no sync engine started).
            tray::run_tray_update_mode(&auth_state, &config, &rt, &info);
            std::process::exit(0);
        }
    }

    // ── Post-update success notification + event registration ────────────────
    // If the previous process wrote a just_updated flag before exiting, show
    // a "Successfully installed" toast and report the UPDATE event to the backend.
    if let Some((new_version, prev_version)) = update::take_just_updated_flag() {
        info!("First run after update to v{new_version} — showing success notification");
        update::notify_update_success(&new_version);
        let http = auth_state.client().clone();
        let server_url = config.server_url.clone();
        let prev = if prev_version.is_empty() { None } else { Some(prev_version) };
        rt.spawn(async move {
            update::register_client_event(
                &server_url,
                update::ClientEventType::Updated,
                &new_version,
                prev.as_deref(),
                &http,
            )
            .await;
        });
    }

    // ── Register this installation on startup ─────────────────────────────────
    // Fire-and-forget: keeps AppClientStatus up-to-date on the server
    // (current version, machine name, last-seen timestamp).
    {
        let http = auth_state.client().clone();
        let server_url = config.server_url.clone();
        rt.spawn(async move {
            update::register_client_event(
                &server_url,
                update::ClientEventType::Installed,
                env!("CARGO_PKG_VERSION"),
                None,
                &http,
            )
            .await;
        });
    }

    info!(
        "Starting sync with {} selected folder(s)",
        config.selected_folders.len()
    );

    // Shared trigger: allows the tray (polling progress.json) to wake the engine
    // early when the user saves new folder selections in Manage Folders.
    let sync_trigger = std::sync::Arc::new(tokio::sync::Notify::new());

    // Spawn background sync engine — keep abort_flag handle to cancel on company removal
    let abort_flag = {
        let sync_auth = auth_state.clone();
        let sync_progress = progress.clone();
        let engine_trigger = sync_trigger.clone();
        let engine = sync::SyncEngine::new(sync_auth, sync_progress);
        let flag = engine.abort_flag();
        rt.spawn(async move { engine.run(engine_trigger).await; });
        flag
    };

    // Spawn WebSocket listener — wakes the sync engine on CLOUD_FILE events.
    // Also listens for company_supervisor.changed (tree patches) and
    // app.update_available (new installer published by the backend).
    let (patch_tx, patch_rx) = mpsc::channel::<ws::events::TreePatch>();
    let (update_ws_tx, update_ws_rx) = mpsc::channel::<update::UpdateInfo>();
    {
        let ws_config  = config.clone();
        let ws_auth    = auth_state.clone();
        let ws_trigger = sync_trigger.clone();
        rt.spawn(async move {
            let my_username = ws_auth.current_username().await.unwrap_or_default();
            ws::WsClient::run(
                ws_config, ws_auth, ws_trigger,
                Some((my_username, patch_tx)),
                Some(update_ws_tx),
            ).await;
        });
    }

    // Bridge: handle app.update_available events from the WS listener.
    // When the backend publishes a new installer, this thread receives the info,
    // persists a flag file, and spawns the mandatory update window subprocess.
    {
        std::thread::spawn(move || {
            while let Ok(info) = update_ws_rx.recv() {
                info!("WS update event received: v{}", info.version);
                if update::is_newer_version(&info.version) {
                    write_update_flag(&info);
                    spawn_update_subprocess(&info);
                }
            }
        });
    }

    // Bridge: handle company-assignment patches in the tray process.
    //   Removed → abort current sync cycle + remove company folders from config
    //   Added   → trigger a new sync cycle so newly accessible folders are downloaded
    {
        let abort_for_patch  = abort_flag.clone();
        let trigger_for_patch = sync_trigger.clone();
        std::thread::spawn(move || {
            while let Ok(patch) = patch_rx.recv() {
                match patch.action {
                    TreePatchAction::Removed => {
                        warn!("Company '{}' removed — aborting sync and cleaning config",
                            patch.company_name);
                        abort_for_patch.store(true, Ordering::Relaxed);
                        remove_company_from_config(&patch.company_name);
                        // Trigger a new cycle: the engine will reload config (without
                        // the removed company) and reset the abort flag.
                        trigger_for_patch.notify_one();
                        // Toast notification — already on a background thread, safe to call.
                        let name = patch.company_name.clone();
                        std::thread::spawn(move || {
                            ui::common::show_company_removed_notification(&name);
                        });
                    }
                    TreePatchAction::Added => {
                        info!("Company '{}' added — triggering sync", patch.company_name);
                        trigger_for_patch.notify_one();
                        // Toast notification — spawn sub-thread to avoid blocking the bridge.
                        let name = patch.company_name.clone();
                        std::thread::spawn(move || {
                            ui::common::show_company_assigned_notification(&name);
                        });
                    }
                }
            }
        });
    }

    // Run system tray on the main thread (blocks with Windows message pump)
    info!("Starting system tray...");
    tray::run_tray(&auth_state, &config, &rt, progress, sync_trigger);
    info!("Tray exited — shutting down");
}

/// Spawn the update window as a subprocess (`--update --version <v> --url <u>`).
/// Public so `tray::run_tray_update_mode` can call it on left-click / menu.
pub fn main_spawn_update_subprocess(info: &update::UpdateInfo) {
    spawn_update_subprocess(info);
}

/// Internal impl — see `main_spawn_update_subprocess` for the public alias.
/// Called when a new version is detected on startup or via WS event.
fn spawn_update_subprocess(info: &update::UpdateInfo) {
    match std::env::current_exe() {
        Ok(exe) => {
            #[cfg(target_os = "windows")]
            let cmd = {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                std::process::Command::new(&exe)
                    .args(["--update", "--version", &info.version, "--url", &info.download_url])
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
            };
            #[cfg(not(target_os = "windows"))]
            let cmd = std::process::Command::new(&exe)
                .args(["--update", "--version", &info.version, "--url", &info.download_url])
                .spawn();

            match cmd {
                Ok(c) => info!("Update window spawned (pid={})", c.id()),
                Err(e) => error!("Failed to spawn update window: {e}"),
            }
        }
        Err(e) => error!("Cannot determine exe path for update spawn: {e}"),
    }
}

/// Remove all selected folder entries that belong to a company from the persisted config.
/// Called when a `company_supervisor.changed { action: "removed" }` event is received so
/// the sync engine won't attempt to sync inaccessible folders on the next cycle.
///
/// Matches by company name prefix in `FolderSelection.path` (e.g. "Acme Corp / Projects")
/// since the CloudFile UUID in the selection differs from the Company entity UUID in the event.
fn remove_company_from_config(company_name: &str) {
    match config::AppConfig::load_or_create() {
        Ok(mut cfg) => {
            let before = cfg.selected_folders.len();
            cfg.selected_folders.retain(|sel| {
                let top = sel.path.split(" / ").next().unwrap_or("").trim();
                top != company_name && sel.name != company_name
            });
            let removed = before - cfg.selected_folders.len();
            if removed > 0 {
                info!("Removed {removed} folder selection(s) for company '{company_name}'");
                if let Err(e) = cfg.save() {
                    error!("Failed to save config after company removal: {e}");
                }
            }
        }
        Err(e) => error!("Could not load config to remove company folders: {e}"),
    }
}

/// Show a Windows MessageBox so errors are visible even without a console.
/// Reserved for fatal startup errors where the log file may not be accessible.
#[allow(dead_code)]
#[cfg(target_os = "windows")]
fn show_error_box(title: &str, message: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }
    let t = to_wide(title);
    let m = to_wide(message);
    unsafe {
        winapi::um::winuser::MessageBoxW(
            std::ptr::null_mut(),
            m.as_ptr(),
            t.as_ptr(),
            winapi::um::winuser::MB_OK | winapi::um::winuser::MB_ICONERROR,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn show_error_box(_title: &str, _message: &str) {}
