use std::collections::HashMap;
use std::sync::Arc;
use eframe::egui;
use tokio::runtime::Runtime;
use tracing::{error, info};

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::models::{FolderSelection, SyncPolicy};
use crate::sync::remote::RemoteClient;
use crate::ui::common::*;

struct FileBrowserApp {
    roots: Vec<TreeNode>,
    selected_policies: HashMap<String, Option<SyncPolicy>>,
    is_loading: bool,
    error_message: String,
    done: bool,
    saved_selections: Vec<FolderSelection>,
    auth: AuthState,
    handle: tokio::runtime::Handle,
    result_rx: Option<std::sync::mpsc::Receiver<FetchResult>>,
    search_query: String,
    disk_total: u64,
    disk_free: u64,
}

impl FileBrowserApp {
    fn new(auth: AuthState, handle: tokio::runtime::Handle, sync_folder: &str, saved_folders: &[FolderSelection]) -> Self {
        let (disk_total, disk_free) = get_disk_space(sync_folder).unwrap_or((0, 0));
        let selected_policies: HashMap<String, Option<SyncPolicy>> = saved_folders
            .iter()
            .map(|f| (f.uuid.clone(), Some(f.policy.clone())))
            .collect();
        let mut app = Self {
            roots: Vec::new(),
            selected_policies,
            is_loading: false,
            error_message: String::new(),
            done: false,
            saved_selections: Vec::new(),
            auth,
            handle,
            result_rx: None,
            search_query: String::new(),
            disk_total,
            disk_free,
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

    fn poll_result(&mut self) {
        let result = self.result_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = result {
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
                    self.selected_policies.insert(uuid.clone(), Some(SyncPolicy::KeepSynced { interval_secs: 30 }));
                    for d in collect_descendant_uuids(&self.roots, &uuid) {
                        self.selected_policies.entry(d).or_insert(Some(SyncPolicy::KeepSynced { interval_secs: 30 }));
                    }
                    for a in find_ancestor_uuids(&self.roots, &uuid) {
                        self.selected_policies.entry(a).or_insert(None);
                    }
                }
                TreeAction::Deselect(uuid) => {
                    self.selected_policies.remove(&uuid);
                    for d in collect_descendant_uuids(&self.roots, &uuid) {
                        self.selected_policies.remove(&d);
                    }
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

impl eframe::App for FileBrowserApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = DARK_BG;
        visuals.override_text_color = Some(TEXT_PRIMARY);
        ctx.set_visuals(visuals);

        self.poll_result();

        if self.done {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.is_loading { ctx.request_repaint(); }

        // Top header
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Select Folders to Sync").size(20.0).color(ACCENT).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search_query)
                            .desired_width(200.0)
                            .hint_text("Search..."),
                    );
                });
            });
            ui.add_space(8.0);
        });

        // Bottom footer
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.add_space(8.0);

            if self.disk_total > 0 {
                let used = self.disk_total - self.disk_free;
                let selected = self.selected_size();
                let total_f = self.disk_total as f32;

                let bar_h = 14.0;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), bar_h), egui::Sense::hover());
                let p = ui.painter();
                p.rect_filled(rect, 3.0, BAR_BG);
                let uw = rect.width() * (used as f32 / total_f);
                p.rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(uw, bar_h)), 3.0, BAR_USED);
                let sw = rect.width() * (selected as f32 / total_f);
                p.rect_filled(egui::Rect::from_min_size(
                    egui::pos2(rect.min.x + uw, rect.min.y),
                    egui::vec2(sw.min(rect.width() - uw), bar_h),
                ), 0.0, ACCENT);

                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("Selected: {}", format_size(selected as i64))).size(10.0).color(ACCENT));
                    ui.label(egui::RichText::new(" | ").size(10.0).color(TEXT_SECONDARY));
                    ui.label(egui::RichText::new(format!("Free: {}", format_size(self.disk_free as i64))).size(10.0).color(SUCCESS_COLOR));
                    ui.label(egui::RichText::new(" | ").size(10.0).color(TEXT_SECONDARY));
                    ui.label(egui::RichText::new(format!("Total: {}", format_size(self.disk_total as i64))).size(10.0).color(TEXT_SECONDARY));
                });

                if selected > self.disk_free {
                    ui.label(egui::RichText::new("! Not enough disk space!").size(11.0).color(WARNING_COLOR));
                }
                ui.add_space(6.0);
            }

            let unset = self.selected_policies.iter()
                .filter(|(_, p)| p.is_none())
                .filter(|(uuid, _)| !has_selected_descendant(&self.roots, uuid, &self.selected_policies))
                .count();
            if unset > 0 {
                ui.label(egui::RichText::new(format!(
                    "! {unset} item(s) need a Copy or Sync policy"
                )).size(11.0).color(WARNING_COLOR));
                ui.add_space(4.0);
            }

            ui.horizontal(|ui| {
                let count = self.selected_policies.values().filter(|p| p.is_some()).count();
                ui.label(egui::RichText::new(format!("{count} item(s) selected")).size(12.0).color(TEXT_SECONDARY));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let has_selections = count > 0 && unset == 0;
                    let btn = egui::Button::new(
                        egui::RichText::new("Confirm & Start Sync").size(15.0).color(TEXT_PRIMARY),
                    )
                    .min_size(egui::vec2(180.0, 36.0))
                    .rounding(6.0)
                    .fill(if has_selections { ACCENT } else { egui::Color32::from_rgb(60, 80, 110) });
                    if ui.add_enabled(has_selections, btn)
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        self.saved_selections = self.build_selections();
                        info!("User selected {} items for sync", self.saved_selections.len());
                        self.done = true;
                    }
                });
            });
            ui.add_space(10.0);
        });

        // Central tree
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(DARK_BG).inner_margin(egui::Margin::same(12.0)))
            .show(ctx, |ui| {
                if self.is_loading && self.roots.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(60.0);
                        ui.spinner();
                        ui.label(egui::RichText::new("Loading folders...").size(14.0).color(TEXT_SECONDARY));
                    });
                    return;
                }

                if !self.error_message.is_empty() {
                    ui.label(egui::RichText::new(&self.error_message).color(egui::Color32::from_rgb(255, 90, 90)));
                    if ui.button("Retry").clicked() { self.fetch_roots(); }
                    return;
                }

                let mut actions = Vec::new();
                let search = self.search_query.clone();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    render_tree(ui, &mut self.roots, &self.selected_policies, 0, &mut actions, &search);
                });

                self.process_tree_actions(actions);
            });
    }
}

