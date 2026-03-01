pub mod menu;

use tracing::{error, info};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::sync::SharedProgress;

/// Run the system tray icon with context menu.
/// This function blocks the calling thread (main thread) forever,
/// pumping Windows messages so the tray menu renders correctly.
pub fn run_tray(
    auth: &AuthState,
    config: &AppConfig,
    rt: &tokio::runtime::Runtime,
    progress: SharedProgress,
) {
    let icon = crate::ui::icon::tray_icon();

    // Build context menu
    let menu = Menu::new();
    let open_folder = MenuItem::new("Open Sync Folder", true, None);
    let manage_folders = MenuItem::new("Manage Folders", true, None);
    let settings = MenuItem::new("Settings", true, None);
    let logout = MenuItem::new("Logout", true, None);
    let quit = MenuItem::new("Quit", true, None);

    menu.append(&open_folder).ok();
    menu.append(&manage_folders).ok();
    menu.append(&settings).ok();
    menu.append(&logout).ok();
    menu.append(&quit).ok();

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("AGB Cloud Client — AGBroadband")
        .with_icon(icon)
        .build()
        .expect("Failed to create tray icon");

    info!("System tray running");

    let menu_channel = MenuEvent::receiver();
    let open_id = open_folder.id().clone();
    let manage_id = manage_folders.id().clone();
    let settings_id = settings.id().clone();
    let logout_id = logout.id().clone();
    let quit_id = quit.id().clone();

    let auth_clone = auth.clone();
    let config_ptr = config as *const AppConfig;
    let tray_channel = TrayIconEvent::receiver();

    // Tooltip update tracking
    let mut last_tooltip = String::new();
    let mut tick_count: u32 = 0;

    // Platform-specific message pump
    #[cfg(target_os = "windows")]
    {
        use std::mem::zeroed;
        use winapi::um::winuser::{DispatchMessageW, PeekMessageW, TranslateMessage, PM_REMOVE, MSG};

        loop {
            // Pump Windows messages — use PeekMessage for non-blocking so we can update tooltip
            unsafe {
                let mut msg: MSG = zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            // Handle left-click on tray icon → open sync folder
            if let Ok(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) = tray_channel.try_recv()
            {
                info!("Tray icon clicked — opening sync folder");
                let config_ref = unsafe { &*config_ptr };
                if let Err(e) = open::that(&config_ref.sync_folder) {
                    error!("Failed to open folder: {e}");
                }
            }

            // Check for menu events (non-blocking)
            if let Ok(event) = menu_channel.try_recv() {
                let config_ref = unsafe { &*config_ptr };
                handle_menu_event(
                    &event.id,
                    &open_id,
                    &manage_id,
                    &settings_id,
                    &logout_id,
                    &quit_id,
                    &auth_clone,
                    config_ref,
                    rt,
                );
            }

            // Update tooltip with sync progress every ~100 ticks (~1s)
            tick_count = tick_count.wrapping_add(1);
            if tick_count % 100 == 0 {
                if let Ok(p) = progress.lock() {
                    let tooltip = p.tooltip();
                    if tooltip != last_tooltip {
                        tray_icon.set_tooltip(Some(&tooltip)).ok();
                        last_tooltip = tooltip;
                    }
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        loop {
            if let Ok(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) = tray_channel.try_recv()
            {
                info!("Tray icon clicked — opening sync folder");
                let config_ref = unsafe { &*config_ptr };
                let _ = open::that(&config_ref.sync_folder);
            }

            if let Ok(event) = menu_channel.try_recv() {
                let config_ref = unsafe { &*config_ptr };
                handle_menu_event(
                    &event.id,
                    &open_id,
                    &manage_id,
                    &settings_id,
                    &logout_id,
                    &quit_id,
                    &auth_clone,
                    config_ref,
                    rt,
                );
            }

            // Update tooltip
            tick_count = tick_count.wrapping_add(1);
            if tick_count % 5 == 0 {
                if let Ok(p) = progress.lock() {
                    let tooltip = p.tooltip();
                    if tooltip != last_tooltip {
                        tray_icon.set_tooltip(Some(&tooltip)).ok();
                        last_tooltip = tooltip;
                    }
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

fn handle_menu_event(
    id: &tray_icon::menu::MenuId,
    open_id: &tray_icon::menu::MenuId,
    manage_id: &tray_icon::menu::MenuId,
    settings_id: &tray_icon::menu::MenuId,
    logout_id: &tray_icon::menu::MenuId,
    quit_id: &tray_icon::menu::MenuId,
    auth: &AuthState,
    config: &AppConfig,
    rt: &tokio::runtime::Runtime,
) {
    if id == open_id {
        info!("Opening sync folder: {}", config.sync_folder);
        if let Err(e) = open::that(&config.sync_folder) {
            error!("Failed to open folder: {e}");
        }
    } else if id == manage_id {
        info!("Manage folders clicked — launching subprocess");
        spawn_ui_subprocess("--manage-folders");
    } else if id == settings_id {
        info!("Settings clicked — launching subprocess");
        spawn_ui_subprocess("--settings");
    } else if id == logout_id {
        info!("Logging out...");
        let _ = rt.block_on(auth.logout());
        info!("Logged out. Exiting.");
        std::process::exit(0);
    } else if id == quit_id {
        info!("Quit requested");
        std::process::exit(0);
    }
}

/// Spawn a UI window (settings, file browser) in a separate process.
/// This avoids eframe lifecycle issues — each window gets its own process
/// with a fresh GPU context, and the tray loop is never blocked.
fn spawn_ui_subprocess(flag: &str) {
    match std::env::current_exe() {
        Ok(exe) => {
            match std::process::Command::new(&exe).arg(flag).spawn() {
                Ok(_) => info!("UI subprocess started: {flag}"),
                Err(e) => error!("Failed to spawn UI subprocess: {e}"),
            }
        }
        Err(e) => error!("Cannot determine exe path: {e}"),
    }
}
