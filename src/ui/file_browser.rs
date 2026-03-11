use std::sync::Arc;
use eframe::egui;
use tokio::runtime::Runtime;
use tracing::info;

use crate::auth::AuthState;
use crate::config::AppConfig;
use crate::models::{CloudFile, FolderSelection};
use crate::ui::common::*;
use crate::ui::folder_tree::FolderTreeWidget;

struct FileBrowserApp {
    tree: FolderTreeWidget,
    done: bool,
    saved_selections: Vec<FolderSelection>,
    disk_total: u64,
    disk_free: u64,
    sync_folder: String,
    /// Currently selected file/folder for the right-side preview
    preview_file: Option<CloudFile>,
}

impl FileBrowserApp {
    fn new(auth: AuthState, handle: tokio::runtime::Handle, sync_folder: &str, saved_folders: &[FolderSelection]) -> Self {
        let (disk_total, disk_free) = get_disk_space(sync_folder).unwrap_or((0, 0));
        let mut tree = FolderTreeWidget::new(auth, handle)
            .with_initial_selections(saved_folders)
            .with_auto_expand(false); // Browse mode: don't shift the view by auto-expanding pre-selected folders
        tree.fetch_roots();
        Self {
            tree,
            done: false,
            saved_selections: Vec::new(),
            disk_total,
            disk_free,
            sync_folder: sync_folder.to_string(),
            preview_file: None,
        }
    }
}

impl eframe::App for FileBrowserApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Install image loaders once (needed for image preview via egui_extras)
        egui_extras::install_image_loaders(ctx);

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = DARK_BG;
        visuals.override_text_color = Some(TEXT_PRIMARY);
        ctx.set_visuals(visuals);

        self.tree.poll(ctx);

        if self.done {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.tree.is_loading { ctx.request_repaint(); }

        // ── Header ──────────────────────────────────────────────────────────
        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::default().fill(SURFACE).inner_margin(egui::Margin::same(10.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Select Folders to Sync").size(20.0).color(ACCENT).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.tree.search_query)
                                .desired_width(180.0)
                                .hint_text("Search..."),
                        );
                    });
                });
            });

        // ── Footer ──────────────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("footer")
            .frame(egui::Frame::default().fill(SURFACE).inner_margin(egui::Margin::same(10.0)))
            .show(ctx, |ui| {
                if self.disk_total > 0 {
                    let used = self.disk_total - self.disk_free;
                    let selected = self.tree.selected_size();
                    let total_f = self.disk_total as f32;
                    let bar_h = 12.0;
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
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("Selected: {}", format_size(selected as i64))).size(10.0).color(ACCENT));
                        ui.label(egui::RichText::new(" | ").size(10.0).color(DIVIDER));
                        ui.label(egui::RichText::new(format!("Free: {}", format_size(self.disk_free as i64))).size(10.0).color(SUCCESS_COLOR));
                        ui.label(egui::RichText::new(" | ").size(10.0).color(DIVIDER));
                        ui.label(egui::RichText::new(format!("Total: {}", format_size(self.disk_total as i64))).size(10.0).color(TEXT_SECONDARY));
                    });
                    if selected > self.disk_free {
                        ui.label(egui::RichText::new("! Not enough disk space!").size(11.0).color(WARNING_COLOR));
                    }
                    ui.add_space(4.0);
                }

                let unset = self.tree.unset_count();
                if unset > 0 {
                    ui.label(egui::RichText::new(format!(
                        "! {unset} item(s) need a Copy or Sync policy"
                    )).size(11.0).color(WARNING_COLOR));
                    ui.add_space(4.0);
                }

                ui.horizontal(|ui| {
                    let count = self.tree.build_selections().len();
                    ui.label(egui::RichText::new(format!("{count} item(s) selected")).size(12.0).color(TEXT_SECONDARY));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let has_selections = count > 0 && unset == 0;
                        let btn = egui::Button::new(
                            egui::RichText::new("Confirm & Start Sync").size(14.0).color(TEXT_PRIMARY),
                        )
                        .min_size(egui::vec2(180.0, 36.0))
                        .rounding(8.0)
                        .fill(if has_selections { ACCENT } else { egui::Color32::from_rgb(50, 70, 100) });
                        if ui.add_enabled(has_selections, btn)
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            self.saved_selections = self.tree.build_selections();
                            info!("User selected {} items for sync", self.saved_selections.len());
                            // Save selections to disk immediately, while still inside eframe.
                            // The code that runs AFTER eframe::run_native returns may never
                            // execute if the GPU destructor crashes the process (same pattern
                            // as the --login GPU teardown fix). Saving here — before sending
                            // ViewportCommand::Close — guarantees the data hits disk.
                            match crate::config::AppConfig::load_or_create() {
                                Ok(mut cfg) => {
                                    cfg.selected_folders = self.saved_selections.clone();
                                    if let Err(e) = cfg.save() {
                                        tracing::error!("Failed to save folder selections: {e}");
                                    } else {
                                        info!("Folder selections saved to config ({} items)", self.saved_selections.len());
                                    }
                                }
                                Err(e) => tracing::error!("Failed to load config for save: {e}"),
                            }
                            self.done = true;
                        }
                    });
                });
            });

        // ── Left panel: folder tree (resizable) ──────────────────────────────
        egui::SidePanel::left("tree_panel")
            .resizable(true)
            .min_width(220.0)
            .default_width(400.0)
            .frame(egui::Frame::default().fill(DARK_BG).inner_margin(egui::Margin::same(10.0)))
            .show(ctx, |ui| {
                if let Some(file) = self.tree.render(ui) {
                    self.preview_file = Some(file);
                }
            });

        // ── Right panel: preview ─────────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(SURFACE).inner_margin(egui::Margin::same(16.0)))
            .show(ctx, |ui| {
                render_preview(ui, &self.preview_file, &self.sync_folder, ctx);
            });
    }
}

