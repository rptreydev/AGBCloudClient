use std::collections::HashMap;
use std::sync::Arc;
use eframe::egui;
use tracing::{error, info};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::models::{FolderSelection, SyncPolicy};
use crate::sync::progress::SharedProgress;
use crate::sync::remote::RemoteClient;
use crate::ui::common::*;

#[derive(PartialEq, Clone, Copy)]
enum Tab { Folders, General, Shortcuts }

struct SettingsApp {
    active_tab: Tab,
    sync_folder: String,
    sync_interval_secs: u64,
    auto_start: bool,
    notifications_enabled: bool,
    roots: Vec<TreeNode>,
    selected_policies: HashMap<String, Option<SyncPolicy>>,
    /// UUIDs explicitly selected by the user (not auto-propagated to ancestors)
    explicit_selections: std::collections::HashSet<String>,
    /// Names from config for displaying selections before/after tree loads
    config_names: Vec<(String, String, String)>, // (uuid, name, policy_label)
    is_loading: bool,
    error_message: String,
    search_query: String,
    auth: AuthState,
    handle: tokio::runtime::Handle,
    progress: SharedProgress,
    result_rx: Option<std::sync::mpsc::Receiver<FetchResult>>,
    disk_total: u64,
    disk_free: u64,
    last_disk_path: String,
    shortcut_message: String,
    shortcut_is_error: bool,
    done: bool,
    saved: bool,
    /// Set to true when Save is clicked, processed by SettingsWrapper
    pending_save: bool,
    /// Feedback message shown after save
    save_message: String,
    save_time: Option<std::time::Instant>,
}

impl SettingsApp {
    fn new(config: &AppConfig, auth: AuthState, handle: tokio::runtime::Handle, progress: SharedProgress) -> Self {
        let selected_policies: HashMap<String, Option<SyncPolicy>> = config
            .selected_folders
            .iter()
            .map(|f| (f.uuid.clone(), Some(f.policy.clone())))
            .collect();
        let explicit_selections: std::collections::HashSet<String> = config
            .selected_folders
            .iter()
            .map(|f| f.uuid.clone())
            .collect();
        let config_names: Vec<(String, String, String)> = config
            .selected_folders
            .iter()
            .map(|f| {
                let policy_label = match &f.policy {
                    SyncPolicy::Copy => "Copy".to_string(),
                    SyncPolicy::KeepSynced { .. } => "Sync".to_string(),
                };
                (f.uuid.clone(), f.name.clone(), policy_label)
            })
            .collect();
        let sync_folder = config.sync_folder.clone();
        let (disk_total, disk_free) = get_disk_space(&sync_folder).unwrap_or((0, 0));
        let mut app = Self {
            active_tab: Tab::Folders,
            sync_folder: sync_folder.clone(),
            sync_interval_secs: config.sync_interval_secs,
            auto_start: config.auto_start,
            notifications_enabled: config.notifications_enabled,
            roots: Vec::new(),
            selected_policies,
            explicit_selections,
            config_names,
            is_loading: false,
            error_message: String::new(),
            search_query: String::new(),
            auth,
            handle,
            progress,
            result_rx: None,
            disk_total,
            disk_free,
            last_disk_path: sync_folder,
            shortcut_message: String::new(),
            shortcut_is_error: false,
            done: false,
            saved: false,
            pending_save: false,
            save_message: String::new(),
            save_time: None,
        };
        app.fetch_roots();
        app
    }

    fn fetch_roots(&mut self) {
        self.is_loading = true;
        self.error_message.clear();
        let auth = self.auth.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.result_rx = Some(rx);
        self.handle.spawn(async move {
            let remote = RemoteClient::new(auth);
            match remote.get_roots().await {
                Ok(roots) => { let _ = tx.send(FetchResult::Roots(roots)); }
                Err(e) => { let _ = tx.send(FetchResult::Error(e.to_string())); }
            }
        });
    }

