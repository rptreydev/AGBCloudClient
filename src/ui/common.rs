//! Shared UI types, brand colors, tree rendering, and helpers.
//!
//! Extracted from login.rs, file_browser.rs, and settings.rs to avoid duplication
//! and provide a single source of truth for the wizard and all UI windows.

use std::collections::HashMap;
use eframe::egui;
use tracing::info;

use crate::models::{CloudFile, FolderSelection, SyncPolicy};

// ── Brand colors (Material Design 3 dark theme) ──

pub const DARK_BG: egui::Color32 = egui::Color32::from_rgb(16, 20, 30);
pub const SURFACE: egui::Color32 = egui::Color32::from_rgb(22, 28, 42);
pub const SURFACE_VARIANT: egui::Color32 = egui::Color32::from_rgb(30, 38, 56);
pub const CARD_BG: egui::Color32 = egui::Color32::from_rgb(26, 34, 52);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0, 190, 215);
pub const ACCENT_DIM: egui::Color32 = egui::Color32::from_rgb(0, 140, 160);
pub const TEXT_PRIMARY: egui::Color32 = egui::Color32::from_rgb(230, 235, 245);
pub const TEXT_SECONDARY: egui::Color32 = egui::Color32::from_rgb(140, 155, 180);
pub const TEXT_DISABLED: egui::Color32 = egui::Color32::from_rgb(80, 95, 115);
pub const SUCCESS_COLOR: egui::Color32 = egui::Color32::from_rgb(80, 200, 120);
pub const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 90, 90);
pub const WARNING_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 180, 50);
pub const FOLDER_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 200, 60);
pub const FOLDER_OPEN_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 220, 100);
pub const FILE_COLOR: egui::Color32 = egui::Color32::from_rgb(140, 170, 210);
pub const BAR_BG: egui::Color32 = egui::Color32::from_rgb(30, 40, 60);
pub const BAR_USED: egui::Color32 = egui::Color32::from_rgb(60, 75, 100);
pub const BTN_BG: egui::Color32 = egui::Color32::from_rgb(36, 48, 72);
pub const BTN_BORDER: egui::Color32 = egui::Color32::from_rgb(55, 70, 100);
pub const BTN_HOVER: egui::Color32 = egui::Color32::from_rgb(44, 58, 85);
pub const INPUT_BG: egui::Color32 = egui::Color32::from_rgb(18, 24, 40);
pub const INPUT_BORDER: egui::Color32 = egui::Color32::from_rgb(50, 65, 90);
pub const DIVIDER: egui::Color32 = egui::Color32::from_rgb(40, 52, 72);
pub const TITLE_BAR_BG: egui::Color32 = egui::Color32::from_rgb(12, 16, 24);

// ── Visuals ──

/// Configure egui visuals to match the Material Design 3 dark theme.
pub fn configure_visuals(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = DARK_BG;
    visuals.window_fill = CARD_BG;
    visuals.widgets.noninteractive.bg_fill = SURFACE_VARIANT;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, INPUT_BORDER);
    visuals.widgets.inactive.bg_fill = INPUT_BG;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, INPUT_BORDER);
    visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
    visuals.widgets.hovered.bg_fill = BTN_HOVER;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT_DIM);
    visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(40, 55, 80);
    visuals.widgets.active.rounding = egui::Rounding::same(8.0);
    visuals.override_text_color = Some(TEXT_PRIMARY);
    visuals.selection.bg_fill = egui::Color32::from_rgb(0, 100, 120);
    visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    ctx.set_visuals(visuals);
}

// ── Tree types ──

pub enum FetchResult {
    Roots(Vec<CloudFile>),
    Children(String, Vec<CloudFile>),
    Error(String),
}

pub enum TreeAction {
    Select(String),
    Deselect(String),
    SetPolicy(String, SyncPolicy),
    FetchChildren(String),
}

pub struct TreeNode {
    pub file: CloudFile,
    pub children_loaded: bool,
    pub expanded: bool,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    pub fn from_cloud_file(file: CloudFile) -> Self {
        let children: Vec<TreeNode> = file
            .children
            .as_ref()
            .map(|c| c.iter().map(|f| TreeNode::from_cloud_file(f.clone())).collect())
            .unwrap_or_default();
        let children_loaded = !children.is_empty();
        Self { file, children_loaded, expanded: false, children }
    }
}

// ── Painted vector icons ──

