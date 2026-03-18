use std::sync::Arc;
use std::sync::mpsc;
use eframe::egui;
use tracing::{error, info};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::models::FolderSelection;
use crate::sync::progress::SharedProgress;
use crate::ui::common::*;
use crate::ui::folder_tree::FolderTreeWidget;
use crate::ws::events::WsClient;

#[derive(PartialEq, Clone, Copy)]
enum Tab { Folders, General, Shortcuts }

struct SettingsApp {
    active_tab: Tab,
    sync_folder: String,
    sync_interval_secs: u64,
    auto_start: bool,
    notifications_enabled: bool,
    tree: FolderTreeWidget,
    disk_total: u64,
    disk_free: u64,
    last_disk_path: String,
    shortcut_message: String,
    shortcut_is_error: bool,
    done: bool,
    /// Set to true when Save is clicked, processed by SettingsWrapper
    pending_save: bool,
    /// Feedback message shown after save
    save_message: String,
    save_time: Option<std::time::Instant>,
    progress: SharedProgress,
}

impl SettingsApp {
    fn new(config: &AppConfig, auth: AuthState, handle: tokio::runtime::Handle, progress: SharedProgress) -> Self {
        let sync_folder = config.sync_folder.clone();
        let (disk_total, disk_free) = get_disk_space(&sync_folder).unwrap_or((0, 0));

        // Wire a WS listener so company-assignment changes update the tree in real-time.
        let (patch_tx, patch_rx) = mpsc::channel();
        let ws_auth   = auth.clone();
        let ws_config = config.clone();
        let dummy_trigger = Arc::new(tokio::sync::Notify::new());
        handle.spawn(async move {
            let my_username = ws_auth.current_username().await.unwrap_or_default();
            WsClient::run(ws_config, ws_auth, dummy_trigger, Some((my_username, patch_tx)), None).await;
        });

        let mut tree = FolderTreeWidget::new(auth.clone(), handle)
            .with_initial_selections(&config.selected_folders)
            .with_auto_expand(false) // Browse mode: checkmarks visible but no auto-expand
            .with_patch_rx(patch_rx);
        tree.fetch_roots();
        Self {
            active_tab: Tab::Folders,
            sync_folder: sync_folder.clone(),
            sync_interval_secs: config.sync_interval_secs,
            auto_start: config.auto_start,
            notifications_enabled: config.notifications_enabled,
            tree,
            disk_total,
            disk_free,
            last_disk_path: sync_folder,
            shortcut_message: String::new(),
            shortcut_is_error: false,
            done: false,
            pending_save: false,
            save_message: String::new(),
            save_time: None,
            progress,
        }
    }

    fn refresh_disk_info(&mut self) {
        if self.sync_folder != self.last_disk_path {
            if let Some((total, free)) = get_disk_space(&self.sync_folder) {
                self.disk_total = total;
                self.disk_free = free;
            }
            self.last_disk_path = self.sync_folder.clone();
        }
    }

    fn selected_size(&self) -> u64 {
        self.tree.selected_size()
    }

    fn build_selections(&self) -> Vec<FolderSelection> {
        self.tree.build_selections()
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = DARK_BG;
        visuals.override_text_color = Some(TEXT_PRIMARY);
        ctx.set_visuals(visuals);

        self.tree.poll(ctx);
        self.refresh_disk_info();

        if self.done {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Tab bar
        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (Tab::Folders, "  Folders  "),
                    (Tab::General, "  General  "),
                    (Tab::Shortcuts, "  Shortcuts  "),
                ] {
                    let selected = self.active_tab == tab;
                    let text = egui::RichText::new(label)
                        .size(14.0)
                        .color(if selected { ACCENT } else { TEXT_SECONDARY });
                    let resp = ui.add(egui::SelectableLabel::new(selected, text));
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.active_tab = tab;
                    }
                }
            });
            ui.add_space(4.0);
        });

        // Footer
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.add_space(8.0);
            // Show save feedback message
            let show_msg = self.save_time
                .map(|t| t.elapsed().as_secs_f32() < 4.0)
                .unwrap_or(false);
            if show_msg && !self.save_message.is_empty() {
                ui.label(egui::RichText::new(&self.save_message).size(12.0).color(SUCCESS_COLOR));
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let save_btn = egui::Button::new(
                        egui::RichText::new("Save").size(14.0).color(TEXT_PRIMARY),
                    )
                    .min_size(egui::vec2(100.0, 34.0))
                    .rounding(6.0)
                    .fill(ACCENT);
                    if ui.add(save_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.pending_save = true;
                    }
                    let close_btn = egui::Button::new(
                        egui::RichText::new("Close").size(14.0).color(TEXT_SECONDARY),
                    )
                    .min_size(egui::vec2(100.0, 34.0))
                    .rounding(6.0)
                    .fill(egui::Color32::from_rgb(40, 60, 90));
                    if ui.add(close_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.done = true;
                    }
                });
            });
            ui.add_space(8.0);
        });

        // Tab content
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(DARK_BG).inner_margin(egui::Margin::same(14.0)))
            .show(ctx, |ui| {
                match self.active_tab {
                    Tab::Folders => self.render_folders_tab(ui),
                    Tab::General => self.render_general_tab(ui),
                    Tab::Shortcuts => self.render_shortcuts_tab(ui),
                }
            });
    }
}

