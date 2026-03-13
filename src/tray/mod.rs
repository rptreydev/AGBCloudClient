pub mod menu;  // reserved for future menu extensions

use std::sync::{Arc, Mutex};
use tracing::{error, info, warn};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::sync::progress::{read_progress_file, write_progress_file};
use crate::sync::SharedProgress;

type Children = Arc<Mutex<Vec<std::process::Child>>>;

/// IDs of the context menu items (stored so we can match events).
struct MenuIds {
    status:  tray_icon::menu::MenuId,
    folders: tray_icon::menu::MenuId,
    settings: tray_icon::menu::MenuId,
    logout:  tray_icon::menu::MenuId,
    quit:    tray_icon::menu::MenuId,
}

/// Build the right-click context menu and return it with the item IDs.
fn build_context_menu() -> (Menu, MenuIds) {
    let status_item   = MenuItem::new("Open Status Panel",  true, None);
    let folders_item  = MenuItem::new("Manage Folders...",  true, None);
    let settings_item = MenuItem::new("Settings...",        true, None);
    let logout_item   = MenuItem::new("Sign Out",           true, None);
    let quit_item     = MenuItem::new("Quit",               true, None);

    let ids = MenuIds {
        status:   status_item.id().clone(),
        folders:  folders_item.id().clone(),
        settings: settings_item.id().clone(),
        logout:   logout_item.id().clone(),
        quit:     quit_item.id().clone(),
    };

    let menu = Menu::new();
    menu.append_items(&[
        &status_item,
        &folders_item,
        &settings_item,
        &PredefinedMenuItem::separator(),
        &logout_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .expect("Failed to build tray context menu");

    (menu, ids)
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the system tray icon.
/// Left-click  → open status panel (or login if unauthenticated).
/// Right-click → show context menu.
/// Blocks the calling thread (main thread) forever.
pub fn run_tray(
    auth: &AuthState,
    _config: &AppConfig,
    rt: &tokio::runtime::Runtime,
    progress: SharedProgress,
    sync_trigger: std::sync::Arc<tokio::sync::Notify>,
) {
    let mut is_auth = rt.block_on(auth.is_authenticated());

    let (menu, menu_ids) = build_context_menu();

    // Retry tray icon creation — the notification area may not be fully
    // initialized if Explorer was just restarted.
    let tray_icon = {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let icon = crate::ui::icon::tray_icon();

            // NOTE: no .with_menu() — we show the menu manually on right-click only,
            // which prevents the menu from also appearing on left-click.
            match TrayIconBuilder::new()
                .with_tooltip("AGB Cloud Client — AGBroadband")
                .with_icon(icon)
                .build()
            {
                Ok(t) => {
                    if attempt > 1 {
                        info!("Tray icon created on attempt {attempt}");
                    }
                    break t;
                }
                Err(e) if attempt < 5 => {
                    warn!("Tray icon creation failed (attempt {attempt}/5): {e} — retrying in 1s");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                Err(e) => {
                    error!("Failed to create tray icon after 5 attempts: {e}");
                    std::process::exit(1);
                }
            }
        }
    };

    info!("System tray running (context menu enabled, authenticated: {is_auth})");

    let tray_channel = TrayIconEvent::receiver();
    let menu_channel = MenuEvent::receiver();
    let auth_clone = auth.clone();

    // Track spawned subprocesses so we can kill them on Quit
    let children: Children = Arc::new(Mutex::new(Vec::new()));

    let mut last_tooltip = String::new();
    let mut tick_count: u32 = 0;
    let mut auth_check_ticks: u32 = 0;
    // Debounce tray clicks
    let mut last_click = std::time::Instant::now()
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

            // Tray clicks: left → panel, right → context menu
            if let Ok(ev) = tray_channel.try_recv() {
                match ev {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => {
                        if last_click.elapsed() > std::time::Duration::from_millis(500) {
                            last_click = std::time::Instant::now();
                            if is_auth {
                                info!("Tray left-click — opening status panel");
                                spawn_ui_subprocess("--status", &children);
                            } else {
                                info!("Tray left-click (unauthenticated) — opening login");
                                spawn_login();
                            }
                        }
                    }
                    TrayIconEvent::Click {
                        button: MouseButton::Right,
                        button_state: MouseButtonState::Up,
                        ..
                    } => {
                        // Show context menu manually on right-click only.
                        // We create a tiny invisible WS_POPUP window on this thread so
                        // that TrackPopupMenu (inside show_context_menu_for_hwnd) has a
                        // valid owner and SetForegroundWindow can activate it properly.
                        unsafe {
                            use winapi::um::winuser::{
                                CreateWindowExW, DestroyWindow, SetForegroundWindow,
                                GetCursorPos, WS_POPUP, WS_EX_TOOLWINDOW,
                            };
                            use tray_icon::menu::ContextMenu as _;
                            let mut pt = winapi::shared::windef::POINT { x: 0, y: 0 };
                            GetCursorPos(&mut pt);
                            let cls: Vec<u16> = "STATIC\0".encode_utf16().collect();
                            let hwnd = CreateWindowExW(
                                WS_EX_TOOLWINDOW,
                                cls.as_ptr(),
                                std::ptr::null(),
                                WS_POPUP,
                                pt.x, pt.y, 1, 1,
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                            );
                            if !hwnd.is_null() {
                                SetForegroundWindow(hwnd);
                                // show_context_menu_for_hwnd blocks until the menu is dismissed,
                                // then queues a MenuEvent which we pick up below.
                                menu.show_context_menu_for_hwnd(hwnd as isize, None);
                                DestroyWindow(hwnd);
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Context menu item selected (queued by show_context_menu_for_hwnd above)
            if let Ok(event) = menu_channel.try_recv() {
                if event.id == menu_ids.status {
                    info!("Menu: Open Status Panel");
                    if is_auth { spawn_ui_subprocess("--status", &children); }
                    else { spawn_login(); }
                } else if event.id == menu_ids.folders {
                    info!("Menu: Manage Folders");
                    spawn_ui_subprocess("--manage-folders", &children);
                } else if event.id == menu_ids.settings {
                    info!("Menu: Settings");
                    spawn_ui_subprocess("--settings", &children);
                } else if event.id == menu_ids.logout && is_auth {
                    info!("Menu: Sign Out");
                    let _ = rt.block_on(auth_clone.logout());
                    if let Ok(exe) = std::env::current_exe() {
                        if let Err(e) = std::process::Command::new(&exe).arg("--login").spawn() {
                            error!("Failed to relaunch for sign-in: {e}");
                        }
                    }
                    kill_all_children(&children);
                    drop(tray_icon);
                    std::process::exit(0);
                } else if event.id == menu_ids.quit {
                    info!("Menu: Quit");
                    kill_all_children(&children);
                    drop(tray_icon);
                    std::process::exit(0);
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
                if new_auth != is_auth {
                    info!("Auth state changed ({} → {})", is_auth, new_auth);
                    is_auth = new_auth;
                }
            }

            // Poll quit/logout/sync signals from status panel every ~5 s (500 ticks × 10 ms)
            if tick_count % 500 == 0 {
                let mut sig = read_progress_file();
                if sig.quit_requested {
                    info!("Quit signal received from status panel — terminating");
                    kill_all_children(&children);
                    drop(tray_icon);
                    std::process::exit(0);
                }
                if sig.logout_requested {
                    info!("Logout signal received from status panel");
                    let _ = rt.block_on(auth_clone.logout());
                    if let Ok(exe) = std::env::current_exe() {
                        if let Err(e) = std::process::Command::new(&exe).arg("--login").spawn() {
                            error!("Failed to relaunch for sign-in: {e}");
                        }
                    }
                    kill_all_children(&children);
                    drop(tray_icon);
                    std::process::exit(0);
                }
                if sig.sync_requested {
                    info!("Sync trigger received — waking engine immediately");
                    sig.sync_requested = false;
                    write_progress_file(&sig);
                    sync_trigger.notify_one();
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        loop {
            if let Ok(ev) = tray_channel.try_recv() {
                match ev {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => {
                        if last_click.elapsed() > std::time::Duration::from_millis(500) {
                            last_click = std::time::Instant::now();
                            if is_auth {
                                spawn_ui_subprocess("--status", &children);
                            } else {
                                spawn_login();
                            }
                        }
                    }
                    TrayIconEvent::Click {
                        button: MouseButton::Right,
                        button_state: MouseButtonState::Up,
                        ..
                    } => {
                        use tray_icon::menu::ContextMenu as _;
                        menu.show_context_menu_for_hwnd(0, None);
                    }
                    _ => {}
                }
            }

            if let Ok(event) = menu_channel.try_recv() {
                if event.id == menu_ids.status {
                    if is_auth { spawn_ui_subprocess("--status", &children); } else { spawn_login(); }
                } else if event.id == menu_ids.folders {
                    spawn_ui_subprocess("--manage-folders", &children);
                } else if event.id == menu_ids.settings {
                    spawn_ui_subprocess("--settings", &children);
                } else if event.id == menu_ids.logout && is_auth {
                    let _ = rt.block_on(auth_clone.logout());
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(&exe).arg("--login").spawn();
                    }
                    kill_all_children(&children);
                    drop(tray_icon);
                    std::process::exit(0);
                } else if event.id == menu_ids.quit {
                    kill_all_children(&children);
                    drop(tray_icon);
                    std::process::exit(0);
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
                if new_auth != is_auth {
                    info!("Auth state changed ({} → {})", is_auth, new_auth);
                    is_auth = new_auth;
                }
            }

            if tick_count % 100 == 0 {
                let mut sig = read_progress_file();
                if sig.quit_requested {
                    kill_all_children(&children);
                    drop(tray_icon);
                    std::process::exit(0);
                }
                if sig.logout_requested {
                    let _ = rt.block_on(auth_clone.logout());
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(&exe).arg("--login").spawn();
                    }
                    kill_all_children(&children);
                    drop(tray_icon);
                    std::process::exit(0);
                }
                if sig.sync_requested {
                    sig.sync_requested = false;
                    write_progress_file(&sig);
                    sync_trigger.notify_one();
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
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

/// Kill all tracked subprocesses (Settings, Manage Folders, Status windows).
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