pub fn paint_folder_icon(ui: &mut egui::Ui, expanded: bool) {
    let size = egui::vec2(16.0, 13.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    if !ui.is_rect_visible(rect) { return; }
    let p = ui.painter();
    // Tab (top-left)
    p.rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(7.0, 3.0)),
        1.0, FOLDER_COLOR,
    );
    // Body
    p.rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(rect.min.x, rect.min.y + 3.0),
            egui::vec2(16.0, 10.0),
        ),
        1.0, FOLDER_COLOR,
    );
    if expanded {
        // Front flap (lighter, offset)
        p.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(rect.min.x + 3.0, rect.min.y + 5.0),
                egui::vec2(13.0, 8.0),
            ),
            1.0, FOLDER_OPEN_COLOR,
        );
    }
}

pub fn paint_file_icon(ui: &mut egui::Ui) {
    let size = egui::vec2(12.0, 15.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    if !ui.is_rect_visible(rect) { return; }
    let p = ui.painter();
    p.rect_filled(rect, 1.0, FILE_COLOR);
    // Dog-ear corner (top-right triangle)
    let c = 4.0;
    let points = vec![
        egui::pos2(rect.max.x - c, rect.min.y),
        egui::pos2(rect.max.x, rect.min.y + c),
        egui::pos2(rect.max.x, rect.min.y),
    ];
    p.add(egui::Shape::convex_polygon(points, DARK_BG, egui::Stroke::NONE));
}

/// Custom-painted expand/collapse button with triangle arrow.
pub fn tree_expand_button(ui: &mut egui::Ui, expanded: bool) -> egui::Response {
    let size = egui::vec2(20.0, 20.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let bg = if hovered { BTN_HOVER } else { BTN_BG };
    let p = ui.painter();
    p.rect_filled(rect, 2.0, bg);
    p.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, BTN_BORDER));

    let c = rect.center();
    let arrow_points = if expanded {
        vec![
            egui::pos2(c.x - 4.0, c.y - 2.0),
            egui::pos2(c.x + 4.0, c.y - 2.0),
            egui::pos2(c.x, c.y + 3.0),
        ]
    } else {
        vec![
            egui::pos2(c.x - 2.0, c.y - 4.0),
            egui::pos2(c.x + 3.0, c.y),
            egui::pos2(c.x - 2.0, c.y + 4.0),
        ]
    };
    p.add(egui::Shape::convex_polygon(arrow_points, TEXT_SECONDARY, egui::Stroke::NONE));

    response
}

// ── Custom checkbox ──

/// Large, high-visibility checkbox with three visual states:
/// - `checked=true, intermediate=false` → ACCENT fill + checkmark (explicitly selected)
/// - `checked=true, intermediate=true`  → ACCENT_DIM fill + dash   (ancestor marker)
/// - `checked=false`                    → INPUT_BG (not selected)
pub fn custom_checkbox(ui: &mut egui::Ui, checked: &mut bool, intermediate: bool) -> egui::Response {
    let size = egui::vec2(20.0, 20.0);
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *checked = !*checked;
        response.mark_changed();
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    let bg = if *checked {
        if intermediate { ACCENT_DIM } else { ACCENT }
    } else {
        INPUT_BG
    };
    let border_color = if response.hovered() {
        ACCENT
    } else if *checked {
        if intermediate { ACCENT_DIM } else { ACCENT }
    } else {
        TEXT_SECONDARY
    };
    painter.rect(rect, 3.0, bg, egui::Stroke::new(1.5, border_color));
    if *checked {
        let c = rect.center();
        let stroke = egui::Stroke::new(2.5, TEXT_PRIMARY);
        if intermediate {
            // Dash — indicates "contains selected children, not directly selected"
            painter.line_segment(
                [egui::pos2(c.x - 5.0, c.y), egui::pos2(c.x + 5.0, c.y)],
                stroke,
            );
        } else {
            // Full checkmark
            painter.line_segment(
                [egui::pos2(c.x - 5.0, c.y - 0.5), egui::pos2(c.x - 1.5, c.y + 3.5)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x - 1.5, c.y + 3.5), egui::pos2(c.x + 5.0, c.y - 4.0)],
                stroke,
            );
        }
    }
    response
}

// ── Tree rendering ──

