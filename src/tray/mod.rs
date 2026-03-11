pub mod menu;  // reserved for future menu extensions

use std::sync::{Arc, Mutex};
use tracing::{error, info};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, MenuId};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::sync::SharedProgress;

type Children = Arc<Mutex<Vec<std::process::Child>>>;

// ── Menu item ID bundle ────────────────────────────────────────────────────────

struct MenuIds {
    // Authenticated items (Some only in auth menu)
    open_id:     Option<MenuId>,
    manage_id:   Option<MenuId>,
    settings_id: Option<MenuId>,
    logout_id:   Option<MenuId>,
    // Unauthenticated items (Some only in unauth menu)
    sign_in_id:  Option<MenuId>,
    // Always present
    quit_id:     MenuId,
    is_auth:     bool,
}

fn build_tray_menu(is_auth: bool) -> (Menu, MenuIds) {
    let menu = Menu::new();
    if is_auth {
        let open     = MenuItem::new("Open Sync Folder", true, None);
        let manage   = MenuItem::new("Manage Folders",   true, None);
        let settings = MenuItem::new("Settings",         true, None);
        let logout   = MenuItem::new("Logout",           true, None);
        let quit     = MenuItem::new("Quit",             true, None);
        let ids = MenuIds {
            open_id:     Some(open.id().clone()),
            manage_id:   Some(manage.id().clone()),
            settings_id: Some(settings.id().clone()),
            logout_id:   Some(logout.id().clone()),
            sign_in_id:  None,
            quit_id:     quit.id().clone(),
            is_auth:     true,
        };
        menu.append(&open).ok();
        menu.append(&manage).ok();
        menu.append(&settings).ok();
        menu.append(&logout).ok();
        menu.append(&quit).ok();
        (menu, ids)
    } else {
        let sign_in = MenuItem::new("Sign In...", true, None);
        let quit    = MenuItem::new("Quit",       true, None);
        let ids = MenuIds {
            open_id:     None,
            manage_id:   None,
            settings_id: None,
            logout_id:   None,
            sign_in_id:  Some(sign_in.id().clone()),
            quit_id:     quit.id().clone(),
            is_auth:     false,
        };
        menu.append(&sign_in).ok();
        menu.append(&quit).ok();
        (menu, ids)
    }
}

// ── Action returned by the menu event handler ─────────────────────────────────

enum MenuAction {
    None,
    /// Process must terminate — caller handles exit so it fires from the main loop.
    Quit,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the system tray icon with context menu.
/// Blocks the calling thread (main thread) forever, pumping Windows messages.
pub fn run_tray(
    auth: &AuthState,
    config: &AppConfig,
    rt: &tokio::runtime::Runtime,
    progress: SharedProgress,
) {
    let icon = crate::ui::icon::tray_icon();

    // Build initial menu based on current auth state
    let is_auth = rt.block_on(auth.is_authenticated());
    let (initial_menu, mut ids) = build_tray_menu(is_auth);

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(initial_menu))
        .with_tooltip("AGB Cloud Client — AGBroadband")
        .with_icon(icon)
        .build()
        .expect("Failed to create tray icon");

    info!("System tray running (authenticated: {is_auth})");

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();
    let auth_clone = auth.clone();
    let config_ptr = config as *const AppConfig;

    // Track spawned subprocesses so we can kill them on Quit
    let children: Children = Arc::new(Mutex::new(Vec::new()));

