//! Setup wizard — unified 5-step window for first-run setup.
//!
//! Steps: Welcome → SyncFolder → Login → SelectFolders → Finish
//! After finishing, stays open showing sync progress until the user clicks "Close".

use std::sync::Arc;
use eframe::egui;
use tokio::runtime::Runtime;
use tracing::{error, info};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::models::FolderSelection;
use crate::sync::progress::{SharedProgress, SyncPhase};
use crate::sync::SyncEngine;
use crate::ui::common::*;
use crate::ui::folder_tree::FolderTreeWidget;
use crate::ui::login_widget::{LoginOutcome, LoginWidget};

#[derive(PartialEq, Clone, Copy)]
enum WizardStep {
    Welcome,
    SyncFolder,
    Login,
    SelectFolders,
    Finish,
    /// After "Finish Setup" — shows live sync progress
    Syncing,
}

impl WizardStep {
    /// Steps shown in the indicator bar (not Syncing)
    const INDICATOR: [WizardStep; 5] = [
        WizardStep::Welcome,
        WizardStep::SyncFolder,
        WizardStep::Login,
        WizardStep::SelectFolders,
        WizardStep::Finish,
    ];

    fn index(self) -> usize {
        Self::INDICATOR.iter().position(|s| *s == self).unwrap_or(4)
    }

    fn label(self) -> &'static str {
        match self {
            WizardStep::Welcome => "Welcome",
            WizardStep::SyncFolder => "Sync Folder",
            WizardStep::Login => "Sign In",
            WizardStep::SelectFolders => "Select Folders",
            WizardStep::Finish | WizardStep::Syncing => "Finish",
        }
    }
}

struct WizardApp {
    step: WizardStep,

    // SyncFolder step
    sync_folder: String,
    disk_total: u64,
    disk_free: u64,
    last_disk_path: String,

    // Login step
    login: LoginWidget,
    login_complete: bool,

    // SelectFolders step
    tree: FolderTreeWidget,

    // Finish step
    add_explorer_sidebar: bool,
    create_desktop_shortcut_opt: bool,
    start_with_windows: bool,

    // Syncing step — progress from running sync engine
    progress: SharedProgress,
    sync_started: bool,
    last_phase: SyncPhase,
    /// Set once when sync completes: (is_success, files_done, error_msg)
    sync_banner: Option<(bool, usize, String)>,

    // Shared
    auth: AuthState,
    handle: tokio::runtime::Handle,
    done: bool,
    cancelled: bool,
}