pub fn render_tree(
    ui: &mut egui::Ui,
    nodes: &mut [TreeNode],
    selected: &HashMap<String, Option<SyncPolicy>>,
    depth: usize,
    actions: &mut Vec<TreeAction>,
    search: &str,
    preview: &mut Option<CloudFile>,
) {
    for node in nodes.iter_mut() {
        if !search.is_empty() {
            let name_lower = node.file.name.to_lowercase();
            let search_lower = search.to_lowercase();
            if !name_lower.contains(&search_lower) && !any_child_matches(&node.children, &search_lower) {
                continue;
            }
        }

        let indent = depth as f32 * 22.0;
        let uuid = node.file.uuid.clone();
        // contains_key is true for both explicit selections AND ancestor markers (None),
        // so the parent appears checked when any child is selected.
        let is_selected = selected.contains_key(&uuid);
        // flatten() is None for ancestor markers — policy buttons are hidden for them,
        // preventing accidental promotion of a marker to an explicit selection.
        let policy = selected.get(&uuid).cloned().flatten();
        // Ancestor marker: checked but no direct policy → show dash (not full checkmark)
        let is_ancestor_marker = is_selected && policy.is_none();

        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(indent);

            // Checkbox only for folder nodes — files are informational, not sync targets.
            if node.file.folder {
                let mut cb_val = is_selected;
                if custom_checkbox(ui, &mut cb_val, is_ancestor_marker).changed() {
                    if cb_val {
                        actions.push(TreeAction::Select(uuid.clone()));
                    } else {
                        actions.push(TreeAction::Deselect(uuid.clone()));
                    }
                }
            } else {
                // Reserve the same horizontal space so file rows stay aligned with folder rows.
                ui.add_space(22.0);
            }
            ui.add_space(4.0);

            // Expand/collapse + folder/file icon
            if node.file.folder {
                if tree_expand_button(ui, node.expanded).clicked() {
                    node.expanded = !node.expanded;
                    if node.expanded && !node.children_loaded {
                        actions.push(TreeAction::FetchChildren(uuid.clone()));
                    }
                }
                ui.add_space(6.0);
                paint_folder_icon(ui, node.expanded);
            } else {
                ui.add_space(30.0);
                paint_file_icon(ui);
            }
            ui.add_space(8.0);

            // Name — clicking a file row sets it as the preview target
            let name_resp = ui.add(
                egui::Label::new(egui::RichText::new(&node.file.name).size(13.0).color(TEXT_PRIMARY))
                    .sense(egui::Sense::click()),
            );
            if name_resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                *preview = Some(node.file.clone());
                // Clicking the name on a folder also toggles expand
                if node.file.folder {
                    node.expanded = !node.expanded;
                    if node.expanded && !node.children_loaded {
                        actions.push(TreeAction::FetchChildren(uuid.clone()));
                    }
                }
            }

            // Policy buttons only for explicit selections (policy.is_some()).
            if policy.is_some() {
                ui.add_space(8.0);
                let is_copy = matches!(&policy, Some(SyncPolicy::Copy));
                let is_sync = matches!(&policy, Some(SyncPolicy::KeepSynced { .. }));

                if ui.add(policy_button("Copy", is_copy))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    actions.push(TreeAction::SetPolicy(uuid.clone(), SyncPolicy::Copy));
                }
                if ui.add(policy_button("Sync", is_sync))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    actions.push(TreeAction::SetPolicy(uuid.clone(), SyncPolicy::KeepSynced { interval_secs: 30 }));
                }
            }

            // Size
            if let Some(size) = node.file.size {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(format_size(size)).size(11.0).color(TEXT_SECONDARY));
                });
            }
        });

        if node.expanded && node.file.folder {
            render_tree(ui, &mut node.children, selected, depth + 1, actions, search, preview);
        }
    }
}

pub fn policy_button(label: &str, active: bool) -> egui::Button<'_> {
    let (fill, text_color, stroke_color) = if active {
        (ACCENT, TEXT_PRIMARY, ACCENT)
    } else {
        (egui::Color32::TRANSPARENT, TEXT_SECONDARY, BTN_BORDER)
    };
    egui::Button::new(egui::RichText::new(label).size(10.0).color(text_color))
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke_color))
        .rounding(10.0)
        .min_size(egui::vec2(48.0, 22.0))
}

pub fn any_child_matches(nodes: &[TreeNode], search: &str) -> bool {
    nodes.iter().any(|n| {
        n.file.name.to_lowercase().contains(search)
            || any_child_matches(&n.children, search)
    })
}

// ── Tree mutation helpers ──

pub fn insert_children(nodes: &mut [TreeNode], parent_uuid: &str, children: Vec<CloudFile>) {
    for node in nodes.iter_mut() {
        if node.file.uuid == parent_uuid {
            node.children = children.into_iter().map(TreeNode::from_cloud_file).collect();
            node.children_loaded = true;
            return;
        }
        insert_children(&mut node.children, parent_uuid, children.clone());
    }
}

// ── Cascade helpers ──

