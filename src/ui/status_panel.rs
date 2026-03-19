//! iCloud-style live status panel.
//! Spawned as `--status` subprocess when the user clicks the tray icon.
//! Communicates with the sync engine via a shared progress.json file.

use std::sync::Arc;
use std::sync::mpsc;
use tracing::{error, info};

use crate::auth::AuthState;
use crate::models::cloud_file::FolderSelection;
use crate::models::user::UserRole;
use crate::sync::progress::{read_progress_file, SyncPhase, SyncProgress};
use crate::ui::common::{
    work_area, ACCENT, ACCENT_DIM, BAR_BG, CARD_BG, DARK_BG, DIVIDER,
    ERROR_COLOR, FOLDER_COLOR, SUCCESS_COLOR, SURFACE, SURFACE_VARIANT,
    TEXT_DISABLED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use crate::ws::events::{TreePatch, WsClient};

/// A single filesystem entry (file or directory) for the inline file browser.
struct DirEntry {
    name: String,
    path: std::path::PathBuf,
    is_dir: bool,
    size_bytes: u64,
    /// Unix timestamp (seconds) of last modification, if available.
    modified_secs: Option<u64>,
}

#[derive(PartialEq, Clone, Copy)]
enum BrowserTab { Folders, Files, Activity }

#[derive(PartialEq, Clone, Copy)]
enum ViewMode { List, Grid, Details }

// ── Entry point ───────────────────────────────────────────────────────────────

/// Show the iCloud-style status panel positioned at the bottom-right of the screen.
/// Always calls `process::exit(0)` — never returns.
pub fn show_status_panel(auth: &AuthState, rt: &tokio::runtime::Runtime) {
    let (wa_left, wa_top, wa_right, wa_bottom) = work_area();
    let wa_w = wa_right - wa_left;
    let wa_h = wa_bottom - wa_top;
    // Width: ~18% of screen width, clamped between 300 and 440 px.
    let win_w = (wa_w * 0.18).clamp(300.0, 440.0);
    // Height: ~82% of available work area, clamped between 480 and 920 px.
    let win_h = (wa_h * 0.82).clamp(480.0, 920.0);
    let pos_x = (wa_right - win_w - 8.0).max(0.0);
    let pos_y = (wa_bottom - win_h - 8.0).max(0.0);

    // Get user info: full name + role from in-memory auth state (set after try_restore_session)
    let user = rt.block_on(auth.get_user());
    let username = crate::auth::store::CredentialStore::get_last_username()
        .ok()
        .flatten()
        .unwrap_or_else(|| "Unknown".to_string());

    let display_name = user
        .as_ref()
        .and_then(|u| match (u.first_name.as_deref(), u.last_name.as_deref()) {
            (Some(f), Some(l)) if !f.is_empty() => Some(format!("{f} {l}")),
            (Some(f), _) if !f.is_empty() => Some(f.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| username.clone());

    let role_label = user
        .as_ref()
        .and_then(|u| u.role.as_ref())
        .map(|r| match r {
            UserRole::SADMIN => "Super Admin",
            UserRole::ADMIN => "Administrator",
            UserRole::SUPERVISOR => "Supervisor",
            UserRole::COMPANY_SUPERVISOR => "Company Supervisor",
            UserRole::INSTALLER => "Installer",
            UserRole::VIEWER => "Viewer",
            UserRole::Unknown => "User",
        })
        .unwrap_or("User")
        .to_string();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([win_w, win_h])
            .with_min_inner_size([300.0, 480.0])
            .with_position([pos_x, pos_y])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(false)
            .with_icon(Arc::new(crate::ui::icon::app_icon_data())),
        ..Default::default()
    };

    // Spawn a WS listener so company-assignment changes refresh the panel in real-time.
    let (patch_tx, patch_rx) = mpsc::channel();
    let ws_auth   = auth.clone();
    let ws_config = crate::config::AppConfig::load_or_create().unwrap_or_default();
    let dummy_trigger = Arc::new(tokio::sync::Notify::new());
    rt.spawn(async move {
        let my_username = ws_auth.current_username().await.unwrap_or_default();
        WsClient::run(ws_config, ws_auth, dummy_trigger, Some((my_username, patch_tx)), None).await;
    });

    info!("Launching status panel at ({pos_x:.0}, {pos_y:.0}), size {win_w}x{win_h:.0}");
    let run_result = eframe::run_native(
        "AGB Cloud Client — Status",
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(StatusPanel::new(display_name, username, role_label, patch_rx)))
        }),
    );
    match run_result {
        Ok(()) => info!("Status panel closed normally"),
        Err(e) => error!("Status panel error: {e}"),
    }
    std::process::exit(0);
}

// ── App struct ────────────────────────────────────────────────────────────────

struct StatusPanel {
    display_name: String,
    username: String,
    role_label: String,
    /// Avatar initials (up to 2 chars, uppercase)
    initials: String,
    progress: SyncProgress,
    last_poll: std::time::Instant,
    /// Selected sync folders (from config)
    selected_folders: Vec<FolderSelection>,
    sync_folder: String,
    config_last_poll: std::time::Instant,
    /// Active tab: Folders (synced) or Files (browser)
    active_tab: BrowserTab,
    /// File browser view mode: List, Grid, or Details
    view_mode: ViewMode,
    /// File browser: stack of directories navigated into
    nav_stack: Vec<std::path::PathBuf>,
    /// File browser: cached entries of the current directory
    dir_entries: Vec<DirEntry>,
    /// Thumbnail cache: path → loaded texture (None = failed / not yet ready)
    thumbnail_cache: std::collections::HashMap<std::path::PathBuf, Option<egui::TextureHandle>>,
    /// Async thumbnail decoding: background thread sends (path, ColorImage)
    thumbnail_rx: Vec<std::sync::mpsc::Receiver<(std::path::PathBuf, Option<egui::ColorImage>)>>,
    /// Paths whose thumbnail decode has been dispatched (avoids duplicate spawns)
    thumbnail_loading: std::collections::HashSet<std::path::PathBuf>,
    /// Async directory load in progress
    load_rx: Option<std::sync::mpsc::Receiver<Vec<DirEntry>>>,
    /// True between the moment nav is applied and entries arrive — ensures spinner is shown
    is_loading: bool,
    /// Search/filter query for the file browser
    search_query: String,
    /// Previous sync phase — used to detect Syncing → Idle transitions
    prev_phase: SyncPhase,
    /// Receives company-assignment patches from the WS listener.
    patch_rx: Option<mpsc::Receiver<TreePatch>>,
    done: bool,
}

impl StatusPanel {
    fn new(display_name: String, username: String, role_label: String, patch_rx: mpsc::Receiver<TreePatch>) -> Self {
        let initials = display_name
            .split_whitespace()
            .filter_map(|w| w.chars().next())
            .take(2)
            .collect::<String>()
            .to_uppercase();
        let (selected_folders, sync_folder) = load_folders_from_config();
        Self {
            display_name,
            username,
            role_label,
            initials,
            progress: read_progress_file(),
            last_poll: std::time::Instant::now(),
            selected_folders,
            sync_folder,
            config_last_poll: std::time::Instant::now(),
            active_tab: BrowserTab::Folders,
            view_mode: ViewMode::List,
            nav_stack: Vec::new(),
            dir_entries: Vec::new(),
            thumbnail_cache: std::collections::HashMap::new(),
            thumbnail_rx: Vec::new(),
            thumbnail_loading: std::collections::HashSet::new(),
            load_rx: None,
            is_loading: false,
            search_query: String::new(),
            prev_phase: SyncPhase::Idle,
            patch_rx: Some(patch_rx),
            done: false,
        }
    }
}

fn load_folders_from_config() -> (Vec<FolderSelection>, String) {
    match crate::config::AppConfig::load_or_create() {
        Ok(cfg) => (cfg.selected_folders, cfg.sync_folder),
        Err(_) => (vec![], String::new()),
    }
}