impl WizardApp {
    fn new(config: &AppConfig, auth: AuthState, handle: tokio::runtime::Handle, progress: SharedProgress) -> Self {
        let sync_folder = config.sync_folder.clone();
        let (disk_total, disk_free) = get_disk_space(&sync_folder).unwrap_or((0, 0));
        let tree = FolderTreeWidget::new(auth.clone(), handle.clone());
        Self {
            step: WizardStep::Welcome,
            sync_folder: sync_folder.clone(),
            disk_total,
            disk_free,
            last_disk_path: sync_folder,
            login: LoginWidget::new(auth.clone(), handle.clone()),
            login_complete: false,
            tree,
            add_explorer_sidebar: true,
            create_desktop_shortcut_opt: true,
            start_with_windows: true,
            progress,
            sync_started: false,
            last_phase: SyncPhase::Idle,
            sync_banner: None,
            auth,
            handle,
            done: false,
            cancelled: false,
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

    fn can_advance(&self) -> bool {
        match self.step {
            WizardStep::Welcome => true,
            WizardStep::SyncFolder => !self.sync_folder.is_empty(),
            WizardStep::Login => self.login_complete,
            WizardStep::SelectFolders => self.tree.has_real_selection(),
            WizardStep::Finish => true,
            WizardStep::Syncing => false,
        }
    }

    fn advance(&mut self) {
        let next = match self.step {
            WizardStep::Welcome => Some(WizardStep::SyncFolder),
            WizardStep::SyncFolder => Some(WizardStep::Login),
            WizardStep::Login => Some(WizardStep::SelectFolders),
            WizardStep::SelectFolders => Some(WizardStep::Finish),
            WizardStep::Finish => Some(WizardStep::Syncing),
            WizardStep::Syncing => None,
        };
        if let Some(next_step) = next {
            self.step = next_step;
            self.on_step_enter();
        }
    }

    fn go_back(&mut self) {
        let prev = match self.step {
            WizardStep::Welcome => None,
            WizardStep::SyncFolder => Some(WizardStep::Welcome),
            WizardStep::Login => Some(WizardStep::SyncFolder),
            WizardStep::SelectFolders => Some(WizardStep::Login),
            WizardStep::Finish => Some(WizardStep::SelectFolders),
            WizardStep::Syncing => None, // can't go back from syncing
        };
        if let Some(prev_step) = prev {
            self.step = prev_step;
        }
    }

    fn on_step_enter(&mut self) {
        if self.step == WizardStep::SelectFolders {
            self.tree.fetch_roots();
        }
    }

    /// Start the sync engine in background (called when entering Syncing step)
    fn start_sync(&mut self) {
        if self.sync_started { return; }
        self.sync_started = true;

        let auth = self.auth.clone();
        let progress = self.progress.clone();
        self.handle.spawn(async move {
            let engine = SyncEngine::new(auth, progress);
            engine.run().await;
        });
        info!("Wizard: sync engine started");
    }

    fn build_selections(&self) -> Vec<FolderSelection> {
        self.tree.build_selections()
    }

    // ── Step renderers ──

    fn render_welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);

            ui.label(
                egui::RichText::new("AGBroadband")
                    .size(38.0)
                    .color(ACCENT)
                    .strong(),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("Cloud Client")
                    .size(20.0)
                    .color(TEXT_PRIMARY),
            );
            ui.add_space(4.0);

            // Version chip
            let version_text = format!("v{}", env!("CARGO_PKG_VERSION"));
            egui::Frame::default()
                .fill(SURFACE_VARIANT)
                .rounding(12.0)
                .inner_margin(egui::Margin { left: 12.0, right: 12.0, top: 4.0, bottom: 4.0 })
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(version_text).size(11.0).color(TEXT_SECONDARY));
                });

            ui.add_space(32.0);

            ui.label(
                egui::RichText::new("Set up file synchronization between\nyour computer and AGBroadband Cloud Files.")
                    .size(14.0)
                    .color(TEXT_SECONDARY),
            );

            ui.add_space(28.0);
        });

        // Steps preview card
        card(ui, |ui| {
            for (i, (icon, desc)) in [
                ("\u{1F4C1}", "Choose where to store your files"),
                ("\u{1F511}", "Sign in to your account"),
                ("\u{2705}", "Select which folders to sync"),
            ].iter().enumerate() {
                if i > 0 { ui.add_space(10.0); }
                ui.horizontal(|ui| {
                    // Step number circle
                    let (circle_rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                    ui.painter().circle(
                        circle_rect.center(), 14.0, SURFACE_VARIANT,
                        egui::Stroke::new(1.0, ACCENT_DIM),
                    );
                    ui.painter().text(
                        circle_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("{}", i + 1),
                        egui::FontId::proportional(12.0),
                        ACCENT,
                    );
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new(*desc).size(13.0).color(TEXT_PRIMARY));
                    let _ = icon; // emoji not rendered well in egui, using number circles instead
                });
            }
        });
    }

    fn render_sync_folder(&mut self, ui: &mut egui::Ui) {
        self.refresh_disk_info();

        ui.add_space(16.0);
        section_header(ui, "Choose Sync Folder");
        ui.label(
            egui::RichText::new("Select where CloudFiles will store your synced files.")
                .size(13.0)
                .color(TEXT_SECONDARY),
        );
        ui.add_space(20.0);

        card(ui, |ui| {
            ui.label(egui::RichText::new("Location").size(12.0).color(TEXT_SECONDARY));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.sync_folder)
                        .desired_width(ui.available_width() - 100.0)
                        .margin(egui::Margin::symmetric(10.0, 8.0)),
                );
                let btn = tonal_button("Browse...");
                if ui.add(btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_directory(&self.sync_folder)
                        .pick_folder()
                    {
                        self.sync_folder = path.to_string_lossy().to_string();
                    }
                }
            });

            if self.disk_total > 0 {
                ui.add_space(16.0);
                let drive = drive_letter(&self.sync_folder);
                ui.label(egui::RichText::new(format!("Drive {drive}")).size(12.0).color(TEXT_SECONDARY));
                ui.add_space(6.0);

                let used = self.disk_total - self.disk_free;
                let total_f = self.disk_total as f32;
                let bar_height = 8.0;
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), bar_height), egui::Sense::hover(),
                );
                let painter = ui.painter();
                painter.rect_filled(rect, 4.0, BAR_BG);
                let used_w = rect.width() * (used as f32 / total_f);
                painter.rect_filled(
                    egui::Rect::from_min_size(rect.min, egui::vec2(used_w, bar_height)),
                    4.0, BAR_USED,
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{} free", format_size(self.disk_free as i64))).size(11.0).color(SUCCESS_COLOR));
                    ui.label(egui::RichText::new(format!("  of {}", format_size(self.disk_total as i64))).size(11.0).color(TEXT_SECONDARY));
                });
            }
        });
    }

    fn render_login(&mut self, ui: &mut egui::Ui) {
        ui.add_space(16.0);
        self.login.render(ui);
    }

    fn render_select_folders(&mut self, ui: &mut egui::Ui) {
        // Disk info bar
        if self.disk_total > 0 {
            let used = self.disk_total - self.disk_free;
            let selected = self.tree.selected_size();
            let total_f = self.disk_total as f32;

            let bar_height = 14.0;
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), bar_height), egui::Sense::hover(),
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
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("Selected: {}", format_size(selected as i64))).size(10.0).color(ACCENT));
                ui.label(egui::RichText::new(" | ").size(10.0).color(TEXT_SECONDARY));
                ui.label(egui::RichText::new(format!("Free: {}", format_size(self.disk_free as i64))).size(10.0).color(SUCCESS_COLOR));
            });
            if selected > self.disk_free {
                ui.label(egui::RichText::new("! Not enough disk space!").size(11.0).color(WARNING_COLOR));
            }
            ui.add_space(4.0);
        }

        // Search + count
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.tree.search_query)
                    .desired_width(200.0)
                    .hint_text("Search..."),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let count = self.tree.map_len();
                ui.label(egui::RichText::new(format!("{count} item(s) selected")).size(12.0).color(TEXT_SECONDARY));
            });
        });
        ui.add_space(6.0);

        let _ = self.tree.render(ui);
    }

    fn render_finish(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);

            ui.label(
                egui::RichText::new("Ready to Sync!")
                    .size(26.0)
                    .color(ACCENT)
                    .strong(),
            );
            ui.add_space(16.0);
        });

        // Summary card
        card(ui, |ui| {
            ui.label(egui::RichText::new("Summary").size(14.0).color(TEXT_SECONDARY));
            ui.add_space(10.0);

            let count = self.tree.build_selections().len();
            let total_size = self.tree.selected_size();

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Folders:").size(13.0).color(TEXT_SECONDARY));
                ui.label(egui::RichText::new(format!("{count}")).size(13.0).color(TEXT_PRIMARY));
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Size:").size(13.0).color(TEXT_SECONDARY));
                ui.label(egui::RichText::new(format_size(total_size as i64)).size(13.0).color(TEXT_PRIMARY));
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Location:").size(13.0).color(TEXT_SECONDARY));
                ui.label(egui::RichText::new(&self.sync_folder).size(13.0).color(TEXT_PRIMARY));
            });
        });

        ui.add_space(12.0);

        // Options card
        card(ui, |ui| {
            ui.label(egui::RichText::new("Options").size(14.0).color(TEXT_SECONDARY));
            ui.add_space(10.0);
            ui.checkbox(&mut self.add_explorer_sidebar, "Add to Explorer navigation pane");
            ui.add_space(6.0);
            ui.checkbox(&mut self.create_desktop_shortcut_opt, "Create Desktop shortcut");
            ui.add_space(6.0);
            ui.checkbox(&mut self.start_with_windows, "Start with Windows");
        });
    }

    fn render_syncing(&mut self, ui: &mut egui::Ui) {
        // Note: start_sync() is called by WizardWrapper::update() AFTER config is saved to disk.

        ui.vertical_centered(|ui| {
            ui.add_space(20.0);
            ui.label(
                egui::RichText::new("Syncing Your Files")
                    .size(24.0)
                    .color(ACCENT)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("You can close this window — sync continues in the system tray.")
                    .size(13.0)
                    .color(TEXT_SECONDARY),
            );
            ui.add_space(16.0);
        });

        // Extract progress values (drop lock before rendering to avoid holding it across UI calls)
        let (phase, files_done, files_total, files_failed, last_error, current_folder, current_file) =
            if let Ok(p) = self.progress.lock() {
                (p.phase.clone(), p.files_done, p.files_total, p.files_failed, p.last_error.clone(), p.current_folder.clone(), p.current_file.clone())
            } else {
                (SyncPhase::Idle, 0, 0, 0, None, String::new(), String::new())
            };

        // Detect completion: transition from Syncing → Idle (success/failure) or first Error
        if self.last_phase == SyncPhase::Syncing && self.sync_banner.is_none() {
            match &phase {
                SyncPhase::Idle => {
                    if files_failed > 0 {
                        let err_msg = last_error.clone()
                            .unwrap_or_else(|| format!("{files_failed} file(s) failed to download"));
                        self.sync_banner = Some((false, files_done, err_msg));
                    } else {
                        self.sync_banner = Some((true, files_done, String::new()));
                    }
                }
                SyncPhase::Error(e) => {
                    self.sync_banner = Some((false, files_done, e.clone()));
                }
                _ => {}
            }
        }
        self.last_phase = phase.clone();

        // Live progress card
        card(ui, |ui| {
            // Status header
            ui.horizontal(|ui| {
                match &phase {
                    SyncPhase::Idle => {
                        let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot_rect.center(), 5.0, SUCCESS_COLOR);
                        ui.add_space(6.0);
                        if files_done > 0 {
                            ui.label(egui::RichText::new(format!("Up to date — {files_done} files synced")).size(14.0).color(SUCCESS_COLOR));
                        } else {
                            ui.label(egui::RichText::new("Waiting for sync cycle...").size(14.0).color(TEXT_SECONDARY));
                        }
                    }
                    SyncPhase::Syncing => {
                        ui.spinner();
                        ui.label(egui::RichText::new("Syncing...").size(14.0).color(ACCENT));
                    }
                    SyncPhase::Error(e) => {
                        let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot_rect.center(), 5.0, ERROR_COLOR);
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(format!("Error: {e}")).size(14.0).color(ERROR_COLOR));
                    }
                }
            });
            ui.add_space(12.0);

            // Progress bar
            if files_total > 0 {
                // Give half-credit to the file being downloaded so the bar
                // shows activity instead of sitting at 0% for the full download.
                let frac = if !current_file.is_empty() {
                    (files_done as f32 + 0.5) / files_total as f32
                } else {
                    files_done as f32 / files_total as f32
                };
                let bar_h = 8.0;
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), bar_h), egui::Sense::hover(),
                );
                let p = ui.painter();
                p.rect_filled(rect, 4.0, BAR_BG);
                let filled_w = rect.width() * frac.min(1.0);
                p.rect_filled(
                    egui::Rect::from_min_size(rect.min, egui::vec2(filled_w, bar_h)),
                    4.0, ACCENT,
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{:.0}%", frac * 100.0))
                            .size(13.0)
                            .color(ACCENT)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new(format!("  {files_done} / {files_total} files"))
                            .size(12.0)
                            .color(TEXT_SECONDARY),
                    );
                });
            }

            // Show download errors in red if any
            if files_failed > 0 {
                ui.add_space(8.0);
                let err_text = if let Some(ref e) = last_error {
                    format!("⚠ {files_failed} download error(s) — last: {e}")
                } else {
                    format!("⚠ {files_failed} download error(s)")
                };
                ui.label(egui::RichText::new(err_text).size(11.0).color(ERROR_COLOR));
            }

            // Current folder/file
            if !current_folder.is_empty() {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    paint_folder_icon(ui, true);
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(&current_folder).size(12.0).color(TEXT_PRIMARY));
                });
            }
            if !current_file.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    paint_file_icon(ui);
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(&current_file).size(11.0).color(TEXT_SECONDARY));
                });
            }
        });

        // Keep repainting until the completion banner is shown (covers the initial
        // idle state before the engine wakes up as well as active downloading).
        if self.sync_banner.is_none() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
        }

        // ── Completion banner ──
        if let Some((is_success, count, error)) = self.sync_banner.clone() {
            ui.add_space(12.0);
            let (accent_col, bg_col, icon, title) = if is_success {
                (
                    SUCCESS_COLOR,
                    egui::Color32::from_rgba_premultiplied(15, 60, 30, 220),
                    "\u{2714}", // ✔
                    "Sync Complete!",
                )
            } else {
                (
                    ERROR_COLOR,
                    egui::Color32::from_rgba_premultiplied(70, 15, 15, 220),
                    "\u{26A0}", // ⚠
                    "Sync Error",
                )
            };

            let frame = egui::Frame::default()
                .fill(bg_col)
                .stroke(egui::Stroke::new(1.5, accent_col))
                .rounding(egui::Rounding::same(10.0))
                .inner_margin(egui::Margin::same(14.0));

            frame.show(ui, |ui: &mut egui::Ui| {
                ui.horizontal(|ui: &mut egui::Ui| {
                    // Icon circle
                    let (icon_rect, _) = ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::hover());
                    ui.painter().circle_filled(icon_rect.center(), 16.0, egui::Color32::from_rgba_premultiplied(
                        accent_col.r() / 5, accent_col.g() / 5, accent_col.b() / 5, 180
                    ));
                    ui.painter().text(
                        icon_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        icon,
                        egui::FontId::proportional(16.0),
                        accent_col,
                    );
                    ui.add_space(12.0);
                    // Text block
                    ui.vertical(|ui: &mut egui::Ui| {
                        ui.label(egui::RichText::new(title).size(15.0).color(accent_col).strong());
                        ui.add_space(2.0);
                        if is_success {
                            let file_word = if count == 1 { "file" } else { "files" };
                            ui.label(
                                egui::RichText::new(format!("{count} {file_word} downloaded and synced successfully."))
                                    .size(12.0)
                                    .color(TEXT_SECONDARY),
                            );
                        } else if !error.is_empty() {
                            ui.label(egui::RichText::new(error.as_str()).size(12.0).color(TEXT_SECONDARY));
                        } else {
                            ui.label(egui::RichText::new("An error occurred during sync.").size(12.0).color(TEXT_SECONDARY));
                        }
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("You can close this window — the app continues in the system tray.")
                                .size(11.0)
                                .color(TEXT_DISABLED),
                        );
                    });
                });
            });
        }

        // Disk space
        if self.disk_total > 0 {
            ui.add_space(12.0);
            let used = self.disk_total - self.disk_free;
            let total_f = self.disk_total as f32;
            let drive = drive_letter(&self.sync_folder);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("Drive {drive}")).size(11.0).color(TEXT_SECONDARY));
                ui.label(egui::RichText::new(format!("  {} free of {}", format_size(self.disk_free as i64), format_size(self.disk_total as i64))).size(11.0).color(TEXT_SECONDARY));
            });
            ui.add_space(4.0);
            let bar_h = 6.0;
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), bar_h), egui::Sense::hover(),
            );
            let p = ui.painter();
            p.rect_filled(rect, 3.0, BAR_BG);
            let used_w = rect.width() * (used as f32 / total_f);
            p.rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(used_w, bar_h)),
                3.0, BAR_USED,
            );
        }
    }
}