pub fn collect_descendant_uuids(roots: &[TreeNode], parent_uuid: &str) -> Vec<String> {
    fn find_and_collect(nodes: &[TreeNode], target: &str) -> Option<Vec<String>> {
        for node in nodes {
            if node.file.uuid == target {
                let mut out = Vec::new();
                collect_all(&node.children, &mut out);
                return Some(out);
            }
            if let Some(found) = find_and_collect(&node.children, target) {
                return Some(found);
            }
        }
        None
    }
    fn collect_all(nodes: &[TreeNode], out: &mut Vec<String>) {
        for node in nodes {
            out.push(node.file.uuid.clone());
            collect_all(&node.children, out);
        }
    }
    find_and_collect(roots, parent_uuid).unwrap_or_default()
}

pub fn find_ancestor_uuids(roots: &[TreeNode], target: &str) -> Vec<String> {
    fn search(nodes: &[TreeNode], target: &str, path: &mut Vec<String>) -> bool {
        for node in nodes {
            path.push(node.file.uuid.clone());
            if node.file.uuid == target {
                path.pop();
                return true;
            }
            if search(&node.children, target, path) { return true; }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    search(roots, target, &mut path);
    path
}

/// Check if any descendant of `parent_uuid` has a real policy (Some(Some(_))) in `selected`.
/// Used to determine if an ancestor marker (None) should be kept or cleaned up.
pub fn has_selected_descendant(
    roots: &[TreeNode],
    parent_uuid: &str,
    selected: &HashMap<String, Option<SyncPolicy>>,
) -> bool {
    fn find_node<'a>(nodes: &'a [TreeNode], uuid: &str) -> Option<&'a TreeNode> {
        for node in nodes {
            if node.file.uuid == uuid { return Some(node); }
            if let Some(found) = find_node(&node.children, uuid) { return Some(found); }
        }
        None
    }
    fn check(node: &TreeNode, selected: &HashMap<String, Option<SyncPolicy>>) -> bool {
        for child in &node.children {
            if matches!(selected.get(&child.file.uuid), Some(Some(_))) { return true; }
            if check(child, selected) { return true; }
        }
        false
    }
    find_node(roots, parent_uuid).is_some_and(|node| check(node, selected))
}

// ── Size / path helpers ──

pub fn calc_selected_size(nodes: &[TreeNode], selected: &HashMap<String, Option<SyncPolicy>>) -> u64 {
    let mut total = 0u64;
    for node in nodes {
        if matches!(selected.get(&node.file.uuid), Some(Some(_))) {
            total += node_total_size(node);
        } else {
            total += calc_selected_size(&node.children, selected);
        }
    }
    total
}

pub fn node_total_size(node: &TreeNode) -> u64 {
    let own = node.file.size.unwrap_or(0).max(0) as u64;
    let children: u64 = node.children.iter().map(node_total_size).sum();
    own + children
}

pub fn build_path(roots: &[TreeNode], target_uuid: &str) -> String {
    fn find(nodes: &[TreeNode], target: &str, current: &str) -> Option<String> {
        for node in nodes {
            let path = if current.is_empty() {
                node.file.name.clone()
            } else {
                format!("{current} / {}", node.file.name)
            };
            if node.file.uuid == target { return Some(path); }
            if let Some(found) = find(&node.children, target, &path) { return Some(found); }
        }
        None
    }
    find(roots, target_uuid, "").unwrap_or_default()
}

pub fn collect_selections(
    nodes: &[TreeNode],
    selected: &HashMap<String, Option<SyncPolicy>>,
    roots: &[TreeNode],
    out: &mut Vec<FolderSelection>,
) {
    for node in nodes {
        // Only folders can be sync targets — skip file nodes entirely.
        if !node.file.folder {
            continue;
        }
        if let Some(Some(policy)) = selected.get(&node.file.uuid) {
            out.push(FolderSelection {
                uuid: node.file.uuid.clone(),
                name: node.file.name.clone(),
                path: build_path(roots, &node.file.uuid),
                policy: policy.clone(),
                completed: false,
            });
        }
        collect_selections(&node.children, selected, roots, out);
    }
}

pub fn format_size(bytes: i64) -> String {
    let bytes = bytes.max(0) as f64;
    if bytes < 1024.0 { format!("{bytes:.0} B") }
    else if bytes < 1024.0 * 1024.0 { format!("{:.1} KB", bytes / 1024.0) }
    else if bytes < 1024.0 * 1024.0 * 1024.0 { format!("{:.1} MB", bytes / (1024.0 * 1024.0)) }
    else { format!("{:.2} GB", bytes / (1024.0 * 1024.0 * 1024.0)) }
}

// ── UI helpers ──

