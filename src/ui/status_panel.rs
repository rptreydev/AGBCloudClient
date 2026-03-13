//! iCloud-style live status panel.
//! Spawned as `--status` subprocess when the user clicks the tray icon.
//! Communicates with the sync engine via a shared progress.json file.

use std::sync::Arc;
use tracing::{error, info};

use crate::auth::AuthState;
use crate::models::cloud_file::{FolderSelection, SyncPolicy};
use crate::models::user::UserRole;
use crate::sync::progress::{read_progress_file, SyncPhase, SyncProgress};
use crate::ui::common::{
    work_area, ACCENT, ACCENT_DIM, BAR_BG, CARD_BG, DARK_BG, DIVIDER,
    ERROR_COLOR, FOLDER_COLOR, SUCCESS_COLOR, SURFACE, SURFACE_VARIANT,
    TEXT_DISABLED, TEXT_PRIMARY, TEXT_SECONDARY,
};

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
enum BrowserTab { Folders, Files }

#[derive(PartialEq, Clone, Copy)]
enum ViewMode { List, Grid, Details }

// ── Entry point ───────────────────────────────────────────────────────────────

/// Show the iCloud-style status panel positioned at the bottom-right of the screen.
/// Always calls `process::exit(0)` — never returns.
pub fn show_status_panel(auth: &AuthState, rt: &tokio::runtime::Runtime) {
    let (_, _, wa_right, wa_bottom) = work_area();
    let win_w = 340.0_f32;
    let win_h = (wa_bottom * 0.75).clamp(500.0, 720.0);
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
            UserRole::TECH => "Technician",
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
            .with_min_inner_size([300.0, 420.0])
            .with_position([pos_x, pos_y])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(false)
            .with_icon(Arc::new(crate::ui::icon::app_icon_data())),
        ..Default::default()
    };

    info!("Launching status panel at ({pos_x:.0}, {pos_y:.0}), size {win_w}x{win_h:.0}");
    let run_result = eframe::run_native(
        "AGB Cloud Client — Status",
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(StatusPanel::new(display_name, username, role_label)))
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
    /// Thumbnail cache: path → loaded texture (None = failed to load)
    thumbnail_cache: std::collections::HashMap<std::path::PathBuf, Option<egui::TextureHandle>>,
    done: bool,
}