impl eframe::App for StatusPanel {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Exit immediately if the tray has requested a global shutdown.
        if crate::ui::common::is_shutdown_requested() {
            std::process::exit(0);
        }
        // Poll progress file every 500 ms
        if self.last_poll.elapsed() >= std::time::Duration::from_millis(500) {
            self.progress = read_progress_file();
            self.last_poll = std::time::Instant::now();
        }
        // Reload config (folder list) every 3 s — picks up Manage Folders changes
        if self.config_last_poll.elapsed() >= std::time::Duration::from_secs(3) {
            let (folders, sf) = load_folders_from_config();
            self.selected_folders = folders;
            self.sync_folder = sf;
            self.config_last_poll = std::time::Instant::now();
        }
        // Drain company-assignment patches: on any change, force config reload and
        // silently refresh the Files tab so the new/removed folder appears immediately.
        let has_patch = self.patch_rx.as_ref()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).count() > 0)
            .unwrap_or(false);
        if has_patch {
            let (folders, sf) = load_folders_from_config();
            self.selected_folders = folders;
            self.sync_folder = sf.clone();
            self.config_last_poll = std::time::Instant::now();
            // Silently refresh Files tab if currently browsing
            if self.active_tab == BrowserTab::Files
                && !self.nav_stack.is_empty()
                && self.load_rx.is_none()
                && !self.is_loading
            {
                if let Some(current) = self.nav_stack.last().cloned() {
                    let root = std::path::PathBuf::from(&sf);
                    self.load_rx = Some(start_load(current, root));
                }
            }
            ctx.request_repaint();
        }
        // Auto-refresh file browser when a sync cycle finishes (Syncing → Idle/Error).
        // The sync cycle is triggered by the WS event, so this fires shortly after
        // a new file arrives — no need for a polling interval.
        {
            let prev_was_syncing = matches!(self.prev_phase, SyncPhase::Syncing);
            let curr_is_syncing  = matches!(self.progress.phase, SyncPhase::Syncing);
            let just_finished    = prev_was_syncing && !curr_is_syncing;
            self.prev_phase = self.progress.phase.clone();

            if just_finished
                && self.active_tab == BrowserTab::Files
                && !self.nav_stack.is_empty()
                && self.load_rx.is_none()
                && !self.is_loading
            {
                let sync_root = std::path::PathBuf::from(&self.sync_folder);
                if let Some(current) = self.nav_stack.last().cloned() {
                    // Silent background refresh: keep thumbnail cache (same directory,
                    // existing textures remain valid) and do NOT set is_loading=true
                    // so no spinner appears — entries update silently behind the scenes.
                    self.load_rx = Some(start_load(current, sync_root));
                }
            }
        }
        // Drain async dir-load result
        if let Some(rx) = &self.load_rx {
            match rx.try_recv() {
                Ok(entries) => {
                    self.dir_entries = entries;
                    self.load_rx = None;
                    self.is_loading = false;
                    ctx.request_repaint();
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.load_rx = None;
                    self.is_loading = false;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(30));
                }
            }
        }

        // Drain async thumbnail results — upload ColorImage → TextureHandle on main thread
        {
            let mut pending = Vec::new();
            for rx in self.thumbnail_rx.drain(..) {
                match rx.try_recv() {
                    Ok((path, Some(img))) => {
                        self.thumbnail_loading.remove(&path);
                        let key = path.to_string_lossy().to_string();
                        let tex = ctx.load_texture(key, img, egui::TextureOptions::LINEAR);
                        self.thumbnail_cache.insert(path, Some(tex));
                        ctx.request_repaint();
                    }
                    Ok((path, None)) => {
                        self.thumbnail_loading.remove(&path);
                        self.thumbnail_cache.insert(path, None);
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        pending.push(rx);
                    }
                }
            }
            self.thumbnail_rx = pending;
        }
        if !self.thumbnail_rx.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        // Keep refreshing while syncing so the progress bar animates
        let repaint_ms = if self.progress.phase == SyncPhase::Syncing { 250 } else { 1000 };
        ctx.request_repaint_after(std::time::Duration::from_millis(repaint_ms));

        if self.done {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            std::process::exit(0);
        }

        // ── Custom title bar ──────────────────────────────────────────────────
        // Uses manual painter calls so the dot + full title are always visible
        // regardless of the close-button allocation consuming space on the right.
        egui::TopBottomPanel::top("sp_titlebar")
            .frame(egui::Frame::default().fill(SURFACE).inner_margin(egui::Margin::same(0.0)))
            .exact_height(42.0)
            .show(ctx, |ui| {
                let bar = ui.available_rect_before_wrap();
                let h = bar.height();
                let mid_y = bar.min.y + h * 0.5;

                // ── Close button (top-right, allocated first so it gets sense) ──
                let close_size = 34.0_f32;
                let close_center = egui::pos2(bar.max.x - 4.0 - close_size * 0.5, mid_y);
                let close_rect = egui::Rect::from_center_size(
                    close_center,
                    egui::vec2(close_size, close_size),
                );
                let close_resp = ui.allocate_rect(close_rect, egui::Sense::click());
                if close_resp.hovered() {
                    ui.painter().rect_filled(
                        close_rect,
                        6.0,
                        egui::Color32::from_rgba_premultiplied(200, 50, 50, 60),
                    );
                    ctx.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                }
                // Draw X with painter lines — ✕ (U+2715) is not in egui's bundled font
                let c = close_center;
                let s = 5.0_f32;
                let col = if close_resp.hovered() { ERROR_COLOR } else { TEXT_SECONDARY };
                let stroke = egui::Stroke::new(2.0, col);
                ui.painter().line_segment(
                    [egui::pos2(c.x - s, c.y - s), egui::pos2(c.x + s, c.y + s)],
                    stroke,
                );
                ui.painter().line_segment(
                    [egui::pos2(c.x + s, c.y - s), egui::pos2(c.x - s, c.y + s)],
                    stroke,
                );
                if close_resp.clicked() {
                    self.done = true;
                }

                // ── Drag area (fills the rest of the title bar) ──
                let drag_rect = egui::Rect::from_min_max(bar.min, egui::pos2(close_rect.min.x, bar.max.y));
                let drag_resp = ui.allocate_rect(drag_rect, egui::Sense::drag());
                if drag_resp.dragged() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                // ── AGB accent dot ──
                let dot_center = egui::pos2(bar.min.x + 14.0, mid_y);
                ui.painter().circle_filled(dot_center, 4.0, ACCENT);

                // ── Title text (always fully visible, left of close button) ──
                ui.painter().text(
                    egui::pos2(dot_center.x + 12.0, mid_y),
                    egui::Align2::LEFT_CENTER,
                    "AGB Cloud Client",
                    egui::FontId::proportional(13.0),
                    TEXT_PRIMARY,
                );
            });

        // ── Tab bar ───────────────────────────────────────────────────────────
        egui::TopBottomPanel::top("sp_tabbar")
            .frame(egui::Frame::default().fill(SURFACE).inner_margin(egui::Margin::same(0.0)))
            .exact_height(40.0)
            .show(ctx, |ui| {
                let bar = ui.available_rect_before_wrap();
                // Bottom divider
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(egui::pos2(bar.min.x, bar.max.y - 1.0), bar.max),
                    0.0, DIVIDER,
                );
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    for (tab, label, icon) in [
                        (BrowserTab::Folders,  "Synced",    "☁"),
                        (BrowserTab::Files,    "Files",     "📂"),
                        (BrowserTab::Activity, "Activity",  "🕐"),
                    ] {
                        let is_active = self.active_tab == tab;
                        let text_col = if is_active { ACCENT } else { TEXT_SECONDARY };
                        let resp = ui.add(
                            egui::Button::new(
                                egui::RichText::new(format!("{icon}  {label}"))
                                    .size(12.5)
                                    .color(text_col),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .frame(false)
                            .min_size(egui::vec2(80.0, 40.0)),
                        );
                        if is_active {
                            // Accent underline
                            let r = resp.rect;
                            ui.painter().rect_filled(
                                egui::Rect::from_min_max(
                                    egui::pos2(r.min.x + 8.0, r.max.y - 2.0),
                                    egui::pos2(r.max.x - 8.0, r.max.y),
                                ),
                                1.0, ACCENT,
                            );
                        }
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !is_active {
                            self.active_tab = tab;
                            // Switch to Files: ensure nav_stack is initialized at sync root
                            if tab == BrowserTab::Files && self.nav_stack.is_empty() && !self.sync_folder.is_empty() {
                                let root = std::path::PathBuf::from(&self.sync_folder);
                                self.dir_entries.clear();
                                self.search_query.clear();
                                self.load_rx = Some(start_load(root.clone(), root.clone()));
                                self.nav_stack.push(root);
                            }
                        }
                    }
                    // View mode toggles — right-aligned, only in Files tab
                if self.active_tab == BrowserTab::Files {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(10.0);
                        for (mode, kind) in [
                            (ViewMode::Details, 0u8),
                            (ViewMode::Grid,    1u8),
                            (ViewMode::List,    2u8),
                        ] {
                            let is_act = self.view_mode == mode;
                            let col = if is_act { ACCENT } else { TEXT_SECONDARY };
                            let bg  = if is_act { SURFACE_VARIANT } else { egui::Color32::TRANSPARENT };
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
                            draw_view_mode_icon(ui.painter(), rect, kind, col, bg);
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                self.view_mode = mode;
                            }
                        }
                    });
                }
            });
        });

        // ── Quick-action footer ───────────────────────────────────────────────
        egui::TopBottomPanel::bottom("sp_actions")
            .frame(
                egui::Frame::default()
                    .fill(DARK_BG)
                    .inner_margin(egui::Margin { left: 16.0, right: 16.0, top: 10.0, bottom: 12.0 }),
            )
            .show(ctx, |ui| {
                // Divider
                let r = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                ui.painter().rect_filled(r, 0.0, DIVIDER);
                ui.add_space(8.0);

                let btn = |label: &str, color: egui::Color32| {
                    egui::Button::new(egui::RichText::new(label).size(11.5).color(color))
                        .fill(SURFACE_VARIANT)
                        .rounding(7.0)
                        .min_size(egui::vec2(0.0, 28.0))
                };

                // Row 1: main actions
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(5.0, 5.0);
                    if ui.add(btn("↗  Open Folder", TEXT_PRIMARY))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        if let Ok(cfg) = crate::config::AppConfig::load_or_create() {
                            let _ = open::that(&cfg.sync_folder);
                        }
                    }
                    if ui.add(btn("📁  Folders", TEXT_PRIMARY))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(&exe)
                                .arg("--manage-folders")
                                .spawn();
                        }
                    }
                    if ui.add(btn("⚙  Settings", TEXT_PRIMARY))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(&exe)
                                .arg("--settings")
                                .spawn();
                        }
                    }
                });

                ui.add_space(5.0);

                // Row 2: session / window actions
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(5.0, 5.0);
                    if ui.add(btn("↩  Logout", TEXT_SECONDARY))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        // Signal the tray process to perform logout
                        let mut p = crate::sync::progress::read_progress_file();
                        p.logout_requested = true;
                        crate::sync::progress::write_progress_file(&p);
                        self.done = true;
                    }
                    if ui.add(btn("\u{00D7}  Quit App", ERROR_COLOR))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        // Signal the tray process to quit
                        let mut p = crate::sync::progress::read_progress_file();
                        p.quit_requested = true;
                        crate::sync::progress::write_progress_file(&p);
                        self.done = true;
                    }
                    if ui.add(btn("—  Close Panel", TEXT_SECONDARY))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        self.done = true;
                    }
                });
            });

        // Navigation actions collected during rendering (applied after all panels)
        let mut nav_push: Option<std::path::PathBuf> = None;
        let mut nav_back = false;

        // ── Scrollable main content ───────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(DARK_BG).inner_margin(egui::Margin::same(0.0)))
            .show(ctx, |ui| {
                match self.active_tab {
                    BrowserTab::Folders => {
                        // ── Folders tab: fixed header + scrollable folder list ──
                        // User card and status are fixed; only the folder cards scroll.
                        ui.set_width(ui.available_width());
                        self.render_user_card(ui);
                        let r = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                        ui.painter().rect_filled(r, 0.0, DIVIDER);
                        self.render_status(ui);
                        let r = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                        ui.painter().rect_filled(r, 0.0, DIVIDER);
                        egui::ScrollArea::vertical()
                            .id_salt("sp_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.render_folders(ui);
                            });
                    }
                    BrowserTab::Activity => {
                        egui::ScrollArea::vertical()
                            .id_salt("sp_activity_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.render_activity(ui);
                            });
                    }
                    BrowserTab::Files => {
                        // ── Files tab: breadcrumb + view toggles + file entries ──
                        // Ensure nav_stack has at least the sync root
                        if self.nav_stack.is_empty() && !self.sync_folder.is_empty() {
                            let root = std::path::PathBuf::from(&self.sync_folder);
                            self.dir_entries.clear();
                            self.search_query.clear();
                            self.load_rx = Some(start_load(root.clone(), root.clone()));
                            self.nav_stack.push(root);
                        }
                        if self.render_browser_header(ui) {
                            nav_back = true;
                        }
                        let r = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                        ui.painter().rect_filled(r, 0.0, DIVIDER);

                        // ── Search bar ────────────────────────────────────────
                        egui::Frame::default()
                            .fill(SURFACE)
                            .inner_margin(egui::Margin { left: 10.0, right: 10.0, top: 6.0, bottom: 6.0 })
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.search_query)
                                        .hint_text("🔍  Filter files…")
                                        .id(egui::Id::new("sp_search"))
                                        .desired_width(f32::INFINITY)
                                        .font(egui::FontId::proportional(12.5)),
                                );
                            });
                        let r2 = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                        ui.painter().rect_filled(r2, 0.0, DIVIDER);

                        // ── Loading / entries ─────────────────────────────────
                        if self.load_rx.is_some() || self.is_loading {
                            ctx.output_mut(|o| o.cursor_icon = egui::CursorIcon::Progress);
                            egui::Frame::default()
                                .inner_margin(egui::Margin { left: 14.0, right: 14.0, top: 40.0, bottom: 40.0 })
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.vertical_centered(|ui| {
                                        ui.spinner();
                                        ui.add_space(8.0);
                                        ui.label(egui::RichText::new("Cargando…").size(12.0).color(TEXT_SECONDARY));
                                    });
                                });
                        } else {
                            egui::ScrollArea::vertical()
                                .id_salt("sp_browser_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    if let Some(path) = self.render_browser_entries(ui) {
                                        nav_push = Some(path);
                                    }
                                });
                        }
                    }
                }
            });

        // Apply navigation (must run after all panels are rendered)
        let sync_root = std::path::PathBuf::from(&self.sync_folder);
        if nav_back {
            // Don't pop past the sync root
            if self.nav_stack.len() > 1 {
                self.thumbnail_cache.clear();
                self.thumbnail_rx.clear();
                self.thumbnail_loading.clear();
                self.nav_stack.pop();
                self.dir_entries.clear();
                self.search_query.clear();
                if let Some(p) = self.nav_stack.last().cloned() {
                    self.is_loading = true;
                    self.load_rx = Some(start_load(p, sync_root));
                    ctx.request_repaint(); // show spinner on very next frame
                }
            }
        } else if let Some(path) = nav_push {
            self.thumbnail_cache.clear();
            self.thumbnail_rx.clear();
            self.thumbnail_loading.clear();
            self.dir_entries.clear();
            self.search_query.clear();
            self.is_loading = true;
            self.load_rx = Some(start_load(path.clone(), sync_root));
            self.nav_stack.push(path);
            ctx.request_repaint(); // show spinner on very next frame
        }
    }
}