impl eframe::App for WizardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        configure_visuals(ctx);

        // Poll login widget — advance the wizard as soon as auth succeeds.
        if let Some(LoginOutcome::Success) = self.login.poll(ctx) {
            info!("Login successful via wizard");
            self.login_complete = true;
            self.advance();
        }
        self.tree.poll(ctx);

        if self.done || self.cancelled {
            // Don't send viewport commands here — WizardWrapper handles exit via process::exit(0)
            // Sending Visible(false)/Close causes visual flashes before exit takes effect
            return;
        }
        if self.login.is_loading || self.tree.is_loading {
            ctx.request_repaint();
        }

        // ── Custom title bar (Material surface, draggable, close button) ──
        egui::TopBottomPanel::top("title_bar")
            .frame(egui::Frame::default().fill(TITLE_BAR_BG).inner_margin(egui::Margin::same(0.0)))
            .exact_height(38.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    // Draggable title area
                    let title_rect = ui.available_rect_before_wrap();
                    let title_resp = ui.interact(
                        egui::Rect::from_min_size(title_rect.min, egui::vec2(title_rect.width() - 44.0, 38.0)),
                        ui.id().with("title_drag"),
                        egui::Sense::click_and_drag(),
                    );
                    if title_resp.dragged() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    // App name
                    ui.painter().text(
                        egui::pos2(title_rect.min.x, title_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        "AGB Cloud Client",
                        egui::FontId::proportional(13.0),
                        TEXT_SECONDARY,
                    );

                    // Close button (right side, rounded)
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(6.0);
                        let close_r = ui.allocate_rect(
                            egui::Rect::from_min_size(ui.cursor().min, egui::vec2(34.0, 28.0)),
                            egui::Sense::click(),
                        );
                        let hover = close_r.hovered();
                        let bg = if hover {
                            egui::Color32::from_rgb(200, 50, 50)
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        ui.painter().rect_filled(close_r.rect, 6.0, bg);
                        ui.painter().text(
                            close_r.rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "\u{2715}",
                            egui::FontId::proportional(13.0),
                            if hover { TEXT_PRIMARY } else { TEXT_DISABLED },
                        );
                        if hover {
                            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if close_r.clicked() {
                            self.cancelled = true;
                        }
                    });
                });
            });

        // ── Material Design stepper ──
        egui::TopBottomPanel::top("step_bar")
            .frame(egui::Frame::default().fill(SURFACE).inner_margin(egui::Margin { left: 24.0, right: 24.0, top: 16.0, bottom: 12.0 }))
            .show(ctx, |ui| {
                let total = WizardStep::INDICATOR.len();
                let current = self.step.index();
                let avail_w = ui.available_width();
                let circle_r = 14.0;
                let spacing = (avail_w - circle_r * 2.0 * total as f32) / (total - 1) as f32;

                let (stepper_rect, _) = ui.allocate_exact_size(
                    egui::vec2(avail_w, circle_r * 2.0 + 18.0), egui::Sense::hover(),
                );
                let painter = ui.painter();
                let y_center = stepper_rect.min.y + circle_r;

                for (i, ws) in WizardStep::INDICATOR.iter().enumerate() {
                    let is_done = i < current;
                    let is_current = i == current || (self.step == WizardStep::Syncing && i == total - 1);

                    let cx = stepper_rect.min.x + circle_r + i as f32 * (circle_r * 2.0 + spacing);

                    // Connecting line (to next step)
                    if i < total - 1 {
                        let next_cx = stepper_rect.min.x + circle_r + (i + 1) as f32 * (circle_r * 2.0 + spacing);
                        let line_color = if is_done {
                            ACCENT
                        } else {
                            egui::Color32::from_rgb(40, 52, 72)
                        };
                        painter.line_segment(
                            [egui::pos2(cx + circle_r + 4.0, y_center), egui::pos2(next_cx - circle_r - 4.0, y_center)],
                            egui::Stroke::new(2.0, line_color),
                        );
                    }

                    // Circle
                    let (circle_fill, circle_stroke, text_color) = if is_current {
                        (ACCENT, ACCENT, TEXT_PRIMARY)
                    } else if is_done {
                        (ACCENT, ACCENT, TEXT_PRIMARY)
                    } else {
                        (egui::Color32::TRANSPARENT, egui::Color32::from_rgb(55, 70, 95), TEXT_DISABLED)
                    };

                    painter.circle(
                        egui::pos2(cx, y_center),
                        circle_r,
                        circle_fill,
                        egui::Stroke::new(if is_current || is_done { 0.0 } else { 1.5 }, circle_stroke),
                    );

                    // Number or checkmark inside circle
                    if is_done {
                        // Checkmark
                        let c = egui::pos2(cx, y_center);
                        let stroke = egui::Stroke::new(2.0, TEXT_PRIMARY);
                        painter.line_segment(
                            [egui::pos2(c.x - 4.0, c.y), egui::pos2(c.x - 1.0, c.y + 3.0)],
                            stroke,
                        );
                        painter.line_segment(
                            [egui::pos2(c.x - 1.0, c.y + 3.0), egui::pos2(c.x + 4.0, c.y - 3.0)],
                            stroke,
                        );
                    } else {
                        painter.text(
                            egui::pos2(cx, y_center),
                            egui::Align2::CENTER_CENTER,
                            format!("{}", i + 1),
                            egui::FontId::proportional(12.0),
                            text_color,
                        );
                    }

                    // Label below circle
                    let label_color = if is_current {
                        ACCENT
                    } else if is_done {
                        TEXT_SECONDARY
                    } else {
                        TEXT_DISABLED
                    };
                    painter.text(
                        egui::pos2(cx, y_center + circle_r + 10.0),
                        egui::Align2::CENTER_CENTER,
                        ws.label(),
                        egui::FontId::proportional(11.0),
                        label_color,
                    );
                }
            });

        // ── Main content (includes nav buttons at the bottom) ──
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(DARK_BG).inner_margin(egui::Margin { left: 24.0, right: 24.0, top: 16.0, bottom: 16.0 }))
            .show(ctx, |ui| {
                // Step content takes remaining space
                let content_rect = ui.available_rect_before_wrap();
                let nav_height = 56.0;
                let content_max = content_rect.height() - nav_height;

                // Content area
                ui.allocate_ui(egui::vec2(content_rect.width(), content_max), |ui| {
                    match self.step {
                        WizardStep::Welcome => self.render_welcome(ui),
                        WizardStep::SyncFolder => self.render_sync_folder(ui),
                        WizardStep::Login => self.render_login(ui),
                        WizardStep::SelectFolders => self.render_select_folders(ui),
                        WizardStep::Finish => self.render_finish(ui),
                        WizardStep::Syncing => self.render_syncing(ui),
                    }
                });

                // ── Navigation buttons (Material Design) ──
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(16.0);

                    ui.horizontal(|ui| {
                        if self.step == WizardStep::Syncing {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let close_btn = filled_button("Finish", true)
                                    .min_size(egui::vec2(120.0, 40.0));
                                if ui.add(close_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                    self.done = true;
                                }
                            });
                        } else {
                            // Cancel (left side) — text button style
                            if ui.add(text_button("Cancel")).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                self.cancelled = true;
                            }

                            // Right-aligned: Back + Next/Finish
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                // Next / Finish Setup (rightmost) — filled primary
                                if self.step != WizardStep::Login {
                                    let is_finish = self.step == WizardStep::Finish;
                                    let label = if is_finish { "Finish Setup" } else { "Next" };
                                    let can = self.can_advance();

                                    let next_btn = filled_button(label, can)
                                        .min_size(egui::vec2(if is_finish { 140.0 } else { 100.0 }, 40.0));

                                    if ui.add_enabled(can, next_btn)
                                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .clicked()
                                    {
                                        self.advance();
                                    }
                                }

                                // Back — tonal button
                                if self.step != WizardStep::Welcome {
                                    ui.add_space(8.0);
                                    if ui.add(tonal_button("Back"))
                                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .clicked()
                                    {
                                        self.go_back();
                                    }
                                }
                            });
                        }
                    });

                    ui.add_space(10.0);
                    divider(ui);
                });
            });
    }
}