    fn fetch_children(&mut self, folder_uuid: &str) {
        let uuid = folder_uuid.to_string();
        let auth = self.auth.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.result_rx = Some(rx);
        self.handle.spawn(async move {
            let remote = RemoteClient::new(auth);
            match remote.get_children(&uuid).await {
                Ok(children) => { let _ = tx.send(FetchResult::Children(uuid, children)); }
                Err(e) => { let _ = tx.send(FetchResult::Error(e.to_string())); }
            }
        });
    }

    fn poll_result(&mut self, ctx: &egui::Context) {
        let rx = match self.result_rx.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(result) => {
                self.is_loading = false;
                match result {
                    FetchResult::Roots(roots) => {
                        self.roots = roots.into_iter().map(TreeNode::from_cloud_file).collect();
                        info!("Loaded {} root folders", self.roots.len());
                    }
                    FetchResult::Children(parent_uuid, children) => {
                        insert_children(&mut self.roots, &parent_uuid, children);
                        if let Some(Some(policy)) = self.selected_policies.get(&parent_uuid) {
                            let policy = policy.clone();
                            for uuid in collect_descendant_uuids(&self.roots, &parent_uuid) {
                                self.selected_policies.entry(uuid).or_insert(Some(policy.clone()));
                            }
                        }
                    }
                    FetchResult::Error(msg) => {
                        error!("Fetch error: {msg}");
                        self.error_message = msg;
                    }
                }
                ctx.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Sender dropped without sending — treat as error
                if self.is_loading {
                    self.is_loading = false;
                    self.error_message = "Connection lost while fetching folders".to_string();
                    self.result_rx = None;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // Still waiting — keep repainting
                if self.is_loading {
                    ctx.request_repaint();
                }
            }
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
        calc_selected_size(&self.roots, &self.selected_policies)
    }

    fn build_selections(&self) -> Vec<FolderSelection> {
        let mut out = Vec::new();
        collect_selections(&self.roots, &self.selected_policies, &self.roots, &mut out);
        out
    }

    fn process_tree_actions(&mut self, actions: Vec<TreeAction>) {
        for action in actions {
            match action {
                TreeAction::Select(uuid) => {
                    self.explicit_selections.insert(uuid.clone());
                    self.selected_policies.insert(uuid.clone(), Some(SyncPolicy::KeepSynced { interval_secs: 30 }));
                    for d in collect_descendant_uuids(&self.roots, &uuid) {
                        self.explicit_selections.insert(d.clone());
                        self.selected_policies.entry(d).or_insert(Some(SyncPolicy::KeepSynced { interval_secs: 30 }));
                    }
                    for a in find_ancestor_uuids(&self.roots, &uuid) {
                        self.selected_policies.entry(a).or_insert(Some(SyncPolicy::KeepSynced { interval_secs: 30 }));
                    }
                }
                TreeAction::Deselect(uuid) => {
                    self.selected_policies.remove(&uuid);
                    self.explicit_selections.remove(&uuid);
                    for d in collect_descendant_uuids(&self.roots, &uuid) {
                        self.selected_policies.remove(&d);
                        self.explicit_selections.remove(&d);
                    }
                    let mut ancestors = find_ancestor_uuids(&self.roots, &uuid);
                    ancestors.reverse();
                    for a in ancestors {
                        if !self.explicit_selections.contains(&a)
                            && !has_selected_descendant(&self.roots, &a, &self.selected_policies)
                        {
                            self.selected_policies.remove(&a);
                        }
                    }
                }
                TreeAction::SetPolicy(uuid, policy) => {
                    self.explicit_selections.insert(uuid.clone());
                    self.selected_policies.insert(uuid.clone(), Some(policy.clone()));
                    for d in collect_descendant_uuids(&self.roots, &uuid) {
                        if self.selected_policies.contains_key(&d) {
                            self.selected_policies.insert(d, Some(policy.clone()));
                        }
                    }
                }
                TreeAction::FetchChildren(uuid) => {
                    self.fetch_children(&uuid);
                }
            }
        }
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = DARK_BG;
        visuals.override_text_color = Some(TEXT_PRIMARY);
        ctx.set_visuals(visuals);

        self.poll_result(ctx);
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
            ui.separator();
        });