impl StatusPanel {
    fn new(display_name: String, username: String, role_label: String) -> Self {
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
                        (BrowserTab::Folders, "Synced", "☁"),
                        (BrowserTab::Files,   "Files",  "📂"),
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
                                self.dir_entries = load_dir_entries(&root, &root);
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
                        // ── Folders tab: user card + sync status + synced folder list ──
                        egui::ScrollArea::vertical()
                            .id_salt("sp_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.render_user_card(ui);
                                let r = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                                ui.painter().rect_filled(r, 0.0, DIVIDER);
                                self.render_status(ui);
                                let r = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                                ui.painter().rect_filled(r, 0.0, DIVIDER);
                                self.render_folders(ui);
                            });
                    }
                    BrowserTab::Files => {
                        // ── Files tab: breadcrumb + view toggles + file entries ──
                        // Ensure nav_stack has at least the sync root
                        if self.nav_stack.is_empty() && !self.sync_folder.is_empty() {
                            let root = std::path::PathBuf::from(&self.sync_folder);
                            self.dir_entries = load_dir_entries(&root, &root);
                            self.nav_stack.push(root);
                        }
                        if self.render_browser_header(ui) {
                            nav_back = true;
                        }
                        let r = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
                        ui.painter().rect_filled(r, 0.0, DIVIDER);
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
            });

        // Apply navigation (must run after all panels are rendered)
        let sync_root = std::path::PathBuf::from(&self.sync_folder);
        if nav_back {
            // Don't pop past the sync root
            if self.nav_stack.len() > 1 {
                self.thumbnail_cache.clear();
                self.nav_stack.pop();
                if let Some(p) = self.nav_stack.last().cloned() {
                    self.dir_entries = load_dir_entries(&p, &sync_root);
                }
            }
        } else if let Some(path) = nav_push {
            self.thumbnail_cache.clear();
            self.dir_entries = load_dir_entries(&path, &sync_root);
            self.nav_stack.push(path);
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

        if p.files_copied > 0 || p.files_synced > 0 {
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
                        if p.files_copied > 0 {
                            ui.label(
                                egui::RichText::new(format!("↓  {} copied", p.files_copied))
                                    .size(13.0)
                                    .color(ACCENT),
                            );
                            if p.files_synced > 0 {
                                ui.add_space(16.0);
                            }
                        }
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
        if p.files_copied > 0 || p.files_synced > 0 || p.files_failed > 0 {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if p.files_copied > 0 {
                    ui.label(
                        egui::RichText::new(format!("↓  {}", p.files_copied))
                            .size(12.0)
                            .color(ACCENT),
                    );
                    ui.label(egui::RichText::new(" copied").size(12.0).color(TEXT_SECONDARY));
                    ui.add_space(12.0);
                }
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
                        self.nav_stack.clear();
                        self.nav_stack.push(root.clone());
                        // Push intermediate path segments
                        if nav_to != root {
                            self.dir_entries = load_dir_entries(&nav_to, &root);
                            self.nav_stack.push(nav_to);
                        } else {
                            self.dir_entries = load_dir_entries(&root, &root);
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
                    let icon = match &sel.policy {
                        SyncPolicy::Copy => "📋",
                        SyncPolicy::KeepSynced { .. } => "📁",
                    };
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

        if self.dir_entries.is_empty() {
            egui::Frame::default()
                .inner_margin(egui::Margin { left: 18.0, right: 18.0, top: 24.0, bottom: 24.0 })
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Empty folder").size(13.0).color(TEXT_SECONDARY));
                });
            return None;
        }

        // Pre-load thumbnails (once per path, cached)
        let ctx = ui.ctx().clone();
        let to_load: Vec<std::path::PathBuf> = self.dir_entries.iter()
            .filter(|e| is_image_file(&e.name) && !self.thumbnail_cache.contains_key(&e.path))
            .map(|e| e.path.clone())
            .collect();
        for p in to_load {
            let tex = load_thumbnail(&p, &ctx);
            self.thumbnail_cache.insert(p, tex);
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

                    if Some(idx) == first_file_idx {
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
                    let row_fill = if ui.ctx().pointer_hover_pos()
                        .map(|p| approx_rect.contains(p)).unwrap_or(false)
                    {
                        egui::Color32::from_rgba_premultiplied(255, 255, 255, 18)
                    } else {
                        egui::Color32::TRANSPARENT
                    };

                    let cached_tex: Option<egui::load::SizedTexture> = if is_img {
                        self.thumbnail_cache.get(&path)
                            .and_then(|o| o.as_ref())
                            .map(|t| egui::load::SizedTexture::new(t.id(), t.size_vec2()))
                    } else { None };

                    let v_margin = if is_img { 5.0 } else { 8.0 };
                    let row_resp = egui::Frame::default()
                        .fill(row_fill)
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
                                } else if name == ".." {
                                    let (ar, _) = ui.allocate_exact_size(egui::vec2(30.0, 28.0), egui::Sense::hover());
                                    let (acx, acy) = (ar.center().x, ar.center().y);
                                    let as_ = egui::Stroke::new(1.5, TEXT_SECONDARY);
                                    ui.painter().line_segment([egui::pos2(acx, acy + 6.0), egui::pos2(acx, acy - 6.0)], as_);
                                    ui.painter().line_segment([egui::pos2(acx - 4.0, acy - 2.0), egui::pos2(acx, acy - 6.0)], as_);
                                    ui.painter().line_segment([egui::pos2(acx, acy - 6.0), egui::pos2(acx + 4.0, acy - 2.0)], as_);
                                } else if is_dir {
                                    let (fr, _) = ui.allocate_exact_size(egui::vec2(32.0, 28.0), egui::Sense::hover());
                                    draw_folder_icon(ui.painter(), fr.shrink(2.0));
                                } else {
                                    draw_file_type_icon(ui, &name, 30.0);
                                }
                                ui.add_space(10.0);

                                let name_col = if name == ".." { TEXT_SECONDARY }
                                    else if is_dir { FOLDER_COLOR }
                                    else { TEXT_PRIMARY };
                                let display = if name == ".." {
                                    "Parent folder".to_string()
                                } else {
                                    name.clone()
                                };

                                ui.vertical(|ui| {
                                    ui.label(egui::RichText::new(&display).size(12.5).color(name_col));
                                    if !is_dir && size_b > 0 {
                                        ui.label(egui::RichText::new(format_size(size_b))
                                            .size(10.5).color(TEXT_SECONDARY));
                                    }
                                });

                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if is_dir && name != ".." {
                                        ui.label(egui::RichText::new("›").size(16.0).color(TEXT_SECONDARY));
                                    }
                                });
                            });
                        });

                    let interact = ui.interact(row_resp.response.rect, row_id, egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
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

                // Collect entry data (skip ".." — use back button instead)
                let entries: Vec<(usize, String, std::path::PathBuf, bool, bool)> =
                    self.dir_entries.iter().enumerate()
                        .filter(|(_, e)| e.name != "..")
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
                                    let tile_fill = if was_hov {
                                        SURFACE_VARIANT
                                    } else {
                                        egui::Color32::from_rgba_premultiplied(255, 255, 255, 4)
                                    };

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
                                        .on_hover_cursor(egui::CursorIcon::PointingHand);
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

                    let row_id = ui.id().with(("dd", idx as u32));
                    let approx_rect = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::vec2(ui.available_width(), 34.0),
                    );
                    let row_fill = if ui.ctx().pointer_hover_pos()
                        .map(|p| approx_rect.contains(p)).unwrap_or(false)
                    {
                        egui::Color32::from_rgba_premultiplied(255, 255, 255, 18)
                    } else {
                        egui::Color32::TRANSPARENT
                    };

                    let cached_tex: Option<egui::load::SizedTexture> = if is_img {
                        self.thumbnail_cache.get(&path)
                            .and_then(|o| o.as_ref())
                            .map(|t| egui::load::SizedTexture::new(t.id(), t.size_vec2()))
                    } else { None };

                    let row_resp = egui::Frame::default()
                        .fill(row_fill)
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
                                } else if name == ".." {
                                    let (ar, _) = ui.allocate_exact_size(egui::vec2(24.0, 22.0), egui::Sense::hover());
                                    let (acx, acy) = (ar.center().x, ar.center().y);
                                    let as_ = egui::Stroke::new(1.4, TEXT_SECONDARY);
                                    ui.painter().line_segment([egui::pos2(acx, acy + 5.0), egui::pos2(acx, acy - 5.0)], as_);
                                    ui.painter().line_segment([egui::pos2(acx - 3.5, acy - 1.5), egui::pos2(acx, acy - 5.0)], as_);
                                    ui.painter().line_segment([egui::pos2(acx, acy - 5.0), egui::pos2(acx + 3.5, acy - 1.5)], as_);
                                } else if is_dir {
                                    let (fr, _) = ui.allocate_exact_size(egui::vec2(24.0, 22.0), egui::Sense::hover());
                                    draw_folder_icon(ui.painter(), fr.shrink(2.0));
                                } else {
                                    draw_file_type_icon(ui, &name, 24.0);
                                }
                                ui.add_space(8.0);

                                let name_col = if name == ".." { TEXT_SECONDARY }
                                    else if is_dir { FOLDER_COLOR }
                                    else { TEXT_PRIMARY };
                                let display = if name == ".." {
                                    "Parent folder".to_string()
                                } else {
                                    name.clone()
                                };
                                ui.label(egui::RichText::new(&display).size(12.0).color(name_col));

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

                    let interact = ui.interact(row_resp.response.rect, row_id, egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
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
}