/// Show the setup wizard. Blocks until user finishes or cancels.
/// Always calls `process::exit(0)` — never returns.
pub fn show_setup_wizard(auth: &AuthState, config: &mut AppConfig, rt: &Runtime) {
    let handle = rt.handle().clone();
    let auth_clone = auth.clone();
    let config_snapshot = config.clone();
    let progress = std::sync::Arc::new(std::sync::Mutex::new(
        crate::sync::SyncProgress::default(),
    ));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([700.0, 580.0])
            .with_min_inner_size([550.0, 450.0])
            .with_title("AGB Cloud Client — Setup Wizard (Beta)")
            .with_decorations(false)
            .with_transparent(false)
            .with_icon(Arc::new(crate::ui::icon::app_icon_data())),
        ..Default::default()
    };

    let progress_clone = progress.clone();

    info!("Launching wizard eframe window...");
    let run_result = eframe::run_native(
        "AGB Cloud Client — Setup Wizard",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(WizardWrapper {
                inner: WizardApp::new(&config_snapshot, auth_clone, handle, progress_clone),
            }))
        }),
    );
    match &run_result {
        Ok(()) => info!("Wizard window closed normally"),
        Err(e) => error!("Wizard window error: {e}"),
    }
    // WizardWrapper::update() always calls process::exit(0) before GPU teardown.
    // If we somehow reach this point, exit cleanly.
    std::process::exit(0);
}

