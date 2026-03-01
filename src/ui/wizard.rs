//! Setup wizard — unified 5-step window for first-run setup.
//!
//! Steps: Welcome → SyncFolder → Login → SelectFolders → Finish
//! After finishing, stays open showing sync progress until the user clicks "Close".

use std::collections::HashMap;
use std::sync::Arc;
use eframe::egui;
use tokio::runtime::Runtime;
use tracing::{error, info};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::models::{FolderSelection, LoginCredentials, SyncPolicy};
use crate::sync::progress::{SharedProgress, SyncPhase};
use crate::sync::remote::RemoteClient;
use crate::sync::SyncEngine;
use crate::ui::common::*;

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

/// Result from async login attempt
enum LoginResult {
    Success,
    Needs2FA(String),
    Error(String),
}

struct WizardApp {
    step: WizardStep,

    // SyncFolder step
    sync_folder: String,
    disk_total: u64,
    disk_free: u64,
    last_disk_path: String,

    // Login step
    username: String,
    password: String,
    verification_code: String,
    login_error: String,
    login_info: String,
    login_loading: bool,
    needs_2fa: bool,
    login_complete: bool,
    login_rx: Option<std::sync::mpsc::Receiver<LoginResult>>,

    // SelectFolders step
    roots: Vec<TreeNode>,
    selected_policies: HashMap<String, Option<SyncPolicy>>,
    tree_loading: bool,
    tree_error: String,
    tree_rx: Option<std::sync::mpsc::Receiver<FetchResult>>,
    search_query: String,

    // Finish step
    add_explorer_sidebar: bool,
    create_desktop_shortcut_opt: bool,
    start_with_windows: bool,