// ── Section renderers ─────────────────────────────────────────────────────────

impl StatusPanel {
    fn render_user_card(&self, ui: &mut egui::Ui) {
        egui::Frame::default()
            .fill(SURFACE)
            .inner_margin(egui::Margin { left: 18.0, right: 18.0, top: 16.0, bottom: 16.0 })
            .show(ui, |ui| {
                let total_w = ui.available_width();
                ui.set_width(total_w);

                ui.horizontal(|ui| {
                    // Avatar circle with initials (fixed 46×46)
                    let (av_rect, _) =
                        ui.allocate_exact_size(egui::vec2(46.0, 46.0), egui::Sense::hover());
                    ui.painter().circle_filled(av_rect.center(), 23.0, ACCENT_DIM);
                    ui.painter().text(
                        av_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &self.initials,
                        egui::FontId::proportional(17.0),
                        egui::Color32::WHITE,
                    );

                    ui.add_space(12.0);

                    // Explicitly constrain text area so labels wrap correctly
                    let text_w = (total_w - 46.0 - 12.0).max(80.0);
                    ui.vertical(|ui| {
                        ui.set_width(text_w);
                        // Full name
                        ui.label(
                            egui::RichText::new(&self.display_name)
                                .size(15.0)
                                .color(TEXT_PRIMARY)
                                .strong(),
                        );
                        ui.add_space(3.0);
                        // username · role (wraps naturally within text_w)
                        ui.label(
                            egui::RichText::new(format!(
                                "{}  ·  {}",
                                self.username, self.role_label
                            ))
                            .size(12.0)
                            .color(TEXT_SECONDARY),
                        );
                        ui.add_space(2.0);
                        // installed version
                        ui.label(
                            egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .size(10.5)
                                .color(TEXT_SECONDARY.linear_multiply(0.6)),
                        );
                    });
                });
            });
    }