struct WizardWrapper {
    inner: WizardApp,
}

impl eframe::App for WizardWrapper {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let was_syncing = self.inner.step == WizardStep::Syncing;

        self.inner.update(ctx, frame);

        // Save config to disk when transitioning to Syncing step (before engine starts)
        if self.inner.step == WizardStep::Syncing && !was_syncing && !self.inner.cancelled {
            if let Ok(mut cfg) = AppConfig::load_or_create() {
                cfg.sync_folder = self.inner.sync_folder.clone();
                cfg.selected_folders = self.inner.build_selections();
                cfg.auto_start = self.inner.start_with_windows;
                cfg.setup_complete = true;
                if let Err(e) = cfg.save() {
                    error!("Failed to save config for sync: {e}");
                } else {
                    info!("Config saved to disk before sync start");
                }
            }
            // Start sync engine NOW — config is already on disk so the engine reads correct selections.
            self.inner.start_sync();

            // Notify the user that setup completed successfully
            let folder_count = self.inner.build_selections().len();
            let body = format!(
                "{} folder{} selected. Sync is running in the background.",
                folder_count,
                if folder_count == 1 { "" } else { "s" }
            );
            if let Err(e) = notify_rust::Notification::new()
                .app_id(crate::ui::common::NOTIFICATION_APP_ID)
                .summary("AGB Cloud Client — Setup Complete")
                .body(&body)
                .timeout(notify_rust::Timeout::Milliseconds(6000))
                .show()
            {
                error!("Setup notification failed: {e}");
            }
        }