        // Footer
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.add_space(8.0);
            let unset = self.selected_policies.iter()
                .filter(|(_, p)| p.is_none())
                .filter(|(uuid, _)| !has_selected_descendant(&self.roots, uuid, &self.selected_policies))
                .count();
            if unset > 0 {
                ui.label(egui::RichText::new(format!(
                    "! {unset} item(s) need a Copy or Sync policy assigned"
                )).size(11.0).color(WARNING_COLOR));
                ui.add_space(4.0);
            }
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
                        self.saved = true;
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
        // Sync status bar with pause/resume control
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
                        ui.label(egui::RichText::new("●").size(10.0).color(SUCCESS_COLOR));
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
        if is_active {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
        ui.add_space(6.0);

        // Currently selected summary (from config, always visible)
        if !self.config_names.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Syncing:").size(11.0).color(TEXT_SECONDARY));
                for (uuid, name, policy_label) in &self.config_names {
                    if self.selected_policies.contains_key(uuid) {
                        let chip_text = format!(" {name} ({policy_label}) ");
                        egui::Frame::default()
                            .fill(CHIP_BG)
                            .rounding(10.0)
                            .inner_margin(egui::Margin::symmetric(6.0, 2.0))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new(&chip_text).size(10.0).color(ACCENT));
                            });
                    }
                }
            });
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(4.0);
        }

        // Disk info bar
        if self.disk_total > 0 {
            let used = self.disk_total - self.disk_free;
            let selected = self.selected_size();
            let total_f = self.disk_total as f32;
            let drive = drive_letter(&self.sync_folder);
            ui.label(egui::RichText::new(format!("Drive {drive}")).size(12.0).color(TEXT_SECONDARY));
            ui.add_space(4.0);

            let bar_height = 18.0;
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
            let sel_w = rect.width() * (selected as f32 / total_f);
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(rect.min.x + used_w, rect.min.y),
                    egui::vec2(sel_w.min(rect.width() - used_w), bar_height),
                ),
                0.0, ACCENT,
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("Used: {}", format_size(used as i64))).size(11.0).color(BAR_USED));
                ui.label(egui::RichText::new(" | ").size(11.0).color(TEXT_SECONDARY));
                ui.label(egui::RichText::new(format!("Selected: {}", format_size(selected as i64))).size(11.0).color(ACCENT));
                ui.label(egui::RichText::new(" | ").size(11.0).color(TEXT_SECONDARY));
                ui.label(egui::RichText::new(format!("Free: {}", format_size(self.disk_free as i64))).size(11.0).color(SUCCESS_COLOR));
                ui.label(egui::RichText::new(" | ").size(11.0).color(TEXT_SECONDARY));
                ui.label(egui::RichText::new(format!("Total: {}", format_size(self.disk_total as i64))).size(11.0).color(TEXT_SECONDARY));
            });
            if selected > self.disk_free {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!(
                    "! Not enough space! Need {} but only {} free.",
                    format_size(selected as i64),
                    format_size(self.disk_free as i64),
                )).size(12.0).color(WARNING_COLOR));
            }
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
        }

        // Search + count + refresh
        let mut do_refresh = false;
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .desired_width(200.0)
                    .hint_text("Search..."),
            );
            let refresh_btn = egui::Button::new(
                egui::RichText::new("Refresh").size(11.0).color(ACCENT),
            )
            .min_size(egui::vec2(56.0, 22.0))
            .rounding(4.0)
            .fill(BTN_BG);
            if ui.add(refresh_btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                do_refresh = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let count = self.selected_policies.len();
                ui.label(egui::RichText::new(format!("{count} item(s) selected")).size(12.0).color(TEXT_SECONDARY));
            });
        });
        if do_refresh {
            self.roots.clear();
            self.fetch_roots();
        }
        ui.add_space(6.0);

        if self.is_loading && self.roots.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.spinner();
                ui.label(egui::RichText::new("Loading folders...").size(14.0).color(TEXT_SECONDARY));
            });
            return;
        }
        if !self.error_message.is_empty() {
            ui.label(egui::RichText::new(&self.error_message).color(ERROR_COLOR));
            if ui.button("Retry").clicked() { self.fetch_roots(); }
            return;
        }

        let mut actions = Vec::new();
        let search = self.search_query.clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            render_tree(ui, &mut self.roots, &self.selected_policies, 0, &mut actions, &search);
        });
        self.process_tree_actions(actions);
    }

    fn render_general_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            section_header(ui, "Sync Folder");
            card(ui, |ui| {
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
        section_header(ui, "Quick Access");
        card(ui, |ui| {
            ui.label(egui::RichText::new("Access your CloudFiles folder quickly:").size(12.0).color(TEXT_SECONDARY));
            ui.add_space(10.0);

            let btn = styled_button("Add to Explorer navigation pane");
            if ui.add(btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                match create_explorer_sidebar(&self.sync_folder) {
                    Ok(()) => {
                        self.shortcut_message = "Added to Explorer navigation pane! May need to restart Explorer.".to_string();
                        self.shortcut_is_error = false;
                    }
                    Err(e) => {
                        self.shortcut_message = format!("Failed: {e}");
                        self.shortcut_is_error = true;
                        error!("Explorer sidebar failed: {e}");
                    }
                }
            }
            ui.add_space(6.0);

            let btn = styled_button("Create Desktop shortcut");
            if ui.add(btn).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
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

    let result = Arc::new(std::sync::Mutex::new(None::<SettingsSnapshot>));
    let result_clone = result.clone();
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
                result: result_clone,
            }))
        }),
    );

    if let Ok(guard) = result.lock() {
        if let Some(ref snap) = *guard {
            if snap.saved {
                config.sync_folder = snap.sync_folder.clone();
                config.sync_interval_secs = snap.sync_interval_secs;
                config.auto_start = snap.auto_start;
                config.notifications_enabled = snap.notifications_enabled;
                config.selected_folders = snap.selected_folders.clone();
                if let Err(e) = config.save() {
                    error!("Failed to save config: {e}");
                }
                if let Err(e) = set_auto_start(config.auto_start) {
                    error!("Auto-start toggle failed: {e}");
                }
                info!("Settings saved to config ({} folders)", config.selected_folders.len());
            }
        }
    }
}