// ── Preview panel ────────────────────────────────────────────────────────────

fn render_preview(ui: &mut egui::Ui, file: &Option<CloudFile>, sync_folder: &str, ctx: &egui::Context) {
    match file {
        None => {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() / 3.0);
                let (icon_rect, _) = ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
                let p = ui.painter();
                p.circle_filled(icon_rect.center(), 28.0, SURFACE_VARIANT);
                p.text(
                    icon_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "\u{1F4C4}",
                    egui::FontId::proportional(28.0),
                    TEXT_DISABLED,
                );
                ui.add_space(12.0);
                ui.label(egui::RichText::new("Click a file or folder to preview").size(14.0).color(TEXT_SECONDARY));
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Drag the divider to resize panels").size(11.0).color(TEXT_DISABLED));
            });
        }
        Some(cf) => {
            egui::ScrollArea::vertical().show(ui, |ui| {
                // Large icon
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    let (icon_rect, _) = ui.allocate_exact_size(egui::vec2(72.0, 72.0), egui::Sense::hover());
                    let p = ui.painter();
                    p.circle_filled(icon_rect.center(), 36.0, SURFACE_VARIANT);
                    p.text(
                        icon_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        file_type_icon(cf),
                        egui::FontId::proportional(36.0),
                        TEXT_PRIMARY,
                    );
                    ui.add_space(10.0);
                    ui.label(egui::RichText::new(&cf.name).size(15.0).color(TEXT_PRIMARY).strong());
                });

                ui.add_space(14.0);
                divider(ui);
                ui.add_space(10.0);

                // Metadata card
                card(ui, |ui| {
                    meta_row(ui, "Type", &file_type_label(cf));
                    if let Some(sz) = cf.size {
                        meta_row(ui, "Size", &format_size(sz));
                    }
                    if let Some(ref ext) = cf.ext {
                        meta_row(ui, "Extension", &format!(".{}", ext.to_uppercase()));
                    }
                    if let Some(ref mime) = cf.mime {
                        meta_row(ui, "MIME", mime.as_str());
                    }
                    if let Some(dt) = cf.created {
                        meta_row(ui, "Created", &dt.format("%b %d, %Y  %H:%M").to_string());
                    }
                    if let Some(dt) = cf.updated {
                        meta_row(ui, "Modified", &dt.format("%b %d, %Y  %H:%M").to_string());
                    }
                    // Show short UUID
                    let short_id = &cf.uuid[..cf.uuid.len().min(18)];
                    meta_row(ui, "ID", short_id);
                });

                ui.add_space(12.0);

                // Image / PDF preview
                if !cf.folder {
                    if is_image_type(cf) {
                        match find_local_file(cf, sync_folder) {
                            Some(ref path_str) => {
                                ui.vertical_centered(|ui| {
                                    ui.label(egui::RichText::new("Preview").size(11.0).color(TEXT_SECONDARY));
                                    ui.add_space(6.0);
                                });
                                let uri = format!("file:///{}", path_str.replace('\\', "/"));
                                let img = egui::Image::from_uri(uri)
                                    .fit_to_fraction(egui::vec2(1.0, 0.7))
                                    .rounding(egui::Rounding::same(8.0));
                                ui.add(img);
                                // Repaint while texture loads
                                ctx.request_repaint_after(std::time::Duration::from_millis(80));
                            }
                            None => {
                                not_synced_banner(ui, "\u{1F5BC}", "Image preview available after sync");
                            }
                        }
                    } else if is_pdf_type(cf) {
                        not_synced_banner(ui, "\u{1F4CB}", "PDF preview available after sync");
                    }
                }

                // Sync badge
                ui.add_space(10.0);
                let local = !cf.folder && find_local_file(cf, sync_folder).is_some();
                let (badge_color, badge_text) = if cf.folder {
                    (TEXT_SECONDARY, "Folder")
                } else if local {
                    (SUCCESS_COLOR, "\u{2714} Synced locally")
                } else {
                    (TEXT_DISABLED, "Not yet synced")
                };
                ui.vertical_centered(|ui| {
                    egui::Frame::default()
                        .fill(badge_color.gamma_multiply(0.15))
                        .stroke(egui::Stroke::new(1.0, badge_color.gamma_multiply(0.5)))
                        .rounding(egui::Rounding::same(12.0))
                        .inner_margin(egui::Margin::symmetric(12.0, 4.0))
                        .show(ui, |ui: &mut egui::Ui| {
                            ui.label(egui::RichText::new(badge_text).size(11.0).color(badge_color));
                        });
                });
            });
        }
    }
}