    fn render_status(&self, ui: &mut egui::Ui) {
        egui::Frame::default()
            .inner_margin(egui::Margin { left: 18.0, right: 18.0, top: 18.0, bottom: 18.0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                match &self.progress.phase.clone() {
                    SyncPhase::Idle => self.render_idle(ui),
                    SyncPhase::Syncing => self.render_syncing(ui),
                    SyncPhase::Error(e) => self.render_error(ui, e),
                }
            });
    }

    fn render_idle(&self, ui: &mut egui::Ui) {
        let p = &self.progress;

        // Status badge
        ui.horizontal(|ui| {
            let (dot, _) =
                ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 5.0, SUCCESS_COLOR);
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Up to date")
                    .size(15.0)
                    .color(SUCCESS_COLOR)
                    .strong(),
            );
        });

        if p.files_synced > 0 {
            ui.add_space(14.0);
            egui::Frame::default()
                .fill(SURFACE_VARIANT)
                .rounding(10.0)
                .inner_margin(egui::Margin { left: 14.0, right: 14.0, top: 12.0, bottom: 12.0 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("Last sync")
                            .size(11.0)
                            .color(TEXT_SECONDARY),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if p.files_synced > 0 {
                            ui.label(
                                egui::RichText::new(format!("↻  {} synced", p.files_synced))
                                    .size(13.0)
                                    .color(TEXT_PRIMARY),
                            );
                        }
                        if p.files_failed > 0 {
                            ui.add_space(16.0);
                            ui.label(
                                egui::RichText::new(format!("✗  {} failed", p.files_failed))
                                    .size(13.0)
                                    .color(ERROR_COLOR),
                            );
                        }
                    });
                });
        }

        if p.paused {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("⏸  Sync paused").size(13.0).color(TEXT_SECONDARY));
            });
        }
    }

    fn render_syncing(&self, ui: &mut egui::Ui) {
        let p = &self.progress;

        // Status badge with spinner
        ui.horizontal(|ui| {
            ui.spinner();
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Syncing files…")
                    .size(15.0)
                    .color(ACCENT)
                    .strong(),
            );
        });

        if !p.current_folder.is_empty() {
            ui.add_space(16.0);

            // Current operation card
            egui::Frame::default()
                .fill(CARD_BG)
                .rounding(10.0)
                .stroke(egui::Stroke::new(1.0, DIVIDER))
                .inner_margin(egui::Margin::same(14.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());

                    // Current folder
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("📁").size(14.0));
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&p.current_folder)
                                .size(13.0)
                                .color(FOLDER_COLOR)
                                .strong(),
                        );
                    });

                    // Current file being downloaded
                    if !p.current_file.is_empty() {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new("📄").size(12.0));
                            ui.add_space(4.0);
                            let fname = if p.current_file.len() > 34 {
                                format!("{}…", &p.current_file[..32])
                            } else {
                                p.current_file.clone()
                            };
                            ui.label(
                                egui::RichText::new(fname).size(12.0).color(TEXT_PRIMARY),
                            );
                        });
                    }

                    // Progress bar + counters
                    if p.files_total > 0 {
                        ui.add_space(12.0);

                        let pct = (p.files_done as f32 / p.files_total as f32).clamp(0.0, 1.0);
                        let bar_w = ui.available_width();
                        let bar_h = 5.0;
                        let (bar_rect, _) = ui.allocate_exact_size(
                            egui::vec2(bar_w, bar_h),
                            egui::Sense::hover(),
                        );
                        // Track
                        ui.painter().rect_filled(bar_rect, 3.0, BAR_BG);
                        // Fill
                        if pct > 0.0 {
                            let fill_w = (bar_w * pct).max(bar_h);
                            let fill_rect = egui::Rect::from_min_size(
                                bar_rect.min,
                                egui::vec2(fill_w, bar_h),
                            );
                            ui.painter().rect_filled(fill_rect, 3.0, ACCENT);
                        }

                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} / {} files",
                                    p.files_done, p.files_total
                                ))
                                .size(11.0)
                                .color(TEXT_SECONDARY),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!("{:.0}%", pct * 100.0))
                                            .size(11.0)
                                            .color(ACCENT),
                                    );
                                },
                            );
                        });
                    }
                });
        }

        // Running totals row
        if p.files_synced > 0 || p.files_failed > 0 {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if p.files_synced > 0 {
                    ui.label(
                        egui::RichText::new(format!("↻  {}", p.files_synced))
                            .size(12.0)
                            .color(SUCCESS_COLOR),
                    );
                    ui.label(egui::RichText::new(" synced").size(12.0).color(TEXT_SECONDARY));
                    ui.add_space(12.0);
                }
                if p.files_failed > 0 {
                    ui.label(
                        egui::RichText::new(format!("✗  {}", p.files_failed))
                            .size(12.0)
                            .color(ERROR_COLOR),
                    );
                    ui.label(egui::RichText::new(" failed").size(12.0).color(TEXT_SECONDARY));
                }
            });
        }

        // Error message card
        if let Some(err) = &p.last_error {
            ui.add_space(12.0);
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_premultiplied(80, 20, 20, 200))
                .rounding(8.0)
                .inner_margin(egui::Margin { left: 12.0, right: 12.0, top: 8.0, bottom: 8.0 })
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(format!("⚠  {err}"))
                            .size(11.0)
                            .color(ERROR_COLOR),
                    );
                });
        }
    }

    fn render_error(&self, ui: &mut egui::Ui, error_msg: &str) {
        let p = &self.progress;

        ui.horizontal(|ui| {
            let (dot, _) =
                ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 5.0, ERROR_COLOR);
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Sync error")
                    .size(15.0)
                    .color(ERROR_COLOR)
                    .strong(),
            );
        });

        ui.add_space(12.0);
        egui::Frame::default()
            .fill(SURFACE_VARIANT)
            .rounding(10.0)
            .inner_margin(egui::Margin::same(14.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(error_msg).size(12.0).color(ERROR_COLOR));
            });

        if p.files_done > 0 {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(format!(
                    "{} file{} completed before error",
                    p.files_done,
                    if p.files_done == 1 { "" } else { "s" }
                ))
                .size(12.0)
                .color(TEXT_SECONDARY),
            );
        }
    }

    /// Render synced folder cards. Clicking a card switches to the Files tab.
    fn render_folders(&mut self, ui: &mut egui::Ui) {
        egui::Frame::default()
            .inner_margin(egui::Margin { left: 18.0, right: 18.0, top: 14.0, bottom: 18.0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());

                let n = self.selected_folders.len();
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Synced Folders")
                            .size(11.5)
                            .color(TEXT_SECONDARY)
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(if n == 0 {
                                "None configured".to_string()
                            } else {
                                format!("{} folder{}", n, if n == 1 { "" } else { "s" })
                            })
                            .size(11.0)
                            .color(TEXT_SECONDARY),
                        );
                    });
                });

                if n == 0 {
                    ui.add_space(10.0);
                    egui::Frame::default()
                        .fill(SURFACE_VARIANT)
                        .rounding(8.0)
                        .inner_margin(egui::Margin::same(14.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                egui::RichText::new(
                                    "Open Manage Folders to select which folders to sync.",
                                )
                                .size(12.0)
                                .color(TEXT_SECONDARY),
                            );
                        });
                    return;
                }

                ui.add_space(10.0);

                for (idx, sel) in self.selected_folders.iter().enumerate() {
                    let is_syncing = self.progress.phase == SyncPhase::Syncing
                        && self.progress.current_folder == sel.name;
                    let local_path = sel_local_path(&self.sync_folder, &sel.path);
                    let exists = local_path.exists();

                    let frame_resp = self.render_folder_row(ui, sel, is_syncing, exists);

                    // Sense click on the entire card
                    let interact = ui
                        .interact(
                            frame_resp.response.rect,
                            ui.id().with(("fc", idx as u32)),
                            egui::Sense::click(),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand);

                    if interact.clicked() && !self.sync_folder.is_empty() {
                        // Switch to Files tab and navigate to the specific folder's local path
                        let root = std::path::PathBuf::from(&self.sync_folder);
                        let target = sel_local_path(&self.sync_folder, &sel.path);
                        let nav_to = if target.exists() { target } else { root.clone() };
                        self.thumbnail_cache.clear();
                        self.thumbnail_rx.clear();
                        self.thumbnail_loading.clear();
                        self.nav_stack.clear();
                        self.nav_stack.push(root.clone());
                        // Push intermediate path segments
                        self.dir_entries.clear();
                        self.search_query.clear();
                        self.is_loading = true;
                        if nav_to != root {
                            self.load_rx = Some(start_load(nav_to.clone(), root.clone()));
                            self.nav_stack.push(nav_to);
                        } else {
                            self.load_rx = Some(start_load(root.clone(), root.clone()));
                        }
                        self.active_tab = BrowserTab::Files;
                    }

                    ui.add_space(6.0);
                }
            });
    }

    fn render_folder_row(
        &self,
        ui: &mut egui::Ui,
        sel: &FolderSelection,
        is_syncing: bool,
        exists: bool,
    ) -> egui::InnerResponse<()> {
        let card_fill = if is_syncing {
            egui::Color32::from_rgba_premultiplied(30, 60, 100, 200)
        } else {
            SURFACE_VARIANT
        };
        let card_stroke = if is_syncing {
            egui::Stroke::new(1.5, ACCENT)
        } else {
            egui::Stroke::new(1.0, DIVIDER)
        };

        egui::Frame::default()
            .fill(card_fill)
            .rounding(8.0)
            .stroke(card_stroke)
            .inner_margin(egui::Margin { left: 12.0, right: 12.0, top: 10.0, bottom: 10.0 })
            .show(ui, |ui| {
                let w = ui.available_width();
                ui.set_width(w);
                ui.horizontal(|ui| {
                    let icon = "📁";
                    ui.label(egui::RichText::new(icon).size(15.0));
                    ui.add_space(6.0);

                    ui.vertical(|ui| {
                        ui.set_width((w - 140.0).max(80.0));
                        ui.label(
                            egui::RichText::new(&sel.name)
                                .size(12.5)
                                .color(TEXT_PRIMARY)
                                .strong(),
                        );
                        if sel.path != sel.name && !sel.path.is_empty() {
                            let path_short = path_truncate(&sel.path, 36);
                            ui.label(
                                egui::RichText::new(path_short)
                                    .size(10.5)
                                    .color(TEXT_SECONDARY),
                            );
                        }
                    });

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            // Navigate arrow (only if local folder exists)
                            if exists {
                                ui.label(
                                    egui::RichText::new("›")
                                        .size(18.0)
                                        .color(TEXT_SECONDARY),
                                );
                                ui.add_space(4.0);
                            }
                            let (badge_txt, badge_col) = folder_badge(sel, is_syncing);
                            egui::Frame::default()
                                .fill(egui::Color32::from_rgba_premultiplied(20, 20, 30, 180))
                                .rounding(12.0)
                                .inner_margin(egui::Margin {
                                    left: 8.0,
                                    right: 8.0,
                                    top: 3.0,
                                    bottom: 3.0,
                                })
                                .show(ui, |ui| {
                                    if is_syncing {
                                        ui.spinner();
                                        ui.add_space(2.0);
                                    }
                                    ui.label(
                                        egui::RichText::new(badge_txt)
                                            .size(10.5)
                                            .color(badge_col),
                                    );
                                });
                        },
                    );
                });
            })
    }

    // ── File browser ──────────────────────────────────────────────────────────

    /// Renders the breadcrumb + back button + view mode toggles. Returns true if back was clicked.
    fn render_browser_header(&mut self, ui: &mut egui::Ui) -> bool {
        let mut back_clicked = false;
        let at_root = self.nav_stack.len() <= 1;

        egui::Frame::default()
            .fill(SURFACE)
            .inner_margin(egui::Margin { left: 14.0, right: 10.0, top: 8.0, bottom: 8.0 })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // ← Back — painter chevron (avoids missing-glyph box)
                    let back_col = if at_root { TEXT_DISABLED } else { ACCENT };
                    let (back_rect, back_resp) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
                    let cx = back_rect.center().x + 1.5;
                    let cy = back_rect.center().y;
                    let bs = egui::Stroke::new(1.6, back_col);
                    ui.painter().line_segment([egui::pos2(cx, cy - 5.0), egui::pos2(cx - 4.5, cy)], bs);
                    ui.painter().line_segment([egui::pos2(cx - 4.5, cy), egui::pos2(cx, cy + 5.0)], bs);
                    if !at_root && back_resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        back_clicked = true;
                    }

                    // Parent folder name next to back chevron
                    if !at_root {
                        if let Some(parent) = self.nav_stack.len().checked_sub(2)
                            .and_then(|i| self.nav_stack.get(i))
                        {
                            let pname = parent.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("↑");
                            if ui.add(egui::Button::new(
                                    egui::RichText::new(pname).size(11.0).color(TEXT_SECONDARY))
                                .fill(egui::Color32::TRANSPARENT)
                                .frame(false))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text("Go to parent folder")
                                .clicked()
                            {
                                back_clicked = true;
                            }
                        }
                    }

                    ui.add_space(4.0);
                    let sep = ui.allocate_space(egui::vec2(1.0, 16.0)).1;
                    ui.painter().rect_filled(sep, 0.0, DIVIDER);
                    ui.add_space(8.0);

                    // Breadcrumb (takes remaining space minus view-toggle buttons)
                    let sync_base = std::path::PathBuf::from(&self.sync_folder);
                    let current = self.nav_stack.last().cloned().unwrap_or_else(|| sync_base.clone());
                    let rel = current.strip_prefix(&sync_base).unwrap_or(&current);
                    let root_name = sync_base.file_name().and_then(|n| n.to_str()).unwrap_or("CloudFiles");

                    ui.label(egui::RichText::new(root_name).size(11.5).color(TEXT_SECONDARY));
                    for comp in rel.components() {
                        if let Some(s) = comp.as_os_str().to_str() {
                            ui.label(egui::RichText::new(" › ").size(11.0).color(DIVIDER));
                            ui.label(egui::RichText::new(s).size(11.5).color(TEXT_PRIMARY).strong());
                        }
                    }

                });
            });

        back_clicked
    }

    /// Renders directory entries. Returns Some(path) if a sub-folder was clicked.
    /// Opens files directly with the OS default app.
    /// Dispatches to List / Grid / Details view based on `self.view_mode`.
    fn render_browser_entries(&mut self, ui: &mut egui::Ui) -> Option<std::path::PathBuf> {
        let mut navigate_to: Option<std::path::PathBuf> = None;

        let q = self.search_query.trim().to_lowercase();
        let visible_count = self.dir_entries.iter()
            .filter(|e| e.name != "..")
            .filter(|e| q.is_empty() || e.name.to_lowercase().contains(&q))
            .count();

        if visible_count == 0 {
            egui::Frame::default()
                .inner_margin(egui::Margin { left: 18.0, right: 18.0, top: 24.0, bottom: 24.0 })
                .show(ui, |ui| {
                    let msg = if q.is_empty() { "Empty folder" } else { "No matching files" };
                    ui.label(egui::RichText::new(msg).size(13.0).color(TEXT_SECONDARY));
                });
            return None;
        }

        // Dispatch async thumbnail decoding for images not yet queued or cached.
        // image::open() is blocking — doing it on the UI thread freezes the panel.
        // Limit to 4 spawns per frame: spawning many OS threads at once (CreateThread)
        // blocks the main thread on Windows for tens of milliseconds each.
        let to_enqueue: Vec<std::path::PathBuf> = self.dir_entries.iter()
            .filter(|e| {
                is_image_file(&e.name)
                    && !self.thumbnail_cache.contains_key(&e.path)
                    && !self.thumbnail_loading.contains(&e.path)
            })
            .map(|e| e.path.clone())
            .take(4) // max 4 new threads per frame — rest dispatched on subsequent repaints
            .collect();
        for p in to_enqueue {
            self.thumbnail_loading.insert(p.clone());
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result = load_color_image(&p);
                let _ = tx.send((p, result));
            });
            self.thumbnail_rx.push(rx);
        }

        match self.view_mode {
            // ── List view ─────────────────────────────────────────────────────
            ViewMode::List => {
                let first_file_idx = self.dir_entries.iter().position(|e| !e.is_dir);

                for idx in 0..self.dir_entries.len() {
                    let is_img  = is_image_file(&self.dir_entries[idx].name);
                    let is_dir  = self.dir_entries[idx].is_dir;
                    let name    = self.dir_entries[idx].name.clone();
                    let path    = self.dir_entries[idx].path.clone();
                    let size_b  = self.dir_entries[idx].size_bytes;
                    if name == ".." { continue; }
                    if !q.is_empty() && !name.to_lowercase().contains(&q) { continue; }

                    if Some(idx) == first_file_idx && q.is_empty() {
                        let r = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                        ui.painter().rect_filled(r, 0.0, DIVIDER);
                    }

                    let row_id = ui.id().with(("bl", idx as u32));
                    // Immediate hover: check current pointer position against estimated row rect
                    let est_h = if is_img { 52.0 } else { 46.0 };
                    let approx_rect = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::vec2(ui.available_width(), est_h),
                    );
                    let is_hovered = ui.ctx().pointer_hover_pos()
                        .map(|p| approx_rect.contains(p)).unwrap_or(false);

                    let cached_tex: Option<egui::load::SizedTexture> = if is_img {
                        self.thumbnail_cache.get(&path)
                            .and_then(|o| o.as_ref())
                            .map(|t| egui::load::SizedTexture::new(t.id(), t.size_vec2()))
                    } else { None };

                    let v_margin = if is_img { 5.0 } else { 8.0 };
                    let row_resp = egui::Frame::default()
                        .fill(if is_hovered { SURFACE_VARIANT } else { egui::Color32::TRANSPARENT })
                        .inner_margin(egui::Margin {
                            left: 14.0, right: 12.0, top: v_margin, bottom: v_margin,
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                if let Some(sized) = cached_tex {
                                    ui.add(egui::Image::new(egui::ImageSource::Texture(sized))
                                        .max_size(egui::vec2(40.0, 40.0)).rounding(4.0));
                                } else if is_img {
                                    let (r, _) = ui.allocate_exact_size(
                                        egui::vec2(40.0, 40.0), egui::Sense::hover());
                                    ui.painter().rect_filled(r, 4.0, SURFACE_VARIANT);
                                } else if is_dir {
                                    let (fr, _) = ui.allocate_exact_size(egui::vec2(32.0, 28.0), egui::Sense::hover());
                                    draw_folder_icon(ui.painter(), fr.shrink(2.0));
                                } else {
                                    draw_file_type_icon(ui, &name, 30.0);
                                }
                                ui.add_space(10.0);

                                let name_col = if is_dir { FOLDER_COLOR } else { TEXT_PRIMARY };

                                ui.vertical(|ui| {
                                    ui.label(egui::RichText::new(&name).size(12.5).color(name_col));
                                    if !is_dir && size_b > 0 {
                                        ui.label(egui::RichText::new(format_size(size_b))
                                            .size(10.5).color(TEXT_SECONDARY));
                                    }
                                });

                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if is_dir {
                                        ui.label(egui::RichText::new("›").size(16.0).color(TEXT_SECONDARY));
                                    }
                                });
                            });
                        });

                    let tooltip = if is_dir {
                        name.clone()
                    } else if size_b > 0 {
                        format!("{}\n{}", name, format_size(size_b))
                    } else {
                        name.clone()
                    };
                    let interact = ui.interact(row_resp.response.rect, row_id, egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(tooltip);
                    if interact.clicked() {
                        if is_dir { navigate_to = Some(path); } else { let _ = open::that(&path); }
                    }
                }
            }

            // ── Grid view ─────────────────────────────────────────────────────
            ViewMode::Grid => {
                let tile_w = 86.0_f32;
                let avail  = ui.available_width();
                let cols   = ((avail - 16.0) / (tile_w + 6.0)).floor().max(2.0) as usize;

                // Collect entry data (skip ".." — use back button instead; apply search filter)
                let entries: Vec<(usize, String, std::path::PathBuf, bool, bool)> =
                    self.dir_entries.iter().enumerate()
                        .filter(|(_, e)| e.name != "..")
                        .filter(|(_, e)| q.is_empty() || e.name.to_lowercase().contains(&q))
                        .map(|(i, e)| (i, e.name.clone(), e.path.clone(), e.is_dir, is_image_file(&e.name)))
                        .collect();

                egui::Frame::default()
                    .inner_margin(egui::Margin { left: 8.0, right: 8.0, top: 8.0, bottom: 8.0 })
                    .show(ui, |ui| {
                        ui.set_width(avail);
                        let row_count = (entries.len() + cols - 1) / cols;
                        for row_i in 0..row_count {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                for col_i in 0..cols {
                                    let ei = row_i * cols + col_i;
                                    if ei >= entries.len() { break; }
                                    let (idx, ref name, ref path, is_dir, is_img) = entries[ei];

                                    let cached_tex: Option<egui::load::SizedTexture> = if is_img {
                                        self.thumbnail_cache.get(path)
                                            .and_then(|o| o.as_ref())
                                            .map(|t| egui::load::SizedTexture::new(t.id(), t.size_vec2()))
                                    } else { None };

                                    let tile_id = ui.id().with(("gt", idx as u32));
                                    let was_hov = ui.ctx().data(|d| d.get_temp::<bool>(tile_id)).unwrap_or(false);
                                    let tile_fill = if was_hov { SURFACE_VARIANT } else { egui::Color32::TRANSPARENT };

                                    let tile_resp = egui::Frame::default()
                                        .fill(tile_fill)
                                        .rounding(8.0)
                                        .stroke(egui::Stroke::new(1.0,
                                            if was_hov { DIVIDER } else { egui::Color32::TRANSPARENT }))
                                        .inner_margin(egui::Margin::same(6.0))
                                        .show(ui, |ui| {
                                            ui.set_width(tile_w);
                                            ui.set_min_height(104.0);
                                            ui.vertical_centered(|ui| {
                                                ui.add_space(4.0);
                                                if let Some(sized) = cached_tex {
                                                    ui.add(egui::Image::new(egui::ImageSource::Texture(sized))
                                                        .max_size(egui::vec2(48.0, 48.0)).rounding(6.0));
                                                } else if is_img {
                                                    let (r, _) = ui.allocate_exact_size(
                                                        egui::vec2(48.0, 48.0), egui::Sense::hover());
                                                    ui.painter().rect_filled(r, 6.0, SURFACE_VARIANT);
                                                } else if is_dir {
                                                    let (r, _) = ui.allocate_exact_size(
                                                        egui::vec2(48.0, 48.0), egui::Sense::hover());
                                                    draw_folder_icon(ui.painter(), r.shrink(5.0));
                                                } else {
                                                    draw_file_type_icon(ui, name, 48.0);
                                                }
                                                ui.add_space(6.0);
                                                let nc = if is_dir { FOLDER_COLOR } else { TEXT_PRIMARY };
                                                ui.label(egui::RichText::new(truncate_name(name, 14))
                                                    .size(10.5).color(nc));
                                            });
                                        });

                                    let inter = ui.interact(
                                        tile_resp.response.rect, tile_id, egui::Sense::click())
                                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .on_hover_text(name.as_str());
                                    ui.ctx().data_mut(|d| d.insert_temp(tile_id, inter.hovered()));
                                    if inter.clicked() {
                                        if is_dir { navigate_to = Some(path.clone()); }
                                        else { let _ = open::that(path.as_path()); }
                                    }
                                }
                            });
                            ui.add_space(6.0);
                        }
                    });
            }

            // ── Details view ──────────────────────────────────────────────────
            ViewMode::Details => {
                // Column header row
                egui::Frame::default()
                    .fill(SURFACE)
                    .inner_margin(egui::Margin { left: 14.0, right: 14.0, top: 5.0, bottom: 5.0 })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let _ = ui.allocate_exact_size(egui::vec2(32.0, 1.0), egui::Sense::hover());
                            ui.add_space(8.0);
                            ui.label(egui::RichText::new("Name").size(10.5).color(TEXT_SECONDARY));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(egui::RichText::new("Modified").size(10.5).color(TEXT_SECONDARY));
                                ui.add_space(16.0);
                                ui.label(egui::RichText::new("Size").size(10.5).color(TEXT_SECONDARY));
                                ui.add_space(8.0);
                            });
                        });
                    });
                let hr = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                ui.painter().rect_filled(hr, 0.0, DIVIDER);

                for idx in 0..self.dir_entries.len() {
                    let is_img  = is_image_file(&self.dir_entries[idx].name);
                    let is_dir  = self.dir_entries[idx].is_dir;
                    let name    = self.dir_entries[idx].name.clone();
                    let path    = self.dir_entries[idx].path.clone();
                    let size_b  = self.dir_entries[idx].size_bytes;
                    let mod_s   = self.dir_entries[idx].modified_secs;
                    if name == ".." { continue; }
                    if !q.is_empty() && !name.to_lowercase().contains(&q) { continue; }

                    let row_id = ui.id().with(("dd", idx as u32));
                    let approx_rect = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::vec2(ui.available_width(), 34.0),
                    );
                    let is_hovered_d = ui.ctx().pointer_hover_pos()
                        .map(|p| approx_rect.contains(p)).unwrap_or(false);

                    let cached_tex: Option<egui::load::SizedTexture> = if is_img {
                        self.thumbnail_cache.get(&path)
                            .and_then(|o| o.as_ref())
                            .map(|t| egui::load::SizedTexture::new(t.id(), t.size_vec2()))
                    } else { None };

                    let row_resp = egui::Frame::default()
                        .fill(if is_hovered_d { SURFACE_VARIANT } else { egui::Color32::TRANSPARENT })
                        .inner_margin(egui::Margin {
                            left: 14.0, right: 14.0, top: 5.0, bottom: 5.0,
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                // Small icon (24×24 area)
                                if let Some(sized) = cached_tex {
                                    ui.add(egui::Image::new(egui::ImageSource::Texture(sized))
                                        .max_size(egui::vec2(24.0, 24.0)).rounding(3.0));
                                } else if is_dir {
                                    let (fr, _) = ui.allocate_exact_size(egui::vec2(24.0, 22.0), egui::Sense::hover());
                                    draw_folder_icon(ui.painter(), fr.shrink(2.0));
                                } else {
                                    draw_file_type_icon(ui, &name, 24.0);
                                }
                                ui.add_space(8.0);

                                let name_col = if is_dir { FOLDER_COLOR } else { TEXT_PRIMARY };
                                ui.label(egui::RichText::new(&name).size(12.0).color(name_col));

                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if let Some(secs) = mod_s {
                                        ui.label(egui::RichText::new(format_modified(secs))
                                            .size(10.5).color(TEXT_SECONDARY));
                                    }
                                    ui.add_space(16.0);
                                    if !is_dir && size_b > 0 {
                                        ui.label(egui::RichText::new(format_size(size_b))
                                            .size(10.5).color(TEXT_SECONDARY));
                                    }
                                    ui.add_space(8.0);
                                });
                            });
                        });

                    let tooltip = if is_dir {
                        name.clone()
                    } else if size_b > 0 {
                        format!("{}\n{}", name, format_size(size_b))
                    } else {
                        name.clone()
                    };
                    let interact = ui.interact(row_resp.response.rect, row_id, egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(tooltip);
                    if interact.clicked() {
                        if is_dir { navigate_to = Some(path); } else { let _ = open::that(&path); }
                    }

                    // Subtle row separator
                    let sr = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                    ui.painter().rect_filled(sr, 0.0,
                        egui::Color32::from_rgba_premultiplied(255, 255, 255, 6));
                }
            }
        }

        navigate_to
    }

    // ── Activity tab ──────────────────────────────────────────────────────────

    fn render_activity(&self, ui: &mut egui::Ui) {
        egui::Frame::default()
            .inner_margin(egui::Margin { left: 18.0, right: 18.0, top: 14.0, bottom: 18.0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());

                // Header row
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Recent Activity")
                            .size(11.5)
                            .color(TEXT_SECONDARY)
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!("{} entries", self.progress.activity_log.len()))
                                .size(10.5)
                                .color(TEXT_DISABLED),
                        );
                    });
                });
                ui.add_space(12.0);

                if self.progress.activity_log.is_empty() {
                    // Empty state
                    ui.vertical_centered(|ui| {
                        ui.add_space(32.0);
                        ui.label(egui::RichText::new("🕐").size(36.0));
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("No recent activity")
                                .size(13.0)
                                .color(TEXT_SECONDARY),
                        );
                        ui.label(
                            egui::RichText::new("Downloaded files will appear here.")
                                .size(11.0)
                                .color(TEXT_DISABLED),
                        );
                    });
                    return;
                }

                for entry in &self.progress.activity_log {
                    egui::Frame::default()
                        .fill(CARD_BG)
                        .rounding(8.0)
                        .inner_margin(egui::Margin { left: 12.0, right: 12.0, top: 8.0, bottom: 8.0 })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                // File type icon
                                let (col, icon) = file_type_meta(&entry.file_name);
                                ui.label(egui::RichText::new(&icon).size(18.0).color(col));
                                ui.add_space(6.0);

                                ui.vertical(|ui| {
                                    // File name + action badge on the same row
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(truncate_name(&entry.file_name, 32))
                                                .size(12.0)
                                                .color(TEXT_PRIMARY)
                                                .strong(),
                                        );
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(
                                                egui::RichText::new(&entry.action)
                                                    .size(10.0)
                                                    .color(SUCCESS_COLOR),
                                            );
                                        });
                                    });

                                    // Folder path + size + time-ago
                                    ui.horizontal(|ui| {
                                        if !entry.folder.is_empty() {
                                            ui.label(
                                                egui::RichText::new(path_truncate(&entry.folder, 28))
                                                    .size(10.5)
                                                    .color(TEXT_SECONDARY),
                                            );
                                            ui.label(
                                                egui::RichText::new("·")
                                                    .size(10.5)
                                                    .color(TEXT_DISABLED),
                                            );
                                        }
                                        if entry.size_bytes > 0 {
                                            ui.label(
                                                egui::RichText::new(format_size(entry.size_bytes))
                                                    .size(10.5)
                                                    .color(TEXT_SECONDARY),
                                            );
                                            ui.label(
                                                egui::RichText::new("·")
                                                    .size(10.5)
                                                    .color(TEXT_DISABLED),
                                            );
                                        }
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(
                                                egui::RichText::new(format_modified(entry.timestamp_secs))
                                                    .size(10.5)
                                                    .color(TEXT_DISABLED),
                                            );
                                        });
                                    });
                                });
                            });
                        });
                    ui.add_space(6.0);
                }
            });
    }
}