pub fn section_header(ui: &mut egui::Ui, title: &str) {
    ui.label(egui::RichText::new(title).size(16.0).color(TEXT_PRIMARY).strong());
    ui.add_space(4.0);
}

/// Material Design 3 elevated card.
pub fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::default()
        .fill(CARD_BG)
        .rounding(12.0)
        .inner_margin(egui::Margin::same(20.0))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(38, 50, 72)))
        .show(ui, content);
}

/// Material filled button (primary action).
pub fn filled_button(label: &str, enabled: bool) -> egui::Button<'_> {
    let fill = if enabled { ACCENT } else { egui::Color32::from_rgb(40, 55, 80) };
    let text_color = if enabled { TEXT_PRIMARY } else { TEXT_DISABLED };
    egui::Button::new(egui::RichText::new(label).size(14.0).color(text_color))
        .min_size(egui::vec2(100.0, 36.0))
        .rounding(18.0)
        .fill(fill)
}

/// Material tonal button (secondary action).
pub fn tonal_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(label).size(13.0).color(TEXT_PRIMARY))
        .min_size(egui::vec2(85.0, 36.0))
        .rounding(18.0)
        .fill(SURFACE_VARIANT)
}

/// Material text button (minimal emphasis).
pub fn text_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(label).size(13.0).color(TEXT_SECONDARY))
        .min_size(egui::vec2(70.0, 36.0))
        .rounding(18.0)
        .fill(egui::Color32::TRANSPARENT)
}

/// Material divider line.
pub fn divider(ui: &mut egui::Ui) {
    let rect = ui.allocate_space(egui::vec2(ui.available_width(), 1.0)).1;
    ui.painter().rect_filled(rect, 0.0, DIVIDER);
}

// ── Disk space ──

pub fn get_disk_space(path: &str) -> Option<(u64, u64)> {
    let mut p = std::path::PathBuf::from(path);
    while !p.exists() {
        if !p.pop() { break; }
    }
    get_disk_space_raw(&p.to_string_lossy())
}

#[cfg(target_os = "windows")]
fn get_disk_space_raw(path: &str) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::fileapi::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut free_total: u64 = 0;
    let result = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_caller as *mut u64 as *mut _,
            &mut total as *mut u64 as *mut _,
            &mut free_total as *mut u64 as *mut _,
        )
    };
    if result != 0 { Some((total, free_total)) } else { None }
}

#[cfg(not(target_os = "windows"))]
fn get_disk_space_raw(_path: &str) -> Option<(u64, u64)> { None }

pub fn drive_letter(path: &str) -> String {
    let p = std::path::Path::new(path);
    if let Some(std::path::Component::Prefix(prefix)) = p.components().next() {
        return prefix.as_os_str().to_string_lossy().trim_end_matches('\\').to_string();
    }
    "?".to_string()
}

// ── Shortcuts / Sidebar ──

