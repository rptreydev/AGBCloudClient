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

// ── Duplicate-instance guard ───────────────────────────────────────────────────

/// Count how many instances of the current executable are alive (including self).
/// Used to prevent a duplicate tray icon when a race condition leaves two
/// processes alive simultaneously (e.g. Explorer restart during wizard hand-off).
#[cfg(target_os = "windows")]
fn count_running_instances() -> u32 {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW,
        PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };

    let exe_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_lowercase))
        .unwrap_or_else(|| "agb-cloud-client.exe".to_string());

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return 1; // Can't enumerate — assume we're the only one
    }

    let mut count = 0u32;
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
        loop {
            let raw = OsString::from_wide(&entry.szExeFile);
            let name = raw.to_string_lossy().trim_end_matches('\0').to_lowercase();
            if name == exe_name {
                count += 1;
            }
            if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snapshot) };
    count
}

#[cfg(not(target_os = "windows"))]
fn count_running_instances() -> u32 { 1 }

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
    // ── Duplicate-process guard ───────────────────────────────────────────────
    // Before registering the tray icon, verify no other tray instance of this
    // executable is running.  Prevents a duplicate icon when Explorer restarts
    // while a race between the old and new tray process is still in progress.
    //
    // Strategy: if count > 1, the extra process may be a SHORT-LIVED parent
    // (e.g. the setup wizard, which intentionally sleeps ~800 ms after spawning
    // us before calling exit(0)).  We wait up to 2 s in short bursts to give
    // those transient processes time to exit.  Only after 2 s with count still
    // > 1 do we treat it as a genuine duplicate tray and bail out.
    {
        let mut instances = count_running_instances();
        if instances > 1 {
            info!(
                "Duplicate tray guard: {} instances detected — waiting up to 2 s \
                 for transient processes (e.g. setup wizard) to exit",
                instances
            );
            for _ in 0u8..8 {   // 8 × 250 ms = 2 s maximum wait
                std::thread::sleep(std::time::Duration::from_millis(250));
                instances = count_running_instances();
                if instances <= 1 {
                    info!("Transient process exited — proceeding with tray icon creation");
                    break;
                }
            }
        }
        if instances > 1 {
            warn!(
                "Duplicate tray guard: {} instances still present after 2 s — \
                 another process already owns the tray icon. Exiting.",
                instances
            );
            std::process::exit(0);
        }
    }

    let mut is_auth = rt.block_on(auth.is_authenticated());

    let (menu, menu_ids) = build_context_menu();

    // ── Ghost icon cleanup ────────────────────────────────────────────────────
    // When a tray process is force-killed (Ctrl+C, taskkill /F, power loss),
    // Shell_NotifyIcon(NIM_DELETE) is never called, leaving a stale (HWND, uID)
    // entry in Explorer's TrayNotify registry cache.  When Explorer restarts it
    // restores these ghost icons alongside our new NIM_ADD → 2 icons visible.
    //
    // Fix: the previous run stored its HWND in a temp file.  We read it here and
    // call NIM_DELETE for that old HWND BEFORE creating our new icon.  Explorer
    // removes the ghost immediately, then our NIM_ADD registers exactly 1 icon.
    // We also enumerate any live "tray_icon_app" windows (race-condition safety).
    #[cfg(target_os = "windows")]
    {
        use winapi::um::shellapi::{NIM_DELETE, NOTIFYICONDATAW};
        use winapi::um::winuser::{FindWindowExW, FindWindowW};

        let hwnd_file = std::env::temp_dir().join("agb_tray_hwnd.dat");

        // (a) Remove ghost from previous run via stored HWND
        if let Ok(data) = std::fs::read_to_string(&hwnd_file) {
            if let Ok(hwnd_val) = data.trim().parse::<usize>() {
                // tray-icon 0.19 uses an incrementing counter for uID; the first
                // (and normally only) TrayIcon per process gets uID = 1.  We try
                // 1–3 to be safe against retry attempts that incremented the counter.
                for uid in 1u32..=3 {
                    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
                    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                    nid.hWnd = hwnd_val as winapi::shared::windef::HWND;
                    nid.uID = uid;
                    unsafe { winapi::um::shellapi::Shell_NotifyIconW(NIM_DELETE, &mut nid); }
                }
                info!("Ghost icon cleanup: NIM_DELETE for previous HWND {hwnd_val:#x}");
            }
        }

        // (b) Also remove any live "tray_icon_app" windows (handles the rare race
        //     where a previous instance hasn't fully exited yet).
        unsafe {
            let class: Vec<u16> = "tray_icon_app\0".encode_utf16().collect();
            let mut hwnd = FindWindowW(class.as_ptr(), std::ptr::null_mut());
            while !hwnd.is_null() {
                for uid in 1u32..=3 {
                    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
                    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                    nid.hWnd = hwnd;
                    nid.uID = uid;
                    winapi::um::shellapi::Shell_NotifyIconW(NIM_DELETE, &mut nid);
                }
                info!("Ghost icon cleanup: live tray_icon_app HWND {hwnd:?}");
                hwnd = FindWindowExW(
                    std::ptr::null_mut(), hwnd,
                    class.as_ptr(), std::ptr::null_mut(),
                );
            }
        }
    }

    // Retry tray icon creation — the notification area may not be fully
    // initialized if Explorer was just restarted.
    //
    // IMPORTANT: if TrayIconBuilder::build() fails (Shell_NotifyIcon(NIM_ADD)
    // returns FALSE because Shell_TrayWnd is not ready), tray-icon internally
    // creates a hidden HWND but does NOT destroy it on error.  That leaked HWND
    // has an active WNDPROC that will handle WM_TASKBARCREATED and call NIM_ADD
    // on its own when Explorer finishes starting.  If the retry loop then also
    // calls NIM_ADD via a new successful build(), we end up with 2 icons.
    //
    // Defence: after each failed attempt, find and DestroyWindow any leaked
    // "tray_icon_app" windows so their WNDPROC can no longer fire NIM_ADD.
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

                    // Destroy any leaked "tray_icon_app" HWNDs from the failed
                    // build() so their WNDPROC cannot register a ghost icon when
                    // WM_TASKBARCREATED fires during the 1-second sleep below.
                    #[cfg(target_os = "windows")]
                    unsafe {
                        use winapi::um::winuser::{DestroyWindow, FindWindowExW, FindWindowW};
                        let class: Vec<u16> = "tray_icon_app\0".encode_utf16().collect();
                        let mut leaked = FindWindowW(class.as_ptr(), std::ptr::null_mut());
                        while !leaked.is_null() {
                            let next = FindWindowExW(
                                std::ptr::null_mut(), leaked,
                                class.as_ptr(), std::ptr::null_mut(),
                            );
                            DestroyWindow(leaked);
                            leaked = next;
                        }
                    }

                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                Err(e) => {
                    error!("Failed to create tray icon after 5 attempts: {e}");
                    std::process::exit(1);
                }
            }
        }
    };

    // Store our hidden HWND so the NEXT run can call NIM_DELETE for it on startup,
    // preventing ghost icons when this process dies without calling NIM_DELETE.
    #[cfg(target_os = "windows")]
    {
        use winapi::um::winuser::FindWindowW;
        let hwnd_file = std::env::temp_dir().join("agb_tray_hwnd.dat");
        unsafe {
            let class: Vec<u16> = "tray_icon_app\0".encode_utf16().collect();
            let hwnd = FindWindowW(class.as_ptr(), std::ptr::null_mut());
            if !hwnd.is_null() {
                let _ = std::fs::write(&hwnd_file, format!("{}", hwnd as usize));
                info!("Stored tray HWND {:#x} for next-run ghost cleanup", hwnd as usize);
            }
        }
    }

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
        use winapi::um::winuser::{
            DispatchMessageW, PeekMessageW,
            TranslateMessage, PM_REMOVE, MSG,
        };

        // tray-icon 0.19 handles WM_TASKBARCREATED internally in its WNDPROC:
        // it calls Shell_NotifyIcon(NIM_DELETE) then Shell_NotifyIcon(NIM_ADD)
        // with the same HWND/uID, giving exactly one icon after Explorer restart.
        // We rely on that built-in handler — our own intercept was causing a
        // second NIM_ADD (one from tray-icon's WNDPROC + one from our rebuild).
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
                    // Do NOT drop(tray_icon) — Shell_NotifyIcon(NIM_DELETE) can block
                    // if Explorer is unresponsive, preventing exit(0) from being reached.
                    // Windows automatically destroys HWNDs (and removes the tray icon)
                    // when the process exits. Ghost-icon cleanup runs on next launch.
                    std::process::exit(0);
                } else if event.id == menu_ids.quit {
                    info!("Menu: Quit");
                    kill_all_children(&children);
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
                // Graceful-quit signal written by a new tray instance (--from-wizard
                // reinstall path) to avoid ghost icons from force-kill.
                // Presence of the flag file → delete it, drop icon (NIM_DELETE), exit.
                let quit_flag = std::env::temp_dir().join("agb_tray_quit.flag");
                if quit_flag.exists() {
                    let _ = std::fs::remove_file(&quit_flag);
                    info!("Graceful-quit flag detected — releasing tray icon and exiting");
                    kill_all_children(&children);
                    std::process::exit(0);
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
                    std::process::exit(0);
                } else if event.id == menu_ids.quit {
                    kill_all_children(&children);
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
                    std::process::exit(0);
                }
                if sig.logout_requested {
                    let _ = rt.block_on(auth_clone.logout());
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(&exe).arg("--login").spawn();
                    }
                    kill_all_children(&children);
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

/// Minimal tray loop used when an update is required.
///
/// Shows a single-item context menu ("Install Update") and blocks all other
/// actions until the update window subprocess completes installation.
/// The sync engine is intentionally NOT started in this path.
pub fn run_tray_update_mode(
    _auth: &crate::auth::AuthState,
    _config: &crate::config::AppConfig,
    _rt: &tokio::runtime::Runtime,
    info: &crate::update::UpdateInfo,
) {
    use tray_icon::menu::ContextMenu as _;

    let update_item = tray_icon::menu::MenuItem::new(
        &format!("⬆  Install Update {}…", info.version),
        true,
        None,
    );
    let quit_item = tray_icon::menu::MenuItem::new("Quit", true, None);

    let update_id = update_item.id().clone();
    let quit_id   = quit_item.id().clone();

    let menu = Menu::new();
    menu.append_items(&[&update_item, &PredefinedMenuItem::separator(), &quit_item])
        .expect("update menu build failed");

    let icon = crate::ui::icon::tray_icon();
    let tray_icon = TrayIconBuilder::new()
        .with_tooltip("AGB Cloud Client — Update Required")
        .with_icon(icon)
        .build()
        .expect("tray icon (update mode)");

    let tray_channel = TrayIconEvent::receiver();
    let menu_channel = MenuEvent::receiver();
    let info_clone   = info.clone();

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

            if let Ok(ev) = tray_channel.try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = ev
                {
                    // Left-click → open update window
                    crate::main_spawn_update_subprocess(&info_clone);
                }
                if let TrayIconEvent::Click {
                    button: MouseButton::Right,
                    button_state: MouseButtonState::Up,
                    ..
                } = ev
                {
                    unsafe {
                        use winapi::um::winuser::{
                            CreateWindowExW, DestroyWindow, GetCursorPos, SetForegroundWindow,
                            WS_EX_TOOLWINDOW, WS_POPUP,
                        };
                        let mut pt = winapi::shared::windef::POINT { x: 0, y: 0 };
                        GetCursorPos(&mut pt);
                        let cls: Vec<u16> = "STATIC\0".encode_utf16().collect();
                        let hwnd = CreateWindowExW(
                            WS_EX_TOOLWINDOW, cls.as_ptr(), std::ptr::null(),
                            WS_POPUP, pt.x, pt.y, 1, 1,
                            std::ptr::null_mut(), std::ptr::null_mut(),
                            std::ptr::null_mut(), std::ptr::null_mut(),
                        );
                        if !hwnd.is_null() {
                            SetForegroundWindow(hwnd);
                            menu.show_context_menu_for_hwnd(hwnd as isize, None);
                            DestroyWindow(hwnd);
                        }
                    }
                }
            }

            if let Ok(event) = menu_channel.try_recv() {
                if event.id == update_id {
                    crate::main_spawn_update_subprocess(&info_clone);
                } else if event.id == quit_id {
                    drop(tray_icon);
                    std::process::exit(0);
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        loop {
            if let Ok(event) = menu_channel.try_recv() {
                if event.id == update_id {
                    crate::main_spawn_update_subprocess(&info_clone);
                } else if event.id == quit_id {
                    std::process::exit(0);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Kill all tracked subprocesses (Settings, Manage Folders, Status windows).
/// Called on Quit so all open windows are closed with the tray.
/// Also writes the shutdown flag so any subprocess from a previous session exits too.
fn kill_all_children(children: &Children) {
    // Signal flag first — catches orphaned subprocesses not in `children`
    // (e.g. opened during a previous tray session that was later restarted).
    crate::ui::common::request_shutdown();
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