// ── Folder list helpers ────────────────────────────────────────────────────────

fn folder_badge(_sel: &FolderSelection, is_syncing: bool) -> (&'static str, egui::Color32) {
    if is_syncing {
        ("Syncing", ACCENT)
    } else {
        ("↻ Synced", SUCCESS_COLOR)
    }
}

/// Truncate a path string keeping the end (most specific part visible).
fn path_truncate(path: &str, max_chars: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= max_chars {
        path.to_string()
    } else {
        let tail: String = chars[chars.len() - (max_chars.saturating_sub(1))..].iter().collect();
        format!("…{tail}")
    }
}

/// Build the local filesystem path from sync_folder + the visual selection path.
/// Mirrors the same logic used by the sync engine.
fn sel_local_path(sync_folder: &str, sel_path: &str) -> std::path::PathBuf {
    let base = std::path::PathBuf::from(sync_folder);
    sel_path.split(" / ").fold(base, |p, c| p.join(c.trim()))
}

/// Read a directory and return a sorted list: directories first, then files.
/// Skips hidden entries and desktop.ini.
fn load_dir_entries(path: &std::path::Path, root: &std::path::Path) -> Vec<DirEntry> {
    let mut dirs: Vec<DirEntry> = Vec::new();
    let mut files: Vec<DirEntry> = Vec::new();
    let Ok(rd) = std::fs::read_dir(path) else {
        return vec![];
    };
    for entry in rd.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        let is_dir = meta.is_dir();
        let size_bytes = if is_dir { 0 } else { meta.len() };
        let modified_secs = meta.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let de = DirEntry { name, path: entry.path(), is_dir, size_bytes, modified_secs };
        if is_dir { dirs.push(de); } else { files.push(de); }
    }
    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    // Files: most recently modified first; fall back to alphabetical when mtime is equal or missing.
    files.sort_by(|a, b| {
        b.modified_secs.cmp(&a.modified_secs)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    // Prepend ".." only if we are strictly inside the root (not at the root itself)
    let mut result = Vec::new();
    if path != root {
        if let Some(parent) = path.parent() {
            if parent.exists() {
                result.push(DirEntry {
                    name: "..".to_string(),
                    path: parent.to_path_buf(),
                    is_dir: true,
                    size_bytes: 0,
                    modified_secs: None,
                });
            }
        }
    }
    result.extend(dirs);
    result.extend(files);
    result
}

/// Spawn a background thread to load directory entries, returning a receiver.
fn start_load(path: std::path::PathBuf, root: std::path::PathBuf) -> std::sync::mpsc::Receiver<Vec<DirEntry>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let entries = load_dir_entries(&path, &root);
        let _ = tx.send(entries);
    });
    rx
}