/// Register CloudFiles in the Explorer navigation pane using shell CLSID (like OneDrive).
pub fn create_explorer_sidebar(sync_folder: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(sync_folder)?;
    let guid = "{2CC5E37B-3737-4C89-A1E7-23A99F4C0E00}";
    let clsid = format!(r"HKCU\Software\Classes\CLSID\{guid}");

    reg_set(&clsid, None, "AGB CloudFiles", "REG_SZ")?;
    reg_set_dword(&clsid, "SortOrderIndex", 0x42)?;
    reg_set_dword(&clsid, "System.IsPinnedToNameSpaceTree", 1)?;
    let icon_value = exe_icon_path().unwrap_or_else(|| "%SystemRoot%\\system32\\imageres.dll,-1040".to_string());
    reg_set(&format!(r"{clsid}\DefaultIcon"), None, &icon_value, "REG_SZ")?;
    reg_set(&format!(r"{clsid}\InProcServer32"), None, "%SystemRoot%\\system32\\shell32.dll", "REG_EXPAND_SZ")?;
    reg_set(&format!(r"{clsid}\InProcServer32"), Some("ThreadingModel"), "Apartment", "REG_SZ")?;
    reg_set(
        &format!(r"{clsid}\Instance"),
        Some("CLSID"),
        "{0E5AAE11-A475-4c5b-AB00-C66DE400274E}",
        "REG_SZ",
    )?;
    let ipb = format!(r"{clsid}\Instance\InitPropertyBag");
    reg_set_dword(&ipb, "Attributes", 0x11)?;
    reg_set(&ipb, Some("TargetFolderPath"), sync_folder, "REG_EXPAND_SZ")?;
    let sf = format!(r"{clsid}\ShellFolder");
    reg_set_dword(&sf, "FolderValueFlags", 0x28)?;
    reg_set_dword(&sf, "Attributes", 0xf080004d)?;
    let ns = format!(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{guid}");
    reg_set(&ns, None, "AGB CloudFiles", "REG_SZ")?;
    let hide = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";
    reg_set_dword(hide, guid, 1)?;
    // Apply custom icon to the sync folder itself (visible in File Explorer).
    let _ = create_folder_icon(sync_folder);
    // Notify shell to refresh navigation pane without restarting Explorer.
    notify_shell_namespace_change();
    info!("Explorer sidebar registered for: {sync_folder}");
    Ok(())
}

/// Remove the Explorer navigation pane entry and the folder icon for `sync_folder`.
/// Safe to call even if the entry was never created (registry deletes are idempotent).
pub fn remove_explorer_sidebar(sync_folder: &str) -> anyhow::Result<()> {
    let guid = "{2CC5E37B-3737-4C89-A1E7-23A99F4C0E00}";

    // Delete the entire CLSID subtree (all keys and values under it).
    let clsid = format!(r"HKCU\Software\Classes\CLSID\{guid}");
    let _ = no_window_cmd("reg").args(["delete", &clsid, "/f"]).output();

    // Delete the NameSpace registration.
    let ns = format!(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{guid}",
    );
    let _ = no_window_cmd("reg").args(["delete", &ns, "/f"]).output();

    // Delete the HideDesktopIcons value.
    let hide = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";
    let _ = no_window_cmd("reg").args(["delete", hide, "/v", guid, "/f"]).output();

    // Also clean up the folder icon (desktop.ini + ReadOnly attribute).
    let _ = remove_folder_icon(sync_folder);

    info!("Explorer sidebar entry removed for: {sync_folder}");
    Ok(())
}

/// Give the sync folder a custom AGB icon visible in File Explorer.
///
/// Creates a `desktop.ini` file inside the folder with `[.ShellClassInfo]` →
/// `IconResource` pointing to the app's embedded icon, then marks:
/// - The folder as **ReadOnly** (`+r`) — tells Explorer to honour `desktop.ini`.
/// - `desktop.ini` as **System + Hidden** (`+s +h`) — read by Explorer, hidden from users.
pub fn create_folder_icon(sync_folder: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(sync_folder)?;

    // `IconResource` must be "path,index".  `exe_icon_path()` already returns
    // "path,0" for .exe files; append ",0" only when the path has no index yet.
    let raw = exe_icon_path().unwrap_or_else(|| {
        std::env::current_exe()
            .map(|p| format!("{},0", p.to_string_lossy()))
            .unwrap_or_else(|_| "%SystemRoot%\\system32\\imageres.dll,-1040".to_string())
    });
    let icon_resource = if raw.contains(',') { raw } else { format!("{raw},0") };

    // Write desktop.ini with CRLF line endings (required on Windows).
    let content = format!(
        "[.ShellClassInfo]\r\nIconResource={icon_resource}\r\nInfoTip=AGB CloudFiles Sync Folder\r\n",
    );
    let ini_path = std::path::Path::new(sync_folder).join("desktop.ini");
    std::fs::write(&ini_path, content.as_bytes())?;

    // Mark desktop.ini as System+Hidden so Explorer reads it and users don't see it.
    let ini_str = ini_path.to_string_lossy().to_string();
    let _ = no_window_cmd("attrib").args(["+s", "+h", &ini_str]).output();

    // Mark the folder as ReadOnly — triggers Explorer to honour desktop.ini.
    let _ = no_window_cmd("attrib").args(["+r", sync_folder]).output();

    info!("Folder icon applied: {sync_folder}");
    Ok(())
}

/// Reverse `create_folder_icon`: delete `desktop.ini` and clear the ReadOnly flag.
pub fn remove_folder_icon(sync_folder: &str) -> anyhow::Result<()> {
    let ini_path = std::path::Path::new(sync_folder).join("desktop.ini");
    if ini_path.exists() {
        // Clear System+Hidden so the file can be deleted.
        let ini_str = ini_path.to_string_lossy().to_string();
        let _ = no_window_cmd("attrib").args(["-s", "-h", &ini_str]).output();
        let _ = std::fs::remove_file(&ini_path);
    }
    // Remove the ReadOnly attribute from the folder.
    let _ = no_window_cmd("attrib").args(["-r", sync_folder]).output();
    info!("Folder icon removed: {sync_folder}");
    Ok(())
}

/// Delete the `AGB CloudFiles.lnk` shortcut from the user's Desktop.
/// No-ops silently if the file does not exist.
pub fn remove_desktop_shortcut() -> anyhow::Result<()> {
    let user_dirs = directories::UserDirs::new()
        .ok_or_else(|| anyhow::anyhow!("Could not determine user directories"))?;
    let desktop = user_dirs
        .desktop_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find Desktop directory"))?;
    let path = desktop.join("AGB CloudFiles.lnk");
    if path.exists() {
        std::fs::remove_file(&path)?;
        info!("Desktop shortcut removed: {}", path.display());
    } else {
        info!("Desktop shortcut not found — nothing to remove");
    }
    Ok(())
}

/// Notify the Windows shell that namespace/association changes occurred.
/// This causes Explorer to refresh its navigation pane without restarting.
/// Uses SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, NULL, NULL).
pub fn notify_shell_namespace_change() {
    #[cfg(target_os = "windows")]
    unsafe {
        // SHChangeNotify is not in winapi crate — declare extern manually.
        unsafe extern "system" {
            fn SHChangeNotify(
                wEventId: i32,
                uFlags: u32,
                dwItem1: *mut std::ffi::c_void,
                dwItem2: *mut std::ffi::c_void,
            );
        }
        const SHCNE_ASSOCCHANGED: i32 = 0x08000000;
        const SHCNF_IDLIST: u32 = 0x0000;
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, std::ptr::null_mut(), std::ptr::null_mut());
    }
    info!("Shell namespace change notification sent");
}