// ── Folder list helpers ────────────────────────────────────────────────────────

fn folder_badge(sel: &FolderSelection, is_syncing: bool) -> (&'static str, egui::Color32) {
    match &sel.policy {
        SyncPolicy::Copy => {
            if sel.completed {
                ("✓ Copied", SUCCESS_COLOR)
            } else {
                ("Pending copy", TEXT_SECONDARY)
            }
        }
        SyncPolicy::KeepSynced { .. } => {
            if is_syncing {
                ("Syncing", ACCENT)
            } else {
                ("↻ Synced", SUCCESS_COLOR)
            }
        }
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
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

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

/// Returns true for image formats supported by the `image` crate features enabled.
fn is_image_file(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp")
}

/// Load an image from disk, resize to max 96×96, and upload as an egui texture.
/// Returns None on any error (format unsupported, file missing, decode failure).
fn load_thumbnail(path: &std::path::Path, ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::open(path).ok()?;
    let img = img.thumbnail(96, 96);
    let img_rgba = img.to_rgba8();
    let (w, h) = (img_rgba.width() as usize, img_rgba.height() as usize);
    let pixels = img_rgba.into_raw();
    let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels);
    let key = path.to_string_lossy();
    Some(ctx.load_texture(key.as_ref(), color_image, egui::TextureOptions::LINEAR))
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