/// Returns true for image formats supported by the `image` crate features enabled.
fn is_image_file(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp")
}

/// Decode an image from disk and resize to max 96×96 pixels.
/// Pure CPU work — safe to call from a background thread.
/// Returns None on any error (unsupported format, missing file, decode failure).
fn load_color_image(path: &std::path::Path) -> Option<egui::ColorImage> {
    let img = image::open(path).ok()?;
    let img = img.thumbnail(96, 96);
    let img_rgba = img.to_rgba8();
    let (w, h) = (img_rgba.width() as usize, img_rgba.height() as usize);
    let pixels = img_rgba.into_raw();
    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels))
}

/// Draw a Windows-style folder icon (tab + body) into `rect` using the painter.
fn draw_folder_icon(painter: &egui::Painter, rect: egui::Rect) {
    let tab_h = rect.height() * 0.22;
    let tab_w = rect.width()  * 0.44;
    let color_tab  = egui::Color32::from_rgb(230, 170, 18);
    let color_body = egui::Color32::from_rgb(252, 196, 25);
    // Tab (top-left)
    painter.rect_filled(
        egui::Rect::from_min_max(rect.min, egui::pos2(rect.min.x + tab_w, rect.min.y + tab_h + 1.0)),
        egui::Rounding { nw: 3.0, ne: 5.0, sw: 0.0, se: 0.0 },
        color_tab,
    );
    // Body
    painter.rect_filled(
        egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.min.y + tab_h), rect.max),
        egui::Rounding { nw: 0.0, ne: 3.0, sw: 3.0, se: 3.0 },
        color_body,
    );
}

