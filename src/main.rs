#![windows_subsystem = "windows"]

mod auth;
mod config;
mod models;
mod sync;
mod tray;
mod ui;
mod ws;

use std::env;
use single_instance::SingleInstance;
use tracing::{error, info, warn};
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
    // Spawned by the setup wizard on completion.  Skips SingleInstance because the
    // wizard process (which holds the mutex) sleeps 800ms then exits — without this
    // flag the fresh tray would detect "another instance running" and exit immediately,
    // causing the tray icon to never appear after first-time setup.
    let from_wizard = args.contains(&"--from-wizard".to_string());

    // Single-instance check (skip during special UI modes — they run as subprocesses)
    let _instance = if !force_setup && !manage_folders_mode && !settings_mode && !status_mode && !login_mode && !uninstall_mode && !from_wizard {
        let inst = SingleInstance::new("agb-cloud-client-agbroadband")
            .expect("Failed to create instance lock");
        if !inst.is_single() {
            error!("Another instance is already running");
            std::process::exit(1);
        }
        Some(inst)
    } else {
        None
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

    info!(
        "Starting sync with {} selected folder(s)",
        config.selected_folders.len()
    );

    // Shared trigger: allows the tray (polling progress.json) to wake the engine
    // early when the user saves new folder selections in Manage Folders.
    let sync_trigger = std::sync::Arc::new(tokio::sync::Notify::new());

    // Spawn background sync engine
    {
        let sync_auth = auth_state.clone();
        let sync_progress = progress.clone();
        let engine_trigger = sync_trigger.clone();
        rt.spawn(async move {
            let engine = sync::SyncEngine::new(sync_auth, sync_progress);
            engine.run(engine_trigger).await;
        });
    }

    // Spawn WebSocket listener for real-time events
    let ws_config = config.clone();
    let ws_auth = auth_state.clone();
    rt.spawn(async move {
        if let Err(e) = ws::WsClient::connect(&ws_config, &ws_auth).await {
            warn!("WebSocket connection failed: {e}");
        }
    });

    // Run system tray on the main thread (blocks with Windows message pump)
    info!("Starting system tray...");
    tray::run_tray(&auth_state, &config, &rt, progress, sync_trigger);
    info!("Tray exited — shutting down");
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