/// Kill Explorer.exe and restart it so registry changes (e.g. namespace) take effect.
///
/// Blocks until `Shell_TrayWnd` is visible (Explorer's taskbar fully loaded) or
/// the 10-second timeout expires.  This prevents race conditions where the
/// caller spawns a tray-icon process before the notification area exists —
/// `Shell_NotifyIcon(NIM_ADD)` silently fails if Shell_TrayWnd is not yet running.
pub fn restart_explorer() -> anyhow::Result<()> {
    no_window_cmd("taskkill")
        .args(["/f", "/im", "explorer.exe"])
        .output()?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    no_window_cmd("explorer.exe").spawn()?;
    // Wait for Explorer's notification area to be ready before returning.
    wait_for_shell_tray_wnd(10_000);
    Ok(())
}

/// Poll for `Shell_TrayWnd` (Explorer's system tray host window) with a timeout.
/// Returns as soon as the window is found, or after `timeout_ms` milliseconds.
#[cfg(target_os = "windows")]
fn wait_for_shell_tray_wnd(timeout_ms: u64) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let class_wide: Vec<u16> = OsStr::new("Shell_TrayWnd")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(timeout_ms);

    while std::time::Instant::now() < deadline {
        let hwnd = unsafe {
            winapi::um::winuser::FindWindowW(class_wide.as_ptr(), std::ptr::null())
        };
        if !hwnd.is_null() {
            // Shell_TrayWnd visible — give the notification area a moment to
            // finish initialising before callers register new tray icons.
            std::thread::sleep(std::time::Duration::from_millis(800));
            info!("Explorer taskbar ready (Shell_TrayWnd found)");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // Timeout reached — continue anyway; tray-icon handles WM_TASKBARCREATED.
    info!("restart_explorer: Shell_TrayWnd timeout — continuing");
}

#[cfg(not(target_os = "windows"))]
fn wait_for_shell_tray_wnd(_timeout_ms: u64) {}

pub fn create_desktop_shortcut(sync_folder: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(sync_folder)?;
    let user_dirs = directories::UserDirs::new()
        .ok_or_else(|| anyhow::anyhow!("Could not determine user directories"))?;
    let desktop = user_dirs
        .desktop_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find Desktop directory"))?;
    if !desktop.exists() {
        anyhow::bail!("Desktop not found at: {}", desktop.display());
    }
    let shortcut_path = desktop.join("AGB CloudFiles.lnk");
    let icon_loc = exe_icon_path().unwrap_or_else(|| "shell32.dll,3".to_string());
    let ps = format!(
        "$ws = New-Object -ComObject WScript.Shell; \
         $s = $ws.CreateShortcut('{}'); \
         $s.TargetPath = '{}'; \
         $s.IconLocation = '{}'; \
         $s.Description = 'AGBroadband CloudFiles'; \
         $s.Save()",
        shortcut_path.to_string_lossy().replace('\'', "''"),
        sync_folder.replace('\'', "''"),
        icon_loc.replace('\'', "''"),
    );
    let output = no_window_cmd(&powershell_path())
        .args(["-NoProfile", "-Command", &ps])
        .output()?;
    if !output.status.success() {
        anyhow::bail!("PowerShell: {}", String::from_utf8_lossy(&output.stderr));
    }
    info!("Desktop shortcut: {}", shortcut_path.display());
    Ok(())
}

/// Create a Command that runs without a visible console window on Windows.
#[cfg(target_os = "windows")]
fn no_window_cmd(prog: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let mut cmd = std::process::Command::new(prog);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(target_os = "windows"))]
fn no_window_cmd(prog: &str) -> std::process::Command {
    std::process::Command::new(prog)
}