// ── Tab renderers ──

impl SettingsApp {
    fn render_folders_tab(&mut self, ui: &mut egui::Ui) {
        // ── Sync engine status bar ────────────────────────────────────────────
        let (status_text, is_active, is_paused) = self.progress.lock()
            .map(|p| {
                let (text, active) = p.status_line();
                (text, active, p.paused)
            })
            .unwrap_or(("Unknown".to_string(), false, false));
        let status_color = if is_paused { WARNING_COLOR } else if is_active { ACCENT } else { SUCCESS_COLOR };
        let mut toggle_pause = false;
        egui::Frame::default()
            .fill(egui::Color32::from_rgb(15, 28, 48))
            .rounding(6.0)
            .inner_margin(egui::Margin::symmetric(10.0, 6.0))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(40, 65, 100)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if is_paused {
                        ui.label(egui::RichText::new("||").size(11.0).color(WARNING_COLOR));
                    } else if is_active {
                        ui.spinner();
                    } else {
                        let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot_rect.center(), 5.0, SUCCESS_COLOR);
                    }
                    ui.label(egui::RichText::new(&status_text).size(12.0).color(status_color));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (btn_label, btn_color) = if is_paused {
                            ("Resume", SUCCESS_COLOR)
                        } else {
                            ("Pause", WARNING_COLOR)
                        };
                        let btn = egui::Button::new(
                            egui::RichText::new(btn_label).size(11.0).color(TEXT_PRIMARY),
                        )
                        .min_size(egui::vec2(64.0, 22.0))
                        .rounding(4.0)
                        .fill(btn_color.gamma_multiply(0.3));
                        if ui.add(btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            toggle_pause = true;
                        }
                    });
                });
            });
        if toggle_pause {
            if let Ok(mut p) = self.progress.lock() {
                p.paused = !is_paused;
                info!("Sync {}", if p.paused { "paused" } else { "resumed" });
            }
        }
        // Repaint frequently while active; slowly while idle (to update status transitions)
        ui.ctx().request_repaint_after(if is_active {
            std::time::Duration::from_millis(500)
        } else {
            std::time::Duration::from_secs(2)
        });
        ui.add_space(6.0);

        // ── Post-save progress banner (visible for 12 s after Save) ──────────
        let recently_saved = self.save_time
            .map(|t| t.elapsed().as_secs_f32() < 12.0)
            .unwrap_or(false);
        if recently_saved {
            let (fill, stroke_color, msg_color) = if is_active {
                (
                    egui::Color32::from_rgb(8, 38, 58),
                    ACCENT.gamma_multiply(0.35),
                    ACCENT,
                )
            } else {
                (
                    egui::Color32::from_rgb(8, 42, 20),
                    SUCCESS_COLOR.gamma_multiply(0.35),
                    SUCCESS_COLOR,
                )
            };
            let msg = if is_active {
                "Syncing folder selections...".to_string()
            } else {
                self.save_message.clone()
            };
            egui::Frame::default()
                .fill(fill)
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                .stroke(egui::Stroke::new(1.0, stroke_color))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if is_active {
                            ui.spinner();
                        } else {
                            let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                            ui.painter().circle_filled(dot_rect.center(), 5.0, msg_color);
                        }
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(&msg).size(12.0).color(msg_color));
                    });
                });
            ui.add_space(6.0);
            if is_active {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
        }

        // ── Folder tree header: title + count + refresh ───────────────────────
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Select Folders to Sync")
                    .size(13.0)
                    .color(TEXT_PRIMARY)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let refresh_btn = egui::Button::new(
                    egui::RichText::new("Refresh").size(11.0).color(ACCENT),
                )
                .min_size(egui::vec2(60.0, 24.0))
                .rounding(4.0)
                .fill(BTN_BG);
                if ui.add(refresh_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.tree.fetch_roots();
                }
                ui.add_space(8.0);
                let count = self.tree.map_len();
                ui.label(
                    egui::RichText::new(format!("{count} folder(s) selected"))
                        .size(11.0)
                        .color(TEXT_SECONDARY),
                );
            });
        });
        ui.add_space(4.0);

        // ── Search bar ────────────────────────────────────────────────────────
        ui.add(
            egui::TextEdit::singleline(&mut self.tree.search_query)
                .desired_width(ui.available_width())
                .hint_text("Search folders...")
                .margin(egui::Margin::symmetric(10.0, 6.0)),
        );
        ui.add_space(6.0);

        // ── Folder tree (takes most vertical space; reserve room for disk bar) ─
        let disk_bar_height: f32 = if self.disk_total > 0 { 46.0 } else { 0.0 };
        let tree_height = (ui.available_height() - disk_bar_height - 8.0).max(100.0);
        ui.allocate_ui(egui::vec2(ui.available_width(), tree_height), |ui| {
            let _ = self.tree.render(ui);
        });

        // ── Disk usage bar (compact, at the bottom) ───────────────────────────
        if self.disk_total > 0 {
            ui.add_space(4.0);
            ui.add_space(2.0);
            let used = self.disk_total - self.disk_free;
            let selected = self.selected_size();
            let total_f = self.disk_total as f32;
            let drive = drive_letter(&self.sync_folder);

            let bar_height = 10.0;
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), bar_height),
                egui::Sense::hover(),
            );
            let painter = ui.painter();
            painter.rect_filled(rect, 3.0, BAR_BG);
            let used_w = rect.width() * (used as f32 / total_f);
            painter.rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(used_w, bar_height)),
                3.0, BAR_USED,
            );
            let sel_w = rect.width() * (selected as f32 / total_f);
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(rect.min.x + used_w, rect.min.y),
                    egui::vec2(sel_w.min(rect.width() - used_w), bar_height),
                ),
                0.0, ACCENT,
            );
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{drive}:")).size(10.0).color(TEXT_SECONDARY));
                ui.label(egui::RichText::new(format!("  Used {}", format_size(used as i64))).size(10.0).color(BAR_USED));
                ui.label(egui::RichText::new(format!("  ·  Selected {}", format_size(selected as i64))).size(10.0).color(ACCENT));
                ui.label(egui::RichText::new(format!("  ·  Free {}", format_size(self.disk_free as i64))).size(10.0).color(SUCCESS_COLOR));
                if selected > self.disk_free {
                    ui.label(egui::RichText::new("  ⚠ Not enough space!").size(10.0).color(WARNING_COLOR));
                }
            });
        }
    }

    fn render_general_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            section_header(ui, "Sync Folder");
            card(ui, |ui| {
                ui.label(egui::RichText::new("Destination drive").size(12.0).color(TEXT_SECONDARY));
                ui.add_space(6.0);
                if let Some(new_path) = crate::ui::common::render_drive_picker(ui, &self.sync_folder) {
                    self.sync_folder = new_path;
                }
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Local folder where files are downloaded:").size(12.0).color(TEXT_SECONDARY));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.sync_folder)
                            .desired_width(ui.available_width() - 90.0)
                            .margin(egui::Margin::symmetric(8.0, 6.0)),
                    );
                    let btn = egui::Button::new(egui::RichText::new("Browse...").size(13.0).color(TEXT_PRIMARY))
                        .min_size(egui::vec2(80.0, 30.0))
                        .rounding(4.0)
                        .fill(egui::Color32::from_rgb(50, 75, 110));
                    if ui.add(btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_directory(&self.sync_folder)
                            .pick_folder()
                        {
                            self.sync_folder = path.to_string_lossy().to_string();
                            info!("Sync folder changed to: {}", self.sync_folder);
                        }
                    }
                });
            });
            ui.add_space(16.0);

            section_header(ui, "Sync Interval");
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Check for changes every:").size(13.0).color(TEXT_SECONDARY));
                    ui.add(
                        egui::DragValue::new(&mut self.sync_interval_secs)
                            .range(10..=3600)
                            .suffix(" seconds")
                            .speed(1.0),
                    );
                });
            });
            ui.add_space(16.0);

            section_header(ui, "General");
            card(ui, |ui| {
                ui.checkbox(&mut self.auto_start, "Start with Windows");
                ui.add_space(4.0);
                ui.checkbox(&mut self.notifications_enabled, "Show notifications");
            });
        });
    }

    fn render_shortcuts_tab(&mut self, ui: &mut egui::Ui) {
        // ── Explorer Navigation Pane ──────────────────────────────────────────
        section_header(ui, "Explorer Navigation Pane");
        card(ui, |ui| {
            ui.label(
                egui::RichText::new("Pin \"AGB CloudFiles\" to the Explorer left panel (like OneDrive).")
                    .size(12.0).color(TEXT_SECONDARY),
            );
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                // Add
                let add_btn = egui::Button::new(
                    egui::RichText::new("Add to navigation pane").size(13.0).color(TEXT_PRIMARY),
                )
                .min_size(egui::vec2(190.0, 34.0))
                .rounding(17.0)
                .fill(ACCENT);
                if ui.add(add_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    match create_explorer_sidebar(&self.sync_folder) {
                        Ok(()) => {
                            // Auto-restart Explorer so the new entry and icon appear immediately.
                            let msg = match restart_explorer() {
                                Ok(()) => "Added! Explorer restarted — check the navigation pane.".to_string(),
                                Err(e) => format!("Added, but Explorer restart failed: {e}"),
                            };
                            self.shortcut_message = msg;
                            self.shortcut_is_error = false;
                        }
                        Err(e) => {
                            self.shortcut_message = format!("Failed: {e}");
                            self.shortcut_is_error = true;
                            error!("Explorer sidebar failed: {e}");
                        }
                    }
                }

                ui.add_space(8.0);

                // Remove
                let rem_btn = egui::Button::new(
                    egui::RichText::new("Remove").size(13.0).color(ERROR_COLOR),
                )
                .min_size(egui::vec2(100.0, 34.0))
                .rounding(17.0)
                .fill(egui::Color32::from_rgb(50, 18, 18))
                .stroke(egui::Stroke::new(1.0, ERROR_COLOR.gamma_multiply(0.5)));
                if ui.add(rem_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    match remove_explorer_sidebar(&self.sync_folder) {
                        Ok(()) => {
                            let msg = match restart_explorer() {
                                Ok(()) => "Removed. Explorer restarted.".to_string(),
                                Err(_) => "Removed. Restart Explorer manually to apply.".to_string(),
                            };
                            self.shortcut_message = msg;
                            self.shortcut_is_error = false;
                        }
                        Err(e) => {
                            self.shortcut_message = format!("Remove failed: {e}");
                            self.shortcut_is_error = true;
                            error!("Explorer sidebar remove failed: {e}");
                        }
                    }
                }

                ui.add_space(8.0);

                // Restart Explorer
                let restart_btn = egui::Button::new(
                    egui::RichText::new("Restart Explorer").size(12.0).color(TEXT_PRIMARY),
                )
                .min_size(egui::vec2(130.0, 34.0))
                .rounding(17.0)
                .fill(egui::Color32::from_rgb(45, 58, 85))
                .stroke(egui::Stroke::new(1.0, BTN_BORDER));
                if ui.add(restart_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    match restart_explorer() {
                        Ok(()) => {
                            self.shortcut_message =
                                "Explorer restarted — check the navigation pane.".to_string();
                            self.shortcut_is_error = false;
                        }
                        Err(e) => {
                            self.shortcut_message = format!("Restart failed: {e}");
                            self.shortcut_is_error = true;
                        }
                    }
                }
            });

            if !self.shortcut_message.is_empty() {
                ui.add_space(8.0);
                let color = if self.shortcut_is_error { ERROR_COLOR } else { SUCCESS_COLOR };
                ui.label(egui::RichText::new(&self.shortcut_message).size(12.0).color(color));
            }
        });

        ui.add_space(16.0);

        // ── Desktop Shortcut ──────────────────────────────────────────────────
        section_header(ui, "Desktop Shortcut");
        card(ui, |ui| {
            ui.label(
                egui::RichText::new("Create a shortcut to the sync folder on your Desktop.")
                    .size(12.0).color(TEXT_SECONDARY),
            );
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                // Create
                let create_btn = egui::Button::new(
                    egui::RichText::new("Create Desktop shortcut").size(13.0).color(TEXT_PRIMARY),
                )
                .min_size(egui::vec2(190.0, 34.0))
                .rounding(17.0)
                .fill(ACCENT);
                if ui.add(create_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    match create_desktop_shortcut(&self.sync_folder) {
                        Ok(()) => {
                            self.shortcut_message = "Desktop shortcut created!".to_string();
                            self.shortcut_is_error = false;
                        }
                        Err(e) => {
                            self.shortcut_message = format!("Failed: {e}");
                            self.shortcut_is_error = true;
                            error!("Desktop shortcut failed: {e}");
                        }
                    }
                }

                ui.add_space(8.0);

                // Remove
                let rem_btn = egui::Button::new(
                    egui::RichText::new("Remove").size(13.0).color(ERROR_COLOR),
                )
                .min_size(egui::vec2(100.0, 34.0))
                .rounding(17.0)
                .fill(egui::Color32::from_rgb(50, 18, 18))
                .stroke(egui::Stroke::new(1.0, ERROR_COLOR.gamma_multiply(0.5)));
                if ui.add(rem_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    match remove_desktop_shortcut() {
                        Ok(()) => {
                            self.shortcut_message = "Desktop shortcut removed.".to_string();
                            self.shortcut_is_error = false;
                        }
                        Err(e) => {
                            self.shortcut_message = format!("Remove failed: {e}");
                            self.shortcut_is_error = true;
                            error!("Desktop shortcut remove failed: {e}");
                        }
                    }
                }
            });

            if !self.shortcut_message.is_empty() {
                ui.add_space(8.0);
                let color = if self.shortcut_is_error { ERROR_COLOR } else { SUCCESS_COLOR };
                ui.label(egui::RichText::new(&self.shortcut_message).size(12.0).color(color));
            }
        });
    }
}