fn not_synced_banner(ui: &mut egui::Ui, icon: &str, msg: &str) {
    egui::Frame::default()
        .fill(SURFACE_VARIANT)
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::same(14.0))
        .show(ui, |ui: &mut egui::Ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(icon).size(30.0));
                ui.add_space(4.0);
                ui.label(egui::RichText::new(msg).size(12.0).color(TEXT_SECONDARY));
            });
        });
}

fn meta_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{label}:")).size(11.0).color(TEXT_SECONDARY));
        ui.add_space(6.0);
        ui.label(egui::RichText::new(value).size(12.0).color(TEXT_PRIMARY));
    });
    ui.add_space(3.0);
}

fn file_type_icon(cf: &CloudFile) -> &'static str {
    if cf.folder { return "\u{1F4C1}"; }
    let ext = cf.ext.as_deref().unwrap_or("").to_lowercase();
    let mime = cf.mime.as_deref().unwrap_or("").to_lowercase();
    if mime.starts_with("image/") || matches!(ext.as_str(), "jpg"|"jpeg"|"png"|"gif"|"webp"|"bmp"|"svg"|"heic") {
        "\u{1F5BC}"
    } else if mime == "application/pdf" || ext == "pdf" {
        "\u{1F4CB}"
    } else if mime.starts_with("video/") || matches!(ext.as_str(), "mp4"|"avi"|"mov"|"mkv"|"webm") {
        "\u{1F3AC}"
    } else if mime.starts_with("audio/") || matches!(ext.as_str(), "mp3"|"wav"|"flac"|"ogg"|"m4a") {
        "\u{1F3B5}"
    } else if matches!(ext.as_str(), "zip"|"rar"|"7z"|"tar"|"gz") {
        "\u{1F4E6}"
    } else {
        "\u{1F4C4}"
    }
}

fn file_type_label(cf: &CloudFile) -> String {
    if cf.folder { return "Folder".to_string(); }
    let ext = cf.ext.as_deref().unwrap_or("").to_lowercase();
    let mime = cf.mime.as_deref().unwrap_or("").to_lowercase();
    if mime.starts_with("image/") || matches!(ext.as_str(), "jpg"|"jpeg"|"png"|"gif"|"webp"|"bmp"|"svg"|"heic") {
        "Image".to_string()
    } else if mime == "application/pdf" || ext == "pdf" {
        "PDF Document".to_string()
    } else if mime.starts_with("video/") || matches!(ext.as_str(), "mp4"|"avi"|"mov"|"mkv"|"webm") {
        "Video".to_string()
    } else if mime.starts_with("audio/") || matches!(ext.as_str(), "mp3"|"wav"|"flac"|"ogg"|"m4a") {
        "Audio".to_string()
    } else if matches!(ext.as_str(), "zip"|"rar"|"7z"|"tar"|"gz") {
        "Archive".to_string()
    } else if ext.is_empty() {
        "File".to_string()
    } else {
        format!("{} File", ext.to_uppercase())
    }
}

fn is_image_type(cf: &CloudFile) -> bool {
    let ext = cf.ext.as_deref().unwrap_or("").to_lowercase();
    let mime = cf.mime.as_deref().unwrap_or("").to_lowercase();
    mime.starts_with("image/") || matches!(ext.as_str(), "jpg"|"jpeg"|"png"|"gif"|"webp"|"bmp")
}

fn is_pdf_type(cf: &CloudFile) -> bool {
    let ext = cf.ext.as_deref().unwrap_or("").to_lowercase();
    let mime = cf.mime.as_deref().unwrap_or("").to_lowercase();
    mime == "application/pdf" || ext == "pdf"
}

/// Try to find the file in the local sync folder.
/// Checks direct path first, then one level of subdirectories.
fn find_local_file(cf: &CloudFile, sync_folder: &str) -> Option<String> {
    if cf.folder { return None; }
    let direct = std::path::Path::new(sync_folder).join(&cf.name);
    if direct.exists() {
        return Some(direct.to_string_lossy().into_owned());
    }
    if let Ok(entries) = std::fs::read_dir(sync_folder) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let candidate = entry.path().join(&cf.name);
                if candidate.exists() {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Show the file browser window. Blocks until user confirms or closes.
pub fn show_file_browser(auth: &AuthState, config: &mut AppConfig, rt: &Runtime) {
    let handle = rt.handle().clone();
    let auth_clone = auth.clone();
    let sync_folder = config.sync_folder.clone();
    let saved_folders = config.selected_folders.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 580.0])
            .with_min_inner_size([600.0, 400.0])
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