    let mut last_tooltip = String::new();
    let mut tick_count: u32 = 0;
    let mut auth_check_ticks: u32 = 0;
    let mut last_menu_event = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(10))
        .unwrap_or_else(std::time::Instant::now);

    // ── Windows message pump ─────────────────────────────────────────────────
    #[cfg(target_os = "windows")]
    {
        use std::mem::zeroed;
        use winapi::um::winuser::{DispatchMessageW, PeekMessageW, TranslateMessage, PM_REMOVE, MSG};

        loop {
            unsafe {
                let mut msg: MSG = zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            // Handle menu item clicks
            if let Ok(event) = menu_channel.try_recv() {
                last_menu_event = std::time::Instant::now();
                let config_ref = unsafe { &*config_ptr };
                match handle_menu_event(&event.id, &ids, &auth_clone, config_ref, rt, &children) {
                    MenuAction::Quit => {
                        info!("Quit — killing subprocesses and terminating");
                        kill_all_children(&children);
                        drop(tray_icon);
                        std::process::exit(0);
                    }
                    MenuAction::None => {}
                }
            }

            // Left-click on tray icon
            if let Ok(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) = tray_channel.try_recv()
            {
                if last_menu_event.elapsed() > std::time::Duration::from_millis(500) {
                    let config_ref = unsafe { &*config_ptr };
                    if ids.is_auth {
                        info!("Tray icon clicked — opening sync folder");
                        if let Err(e) = open::that(&config_ref.sync_folder) {
                            error!("Failed to open folder: {e}");
                        }
                    } else {
                        info!("Tray icon clicked (unauthenticated) — opening login");
                        spawn_login();
                    }
                }
            }

            tick_count = tick_count.wrapping_add(1);

            // Update tooltip every ~1 s (100 ticks × 10 ms)
            if tick_count % 100 == 0 {
                if let Ok(p) = progress.lock() {
                    let tooltip = p.tooltip();
                    if tooltip != last_tooltip {
                        tray_icon.set_tooltip(Some(&tooltip)).ok();
                        last_tooltip = tooltip;
                    }
                }
                // Clean up subprocesses that have already exited naturally
                if let Ok(mut c) = children.lock() {
                    c.retain_mut(|child| child.try_wait().map(|s| s.is_none()).unwrap_or(true));
                }
            }

            // Check auth state every ~30 s (3 000 ticks × 10 ms)
            auth_check_ticks = auth_check_ticks.wrapping_add(1);
            if auth_check_ticks >= 3000 {
                auth_check_ticks = 0;
                let new_auth = rt.block_on(auth_clone.is_authenticated());
                if new_auth != ids.is_auth {
                    info!("Auth state changed ({} → {}) — rebuilding tray menu", ids.is_auth, new_auth);
                    let (new_menu, new_ids) = build_tray_menu(new_auth);
                    tray_icon.set_menu(Some(Box::new(new_menu)));
                    ids = new_ids;
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        loop {
            if let Ok(event) = menu_channel.try_recv() {
                last_menu_event = std::time::Instant::now();
                let config_ref = unsafe { &*config_ptr };
                match handle_menu_event(&event.id, &ids, &auth_clone, config_ref, rt, &children) {
                    MenuAction::Quit => {
                        info!("Quit — killing subprocesses and terminating");
                        kill_all_children(&children);
                        drop(tray_icon);
                        std::process::exit(0);
                    }
                    MenuAction::None => {}
                }
            }

            if let Ok(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) = tray_channel.try_recv()
            {
                if last_menu_event.elapsed() > std::time::Duration::from_millis(500) {
                    let config_ref = unsafe { &*config_ptr };
                    if ids.is_auth {
                        let _ = open::that(&config_ref.sync_folder);
                    } else {
                        spawn_login();
                    }
                }
            }

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

            auth_check_ticks = auth_check_ticks.wrapping_add(1);
            if auth_check_ticks >= 600 {
                auth_check_ticks = 0;
                let new_auth = rt.block_on(auth_clone.is_authenticated());
                if new_auth != ids.is_auth {
                    info!("Auth state changed ({} → {}) — rebuilding tray menu", ids.is_auth, new_auth);
                    let (new_menu, new_ids) = build_tray_menu(new_auth);
                    tray_icon.set_menu(Some(Box::new(new_menu)));
                    ids = new_ids;
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

// ── Event handler ─────────────────────────────────────────────────────────────

fn handle_menu_event(
    id: &MenuId,
    ids: &MenuIds,
    auth: &AuthState,
    config: &AppConfig,
    rt: &tokio::runtime::Runtime,
    children: &Children,
) -> MenuAction {
    // Authenticated menu items
    if let Some(ref oid) = ids.open_id {
        if id == oid {
            info!("Opening sync folder: {}", config.sync_folder);
            if let Err(e) = open::that(&config.sync_folder) {
                error!("Failed to open folder: {e}");
            }
            return MenuAction::None;
        }
    }
    if let Some(ref mid) = ids.manage_id {
        if id == mid {
            info!("Manage folders clicked — launching subprocess");
            spawn_ui_subprocess("--manage-folders", children);
            return MenuAction::None;
        }
    }
    if let Some(ref sid) = ids.settings_id {
        if id == sid {
            info!("Settings clicked — launching subprocess");
            spawn_ui_subprocess("--settings", children);
            return MenuAction::None;
        }
    }
    if let Some(ref lid) = ids.logout_id {
        if id == lid {
            info!("Logging out...");
            let _ = rt.block_on(auth.logout());
            info!("Logged out — relaunching for sign-in.");
            if let Ok(exe) = std::env::current_exe() {
                if let Err(e) = std::process::Command::new(&exe).arg("--login").spawn() {
                    error!("Failed to relaunch for sign-in: {e}");
                }
            }
            return MenuAction::Quit;
        }
    }

    // Unauthenticated menu items
    if let Some(ref siid) = ids.sign_in_id {
        if id == siid {
            info!("Sign In clicked — launching login window");
            spawn_login();
            return MenuAction::None;
        }
    }

    // Quit (always present)
    if id == &ids.quit_id {
        info!("Quit menu item matched");
        return MenuAction::Quit;
    }

    MenuAction::None
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn spawn_login() {
    match std::env::current_exe() {
        Ok(exe) => match std::process::Command::new(&exe).arg("--login").spawn() {
            Ok(_) => info!("Login window spawned"),
            Err(e) => error!("Failed to spawn login window: {e}"),
        },
        Err(e) => error!("Cannot determine exe path: {e}"),
    }
}

fn spawn_ui_subprocess(flag: &str, children: &Children) {
    match std::env::current_exe() {
        Ok(exe) => match std::process::Command::new(&exe).arg(flag).spawn() {
            Ok(child) => {
                info!("UI subprocess started: {flag} (pid={})", child.id());
                if let Ok(mut c) = children.lock() {
                    c.push(child);
                }
            }
            Err(e) => error!("Failed to spawn UI subprocess: {e}"),
        },
        Err(e) => error!("Cannot determine exe path: {e}"),
    }
}

/// Kill all tracked subprocesses (Settings, Manage Folders windows).
/// Called on Quit so all open windows are closed with the tray.
fn kill_all_children(children: &Children) {
    if let Ok(mut c) = children.lock() {
        for child in c.iter_mut() {
            let pid = child.id();
            match child.kill() {
                Ok(()) => info!("Killed subprocess pid={pid}"),
                Err(e) => info!("Could not kill subprocess pid={pid}: {e}"),
            }
        }
        c.clear();
    }
}