    // Syncing step — progress from running sync engine
    progress: SharedProgress,
    sync_started: bool,

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
        Self {
            step: WizardStep::Welcome,
            sync_folder: sync_folder.clone(),
            disk_total,
            disk_free,
            last_disk_path: sync_folder,
            username: String::new(),
            password: String::new(),
            verification_code: String::new(),
            login_error: String::new(),
            login_info: String::new(),
            login_loading: false,
            needs_2fa: false,
            login_complete: false,
            login_rx: None,
            roots: Vec::new(),
            selected_policies: HashMap::new(),
            tree_loading: false,
            tree_error: String::new(),
            tree_rx: None,
            search_query: String::new(),
            add_explorer_sidebar: true,
            create_desktop_shortcut_opt: true,
            start_with_windows: false,
            progress,
            sync_started: false,
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
            WizardStep::SelectFolders => {
                // At least one folder must have a real policy (Some(policy)).
                // Ancestor markers (None) don't count — they're just UI helpers.
                self.selected_policies.values().any(|p| p.is_some())
            }
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
        if self.step == WizardStep::SelectFolders && self.roots.is_empty() {
            self.fetch_roots();
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

    // ── Login logic ──

    fn attempt_login(&mut self) {
        self.login_loading = true;
        self.login_error.clear();
        self.login_info.clear();

        let credentials = LoginCredentials {
            username: self.username.clone(),
            password: self.password.clone(),
            verification_code: if self.needs_2fa {
                Some(self.verification_code.clone())
            } else {
                None
            },
        };

        let auth = self.auth.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.login_rx = Some(rx);

        self.handle.spawn(async move {
            match auth.login(credentials).await {
                Ok(()) => { let _ = tx.send(LoginResult::Success); }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("2FA_REQUIRED") {
                        let _ = tx.send(LoginResult::Needs2FA(
                            "Verification code sent to your email".to_string(),
                        ));
                    } else {
                        let _ = tx.send(LoginResult::Error(msg));
                    }
                }
            }
        });
    }

    fn poll_login(&mut self) {
        let result = self.login_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = result {
            self.login_loading = false;
            self.login_rx = None;
            match result {
                LoginResult::Success => {
                    info!("Login successful via wizard");
                    self.login_complete = true;
                    self.advance();
                }
                LoginResult::Needs2FA(msg) => {
                    self.needs_2fa = true;
                    self.login_info = msg;
                }
                LoginResult::Error(msg) => {
                    error!("Login error: {msg}");
                    self.login_error = msg;
                }
            }
        }
    }

    // ── Tree logic ──

    fn fetch_roots(&mut self) {
        self.tree_loading = true;
        self.tree_error.clear();
        let auth = self.auth.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.tree_rx = Some(rx);
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
        self.tree_rx = Some(rx);
        self.handle.spawn(async move {
            let remote = RemoteClient::new(auth);
            match remote.get_children(&uuid).await {
                Ok(children) => { let _ = tx.send(FetchResult::Children(uuid, children)); }
                Err(e) => { let _ = tx.send(FetchResult::Error(e.to_string())); }
            }
        });
    }

    fn poll_tree(&mut self) {
        let result = self.tree_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = result {
            self.tree_loading = false;
            match result {
                FetchResult::Roots(roots) => {
                    self.roots = roots.into_iter().map(TreeNode::from_cloud_file).collect();
                    info!("Wizard: loaded {} root folders", self.roots.len());
                }
                FetchResult::Children(parent_uuid, children) => {
                    insert_children(&mut self.roots, &parent_uuid, children);
                    // Only propagate to children if parent has a REAL policy (not None ancestor marker)
                    if let Some(Some(policy)) = self.selected_policies.get(&parent_uuid) {
                        let policy = policy.clone();
                        for uuid in collect_descendant_uuids(&self.roots, &parent_uuid) {
                            self.selected_policies.entry(uuid).or_insert(Some(policy.clone()));
                        }
                    }
                }
                FetchResult::Error(msg) => {
                    error!("Wizard fetch error: {msg}");
                    self.tree_error = msg;
                }
            }
        }
    }

    fn process_tree_actions(&mut self, actions: Vec<TreeAction>) {
        for action in actions {
            match action {
                TreeAction::Select(uuid) => {
                    // Explicitly selected → always set policy (upgrades None ancestor markers)
                    self.selected_policies.insert(uuid.clone(), Some(SyncPolicy::KeepSynced { interval_secs: 30 }));
                    for d in collect_descendant_uuids(&self.roots, &uuid) {
                        self.selected_policies.entry(d).or_insert(Some(SyncPolicy::KeepSynced { interval_secs: 30 }));
                    }
                    // Mark ancestors as checked without policy (won't be synced)
                    for a in find_ancestor_uuids(&self.roots, &uuid) {
                        self.selected_policies.entry(a).or_insert(None);
                    }
                }
                TreeAction::Deselect(uuid) => {
                    self.selected_policies.remove(&uuid);
                    for d in collect_descendant_uuids(&self.roots, &uuid) {
                        self.selected_policies.remove(&d);
                    }
                    // Clean up ancestor markers (bottom-up) when no more selected descendants
                    let mut ancestors = find_ancestor_uuids(&self.roots, &uuid);
                    ancestors.reverse();
                    for a in ancestors {
                        if matches!(self.selected_policies.get(&a), Some(None)) {
                            if !has_selected_descendant(&self.roots, &a, &self.selected_policies) {
                                self.selected_policies.remove(&a);
                            }
                        }
                    }
                }
                TreeAction::SetPolicy(uuid, policy) => {
                    self.selected_policies.insert(uuid.clone(), Some(policy.clone()));
                    // Propagate to all selected descendants
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

    fn selected_size(&self) -> u64 {
        calc_selected_size(&self.roots, &self.selected_policies)
    }

    fn build_selections(&self) -> Vec<FolderSelection> {
        let mut out = Vec::new();
        collect_selections(&self.roots, &self.selected_policies, &self.roots, &mut out);
        out
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
        ui.vertical_centered(|ui| {
            ui.add_space(16.0);

            egui::Frame::default()
                .fill(CARD_BG)
                .rounding(16.0)
                .inner_margin(egui::Margin::same(32.0))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(38, 50, 72)))
                .show(ui, |ui| {
                    ui.set_width(340.0);

                    if !self.needs_2fa {
                        // ── Step 1: Username + Password ──
                        ui.label(
                            egui::RichText::new("Sign In")
                                .size(24.0)
                                .color(TEXT_PRIMARY)
                                .strong(),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Enter your AGBroadband credentials")
                                .size(13.0)
                                .color(TEXT_SECONDARY),
                        );
                        ui.add_space(24.0);

                        ui.label(egui::RichText::new("Username").size(12.0).color(TEXT_SECONDARY));
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.username)
                                .desired_width(320.0)
                                .hint_text("Enter your username")
                                .margin(egui::Margin::symmetric(12.0, 10.0)),
                        );
                        ui.add_space(16.0);

                        ui.label(egui::RichText::new("Password").size(12.0).color(TEXT_SECONDARY));
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.password)
                                .desired_width(320.0)
                                .password(true)
                                .hint_text("Enter your password")
                                .margin(egui::Margin::symmetric(12.0, 10.0)),
                        );
                        ui.add_space(16.0);
                    } else {
                        // ── Step 2: Verification Code only ──
                        ui.label(
                            egui::RichText::new("Verification")
                                .size(24.0)
                                .color(TEXT_PRIMARY)
                                .strong(),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Enter the code sent to your email")
                                .size(13.0)
                                .color(TEXT_SECONDARY),
                        );
                        ui.add_space(24.0);

                        ui.label(egui::RichText::new("Verification Code").size(12.0).color(ACCENT));
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.verification_code)
                                .desired_width(320.0)
                                .hint_text("Enter code from email")
                                .margin(egui::Margin::symmetric(12.0, 10.0)),
                        );
                        ui.add_space(16.0);
                    }

                    // Info message
                    if !self.login_info.is_empty() {
                        egui::Frame::default()
                            .fill(egui::Color32::from_rgb(0, 50, 70))
                            .rounding(8.0)
                            .inner_margin(egui::Margin::same(10.0))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new(&self.login_info).size(12.0).color(ACCENT));
                            });
                        ui.add_space(8.0);
                    }

                    // Error message
                    if !self.login_error.is_empty() {
                        egui::Frame::default()
                            .fill(egui::Color32::from_rgb(60, 20, 20))
                            .rounding(8.0)
                            .inner_margin(egui::Margin::same(10.0))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new(&self.login_error).size(12.0).color(ERROR_COLOR));
                            });
                        ui.add_space(8.0);
                    }

                    // Submit button
                    ui.add_space(8.0);
                    let button_text = if self.login_loading {
                        if self.needs_2fa { "Verifying..." } else { "Signing in..." }
                    } else if self.needs_2fa {
                        "Verify"
                    } else {
                        "Sign In"
                    };

                    let can_submit = !self.login_loading && if self.needs_2fa {
                        !self.verification_code.is_empty()
                    } else {
                        !self.username.is_empty() && !self.password.is_empty()
                    };

                    let btn = filled_button(button_text, can_submit)
                        .min_size(egui::vec2(320.0, 42.0));

                    let btn_response = ui.add_enabled(can_submit, btn);

                    let enter_pressed = ui.input(|i: &egui::InputState| i.key_pressed(egui::Key::Enter));
                    if (btn_response.clicked() || enter_pressed) && can_submit {
                        self.attempt_login();
                    }

                    if btn_response.hovered() && can_submit {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                });
        });
    }

    fn render_select_folders(&mut self, ui: &mut egui::Ui) {
        // Disk info bar
        if self.disk_total > 0 {
            let used = self.disk_total - self.disk_free;
            let selected = self.selected_size();
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
                egui::TextEdit::singleline(&mut self.search_query)
                    .desired_width(200.0)
                    .hint_text("Search..."),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let count = self.selected_policies.len();
                ui.label(egui::RichText::new(format!("{count} item(s) selected")).size(12.0).color(TEXT_SECONDARY));
            });
        });
        ui.add_space(6.0);

        if self.tree_loading && self.roots.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.spinner();
                ui.label(egui::RichText::new("Loading folders...").size(14.0).color(TEXT_SECONDARY));
            });
            return;
        }
        if !self.tree_error.is_empty() {
            ui.label(egui::RichText::new(&self.tree_error).color(ERROR_COLOR));
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

            let count = self.selected_policies.values().filter(|p| p.is_some()).count();
            let total_size = self.selected_size();

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
        // Start sync engine on first render of this step
        self.start_sync();

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

        // Live progress card
        if let Ok(progress) = self.progress.lock() {
            let phase = progress.phase.clone();
            let files_done = progress.files_done;
            let files_total = progress.files_total;
            let current_folder = progress.current_folder.clone();
            let current_file = progress.current_file.clone();

            card(ui, |ui| {
                // Status header
                ui.horizontal(|ui| {
                    match &phase {
                        SyncPhase::Idle => {
                            // Green dot
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
                    let frac = files_done as f32 / files_total as f32;
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

            // Request repaint while actively syncing
            if phase == SyncPhase::Syncing {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
            }
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

        self.poll_login();
        self.poll_tree();

        if self.done || self.cancelled {
            // Don't send viewport commands here — WizardWrapper handles exit via process::exit(0)
            // Sending Visible(false)/Close causes visual flashes before exit takes effect
            return;
        }
        if self.login_loading || self.tree_loading {
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
                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        if self.step == WizardStep::Syncing {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let close_btn = filled_button("Close & Continue in Tray", true)
                                    .min_size(egui::vec2(220.0, 40.0));
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

/// Wizard result shared back to caller
struct WizardResult {
    completed: bool,
    sync_folder: String,
    selected_folders: Vec<FolderSelection>,
    add_explorer_sidebar: bool,
    create_desktop_shortcut: bool,
    start_with_windows: bool,
    /// If true, sync engine was already started inside the wizard
    sync_already_started: bool,
}

/// Show the setup wizard. Blocks until user finishes or cancels.
/// Returns the SharedProgress so caller can pass it to tray without re-creating the engine.
pub fn show_setup_wizard(auth: &AuthState, config: &mut AppConfig, rt: &Runtime) -> (SharedProgress, bool) {
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

    let result = Arc::new(std::sync::Mutex::new(None::<WizardResult>));
    let result_clone = result.clone();
    let progress_clone = progress.clone();

    info!("Launching wizard eframe window...");
    let run_result = eframe::run_native(
        "AGB Cloud Client — Setup Wizard",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(WizardWrapper {
                inner: WizardApp::new(&config_snapshot, auth_clone, handle, progress_clone),
                result: result_clone,
            }))
        }),
    );
    match &run_result {
        Ok(()) => info!("Wizard window closed normally"),
        Err(e) => error!("Wizard window error: {e}"),
    }

    let mut sync_already_started = false;

    // Apply wizard results to config
    if let Ok(guard) = result.lock() {
        info!("Wizard result: has_value={}", guard.is_some());
        if let Some(ref res) = *guard {
            info!("Wizard result: completed={}, folders={}, sync_started={}",
                res.completed, res.selected_folders.len(), res.sync_already_started);
            if res.completed {
                config.sync_folder = res.sync_folder.clone();
                config.selected_folders = res.selected_folders.clone();
                config.auto_start = res.start_with_windows;
                config.setup_complete = true;
                sync_already_started = res.sync_already_started;

                if let Err(e) = config.save() {
                    error!("Failed to save config after wizard: {e}");
                }
                info!("Wizard completed: {} folders selected, sync folder: {}",
                    config.selected_folders.len(), config.sync_folder);

                // Create shortcuts
                if res.add_explorer_sidebar {
                    if let Err(e) = create_explorer_sidebar(&config.sync_folder) {
                        error!("Explorer sidebar failed: {e}");
                    }
                }
                if res.create_desktop_shortcut {
                    if let Err(e) = create_desktop_shortcut(&config.sync_folder) {
                        error!("Desktop shortcut failed: {e}");
                    }
                }
                if let Err(e) = set_auto_start(res.start_with_windows) {
                    error!("Auto-start setup failed: {e}");
                }
            }
        }
    }

    (progress, sync_already_started)
}

struct WizardWrapper {
    inner: WizardApp,
    result: Arc<std::sync::Mutex<Option<WizardResult>>>,
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
                // Cancelled — just exit
                std::process::exit(0);
            }
        }
    }
}