/// Draw a view-mode icon (list/grid/details) into `rect` using the painter.
/// `kind`: 0 = Details (dot+lines), 1 = Grid (2×2), 2 = List (3 lines).
fn draw_view_mode_icon(painter: &egui::Painter, rect: egui::Rect, kind: u8, col: egui::Color32, bg: egui::Color32) {
    painter.rect_filled(rect, 4.0, bg);
    let pad = 4.5;
    let inner = egui::Rect::from_min_max(
        egui::pos2(rect.min.x + pad, rect.min.y + pad),
        egui::pos2(rect.max.x - pad, rect.max.y - pad),
    );
    let stroke = egui::Stroke::new(1.2, col);
    match kind {
        0 => { // Details: dot + line × 3
            for i in 0..3 {
                let y = inner.min.y + inner.height() * (i as f32 + 0.5) / 3.0;
                painter.circle_filled(egui::pos2(inner.min.x + 1.5, y), 1.5, col);
                painter.line_segment(
                    [egui::pos2(inner.min.x + 5.0, y), egui::pos2(inner.max.x, y)],
                    stroke,
                );
            }
        }
        1 => { // Grid: 2×2 squares
            let gap = 1.5;
            let tw = (inner.width() - gap) / 2.0;
            let th = (inner.height() - gap) / 2.0;
            for row in 0..2 {
                for c in 0..2 {
                    let x = inner.min.x + (c as f32) * (tw + gap);
                    let y = inner.min.y + (row as f32) * (th + gap);
                    painter.rect_filled(
                        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(tw, th)),
                        1.0, col,
                    );
                }
            }
        }
        _ => { // List: 3 horizontal lines
            for i in 0..3 {
                let y = inner.min.y + inner.height() * (i as f32 + 0.5) / 3.0;
                painter.line_segment([egui::pos2(inner.min.x, y), egui::pos2(inner.max.x, y)], stroke);
            }
        }
    }
}