        if self.inner.done || self.inner.cancelled {
            // Hide window immediately to prevent flash/blank frame
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));

            let completed = self.inner.done && !self.inner.cancelled;
            let selections = self.inner.build_selections();
            info!("WizardWrapper: done={}, cancelled={}, folders={}",
                self.inner.done, self.inner.cancelled, selections.len());

            if completed {
                // Save config, create shortcuts, and relaunch in tray mode.
                // We do this HERE because eframe::run_native crashes during
                // GPU teardown on some Windows machines — it never returns.
                let mut cfg = AppConfig::load_or_create().unwrap_or_default();
                cfg.sync_folder = self.inner.sync_folder.clone();
                cfg.selected_folders = selections;
                cfg.auto_start = self.inner.start_with_windows;
                cfg.setup_complete = true;
                if let Err(e) = cfg.save() {
                    error!("Failed to save config: {e}");
                } else {
                    info!("Config saved: {} folders, sync_folder={}",
                        cfg.selected_folders.len(), cfg.sync_folder);
                }

                // Create shortcuts
                if self.inner.add_explorer_sidebar {
                    if let Err(e) = create_explorer_sidebar(&cfg.sync_folder) {
                        error!("Explorer sidebar failed: {e}");
                    } else if let Err(e) = restart_explorer() {
                        error!("Explorer restart failed: {e}");
                    }
                }
                if self.inner.create_desktop_shortcut_opt {
                    if let Err(e) = create_desktop_shortcut(&cfg.sync_folder) {
                        error!("Desktop shortcut failed: {e}");
                    }
                }
                if let Err(e) = set_auto_start(self.inner.start_with_windows) {
                    error!("Auto-start setup failed: {e}");
                }

                // Relaunch app in tray mode (without --setup)
                if let Ok(exe) = std::env::current_exe() {
                    info!("Relaunching app for tray mode: {}", exe.display());
                    match std::process::Command::new(&exe).spawn() {
                        Ok(_) => info!("Relaunch successful"),
                        Err(e) => error!("Relaunch failed: {e}"),
                    }
                }

                // Give the log writer time to flush, then exit.
                // We skip eframe's GPU teardown which crashes on some machines.
                std::thread::sleep(std::time::Duration::from_millis(300));
                std::process::exit(0);
            } else {
                // Cancelled — run the NSIS silent uninstaller to clean up
                // files, registry entries and shortcuts that were written
                // before the wizard launched.
                if let Ok(exe) = std::env::current_exe() {
                    if let Some(install_dir) = exe.parent() {
                        let uninstaller = install_dir.join("uninstall.exe");
                        if uninstaller.exists() {
                            info!("Setup cancelled — launching silent uninstaller");
                            // /S = silent, no _?= so NSIS copies to %TEMP% and
                            // can delete the original install directory.
                            std::process::Command::new(&uninstaller)
                                .arg("/S")
                                .spawn()
                                .ok();
                            // Give the uninstaller time to start before we release the exe lock.
                            std::thread::sleep(std::time::Duration::from_millis(800));
                        } else {
                            info!("Setup cancelled — no uninstaller found, exiting");
                        }
                    }
                }
                std::process::exit(0);
            }
        }
    }
}