/// Show the file browser / setup wizard. Blocks until user confirms.
pub fn show_file_browser(auth: &AuthState, config: &mut AppConfig, rt: &Runtime) {
    let handle = rt.handle().clone();
    let auth_clone = auth.clone();
    let sync_folder = config.sync_folder.clone();
    let saved_folders = config.selected_folders.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([700.0, 550.0])
            .with_min_inner_size([500.0, 400.0])
            .with_title("AGB Cloud Client - Select Folders")
            .with_icon(Arc::new(crate::ui::icon::app_icon_data())),
        ..Default::default()
    };

    let selections = Arc::new(std::sync::Mutex::new(Vec::<FolderSelection>::new()));
    let selections_clone = selections.clone();

    let _ = eframe::run_native(
        "AGB Cloud Client - Select Folders",
        options,
        Box::new(move |_cc| {
            let app = FileBrowserApp::new(auth_clone, handle, &sync_folder, &saved_folders);
            Ok(Box::new(BrowserWrapper { inner: app, selections: selections_clone }))
        }),
    );

    if let Ok(sels) = selections.lock() {
        if !sels.is_empty() {
            config.selected_folders = sels.clone();
            info!("Saved {} folder selections to config", config.selected_folders.len());
        }
    }
}

struct BrowserWrapper {
    inner: FileBrowserApp,
    selections: Arc<std::sync::Mutex<Vec<FolderSelection>>>,
}

impl eframe::App for BrowserWrapper {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.inner.update(ctx, frame);
        if self.inner.done {
            if let Ok(mut sels) = self.selections.lock() {
                *sels = self.inner.saved_selections.clone();
            }
        }
    }
}