fn reg_set(key: &str, name: Option<&str>, value: &str, reg_type: &str) -> anyhow::Result<()> {
    let mut args = vec!["add".to_string(), key.to_string()];
    match name {
        Some(n) => { args.push("/v".to_string()); args.push(n.to_string()); }
        None => args.push("/ve".to_string()),
    }
    args.extend(["/t".to_string(), reg_type.to_string(), "/d".to_string(), value.to_string(), "/f".to_string()]);
    let output = no_window_cmd("reg")
        .args(&args)
        .output()?;
    if !output.status.success() {
        anyhow::bail!("reg: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

fn reg_set_dword(key: &str, name: &str, value: u32) -> anyhow::Result<()> {
    let hex_val = format!("0x{value:x}");
    let output = no_window_cmd("reg")
        .args(["add", key, "/v", name, "/t", "REG_DWORD", "/d", &hex_val, "/f"])
        .output()?;
    if !output.status.success() {
        anyhow::bail!("reg: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

/// Return the icon path used for shell registrations (Explorer sidebar, desktop shortcut).
///
/// Priority:
///   1. `icon.ico` next to the exe (present during dev if the file is placed there)
///   2. The exe itself at resource index 0 — installed builds have the AGB icon embedded
///      via the NSIS resource compiler, so `app.exe,0` always shows the branded icon.
fn exe_icon_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    // Prefer a dedicated .ico file when available (dev / portable layout)
    let icon = exe.parent()?.join("icon.ico");
    if icon.exists() {
        return Some(icon.to_string_lossy().into_owned());
    }
    // Fallback: exe,0 — resource index 0 is the first (app) icon embedded in the binary.
    // In a release / NSIS-installed build this is the AGB branded icon.
    Some(format!("{},0", exe.to_string_lossy()))
}

fn powershell_path() -> String {
    if let Ok(sysroot) = std::env::var("SystemRoot") {
        let full = format!("{}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", sysroot);
        if std::path::Path::new(&full).exists() { return full; }
    }
    "powershell.exe".to_string()
}

/// Register the app's Windows AppUserModelID (AUMID) so toast notifications
/// show the AGB icon and "AGB Cloud Client" title instead of the PowerShell icon.
///
/// Writes to `HKCU\Software\Classes\AppUserModelId\AGBroadband.CloudClient`.
/// Safe to call on every startup — registry writes are idempotent.
pub fn register_notification_app_id() {
    #[cfg(target_os = "windows")]
    {
        const AUMID: &str = "AGBroadband.CloudClient";
        let key = format!(r"HKCU\Software\Classes\AppUserModelId\{AUMID}");
        let _ = reg_set(&key, Some("DisplayName"), "AGB Cloud Client", "REG_SZ");
        let icon = exe_icon_path().unwrap_or_else(|| {
            std::env::current_exe()
                .map(|p| format!("{},0", p.to_string_lossy()))
                .unwrap_or_default()
        });
        if !icon.is_empty() {
            let _ = reg_set(&key, Some("DisplayIcon"), &icon, "REG_SZ");
        }
        info!("Notification AUMID registered: {AUMID}");
    }
}

/// The AppUserModelID used for Windows toast notifications.
/// Must match the AUMID registered by [`register_notification_app_id`].
pub const NOTIFICATION_APP_ID: &str = "AGBroadband.CloudClient";

/// Enable or disable auto-start with Windows via the Run registry key.
pub fn set_auto_start(enable: bool) -> anyhow::Result<()> {
    let run_key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let app_name = "AGBCloudClient";

    if enable {
        // Get the path to the current executable
        let exe_path = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("Cannot determine exe path: {e}"))?;
        let exe_str = exe_path.to_string_lossy().to_string();
        reg_set(run_key, Some(app_name), &exe_str, "REG_SZ")?;
        info!("Auto-start enabled: {exe_str}");
    } else {
        // Delete the registry value (ignore error if it doesn't exist)
        let output = no_window_cmd("reg")
            .args(["delete", run_key, "/v", app_name, "/f"])
            .output()?;
        if output.status.success() {
            info!("Auto-start disabled");
        }
    }
    Ok(())
}