struct SettingsSnapshot {
    saved: bool,
    sync_folder: String,
    sync_interval_secs: u64,
    auto_start: bool,
    notifications_enabled: bool,
    selected_folders: Vec<FolderSelection>,
}

struct SettingsWrapper {
    inner: SettingsApp,
    result: Arc<std::sync::Mutex<Option<SettingsSnapshot>>>,
}

impl eframe::App for SettingsWrapper {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
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
            }
            self.inner.save_time = Some(std::time::Instant::now());

            // Update config_names chips to reflect current state
            self.inner.config_names = selections
                .iter()
                .map(|f| {
                    let label = match &f.policy {
                        SyncPolicy::Copy => "Copy".to_string(),
                        SyncPolicy::KeepSynced { .. } => "Sync".to_string(),
                    };
                    (f.uuid.clone(), f.name.clone(), label)
                })
                .collect();

            // Store snapshot so show_settings_window can update in-memory config on close
            if let Ok(mut guard) = self.result.lock() {
                *guard = Some(SettingsSnapshot {
                    saved: true,
                    sync_folder: self.inner.sync_folder.clone(),
                    sync_interval_secs: self.inner.sync_interval_secs,
                    auto_start: self.inner.auto_start,
                    notifications_enabled: self.inner.notifications_enabled,
                    selected_folders: selections,
                });
            }
        }

        // Handle close/cancel
        if self.inner.done {
            if let Ok(guard) = self.result.lock() {
                if guard.is_none() {
                    drop(guard);
                    if let Ok(mut guard) = self.result.lock() {
                        *guard = Some(SettingsSnapshot {
                            saved: self.inner.saved,
                            sync_folder: self.inner.sync_folder.clone(),
                            sync_interval_secs: self.inner.sync_interval_secs,
                            auto_start: self.inner.auto_start,
                            notifications_enabled: self.inner.notifications_enabled,
                            selected_folders: self.inner.build_selections(),
                        });
                    }
                }
            }
        }
    }
}