// ── Public entry point ──

pub fn show_settings_window(
    config: &mut AppConfig,
    auth: &AuthState,
    rt: &tokio::runtime::Runtime,
    progress: &SharedProgress,
) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([700.0, 600.0])
            .with_min_inner_size([550.0, 450.0])
            .with_title("AGB Cloud Client - Settings")
            .with_icon(Arc::new(crate::ui::icon::app_icon_data())),
        ..Default::default()
    };

    let config_snapshot = config.clone();
    let auth_clone = auth.clone();
    let handle = rt.handle().clone();
    let progress_clone = progress.clone();

    let _ = eframe::run_native(
        "AGB Cloud Client - Settings",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(SettingsWrapper {
                inner: SettingsApp::new(&config_snapshot, auth_clone, handle, progress_clone),
            }))
        }),
    );
    // SettingsWrapper calls std::process::exit(0) on close — this line is only
    // reached if eframe exits for some other reason (e.g. on non-Windows or tests).
}

struct SettingsWrapper {
    inner: SettingsApp,
}

impl eframe::App for SettingsWrapper {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Exit immediately if the tray has requested a global shutdown.
        if crate::ui::common::is_shutdown_requested() {
            std::process::exit(0);
        }
        self.inner.update(ctx, frame);

        // Handle save (window stays open)
        if self.inner.pending_save {
            self.inner.pending_save = false;
            let selections = self.inner.build_selections();
            let count = selections.len();

            // Save to disk immediately so sync engine picks it up
            if let Ok(mut cfg) = crate::config::AppConfig::load_or_create() {
                cfg.sync_folder = self.inner.sync_folder.clone();
                cfg.sync_interval_secs = self.inner.sync_interval_secs;
                cfg.auto_start = self.inner.auto_start;
                cfg.notifications_enabled = self.inner.notifications_enabled;
                cfg.selected_folders = selections.clone();
                if let Err(e) = cfg.save() {
                    error!("Failed to save config: {e}");
                    self.inner.save_message = format!("Save failed: {e}");
                } else {
                    info!("Settings saved to config ({count} folders)");
                    self.inner.save_message = format!("Saved! ({count} folders selected)");
                }
                // Apply auto-start here (inside eframe) so it runs before GPU teardown
                if let Err(e) = crate::ui::common::set_auto_start(cfg.auto_start) {
                    error!("Auto-start toggle failed: {e}");
                }
            }
            self.inner.save_time = Some(std::time::Instant::now());
        }

        // Handle close — exit(0) here (inside eframe) to bypass GPU teardown crash.
        // Config was already saved when Save was clicked (pending_save branch above).
        if self.inner.done {
            info!("Settings window closed — exiting subprocess");
            std::process::exit(0);
        }
    }
}