/// Returns (background color, short type label) for a filename based on its extension.
/// Used by `draw_file_type_icon`.
fn file_type_meta(name: &str) -> (egui::Color32, String) {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "pdf"
            => (egui::Color32::from_rgb(200, 55,  55), "PDF".into()),
        "doc" | "docx" | "odt" | "rtf"
            => (egui::Color32::from_rgb( 41, 98, 178), "DOC".into()),
        "xls" | "xlsx" | "csv" | "ods"
            => (egui::Color32::from_rgb( 28,120,  70), "XLS".into()),
        "ppt" | "pptx" | "odp"
            => (egui::Color32::from_rgb(200, 85,  25), "PPT".into()),
        "mp4" | "avi" | "mkv" | "mov" | "wmv" | "webm" | "flv"
            => (egui::Color32::from_rgb(110, 55, 175), "VID".into()),
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a"
            => (egui::Color32::from_rgb(170, 45, 125), "AUD".into()),
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz"
            => (egui::Color32::from_rgb(150,110,  35), "ZIP".into()),
        "rs" | "js" | "ts" | "py" | "java" | "cpp" | "c" | "go"
        | "cs" | "rb" | "php" | "swift" | "kt"
            => (egui::Color32::from_rgb( 25,130, 145), "CODE".into()),
        "json" | "xml" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "env"
            => (egui::Color32::from_rgb( 70,120, 155), "CFG".into()),
        "txt" | "md" | "log"
            => (egui::Color32::from_rgb( 80, 95, 115), "TXT".into()),
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "svg" | "webp" | "ico"
            => (egui::Color32::from_rgb(180,100,  40), "IMG".into()),
        other => {
            let lbl = if other.is_empty() {
                "FILE".to_string()
            } else {
                other.chars().take(3).collect::<String>().to_uppercase()
            };
            (egui::Color32::from_rgb(65, 75, 95), lbl)
        }
    }
}

/// Draw a colored file-type badge (rounded rect + label text) in a `size × size` area.
fn draw_file_type_icon(ui: &mut egui::Ui, name: &str, size: f32) {
    let (bg, label) = file_type_meta(name);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let pad      = size * 0.1;
    let rounding = (size * 0.2).max(3.0);
    ui.painter().rect_filled(rect.shrink(pad), rounding, bg);
    let fs = if size >= 40.0 { size * 0.22 } else { size * 0.30 };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        &label,
        egui::FontId::proportional(fs),
        egui::Color32::WHITE,
    );
}

/// Truncate a filename to `max` chars, preserving the extension when possible.
fn truncate_name(name: &str, max: usize) -> String {
    if name.chars().count() <= max { return name.to_string(); }
    if let Some(dot) = name.rfind('.') {
        let ext = &name[dot..];
        if ext.len() < max {
            let stem_max = max.saturating_sub(ext.len() + 1);
            let stem: String = name[..dot].chars().take(stem_max).collect();
            return format!("{stem}…{ext}");
        }
    }
    let t: String = name.chars().take(max - 1).collect();
    format!("{t}…")
}

/// Format a Unix timestamp (seconds) as a human-readable relative string.
fn format_modified(secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(secs);
    match age {
        0..=59        => "Just now".to_string(),
        60..=3599     => format!("{} min ago", age / 60),
        3600..=86399  => format!("{} hr ago", age / 3600),
        86400..=604799 => format!("{} d ago", age / 86400),
        _ => {
            // Approximate calendar date from epoch
            let days = secs / 86400;
            let y    = 1970u64 + days / 365;
            let doy  = days % 365;
            let m    = (doy / 30).min(11) + 1;
            let d    = doy % 30 + 1;
            format!("{:02}/{:02}/{}", m, d, y)
        }
    }
}

/// Human-readable file size.
fn format_size(bytes: u64) -> String {
    if bytes < 1_024 {
        format!("{} B", bytes)
    } else if bytes < 1_024 * 1_024 {
        format!("{:.0} KB", bytes as f64 / 1_024.0)
    } else if bytes < 1_024 * 1_024 * 1_024 {
        format!("{:.1} MB", bytes as f64 / (1_024.0 * 1_024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1_024.0 * 1_024.0 * 1_024.0))
    }
}
