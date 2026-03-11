//! Reusable folder-tree selection widget shared by wizard, settings, and file_browser.
//!
//! Encapsulates all tree state and async fetch/poll logic so callers only need to:
//!   1. Call `widget.poll(ctx)` each frame from `update()`.
//!   2. Render their own search bar row (binding `widget.search_query`).
//!   3. Call `widget.render(ui)` for the tree body.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use eframe::egui;
use tracing::{error, info};

use crate::auth::AuthState;
use crate::models::{FolderSelection, SyncPolicy};
use crate::sync::remote::RemoteClient;
use crate::ui::common::*;

/// Self-contained folder-tree selection widget.
pub struct FolderTreeWidget {
    /// Bind this string to a `TextEdit` in the search bar row.
    pub search_query: String,
    /// `true` while an async fetch is in flight.
    pub is_loading: bool,
    /// Non-empty when the last fetch failed.
    pub error: String,

    roots: Vec<TreeNode>,
    selected_policies: HashMap<String, Option<SyncPolicy>>,
    explicit_selections: HashSet<String>,
    /// Copy of the original saved selections passed to `with_initial_selections`.
    /// Used as a fallback in `build_selections()` for items whose tree nodes
    /// haven't been loaded yet (user hasn't expanded that branch).
    initial_selections: Vec<FolderSelection>,
    result_rx: Option<mpsc::Receiver<FetchResult>>,
    auth: AuthState,
    handle: tokio::runtime::Handle,
    /// Queue of folder UUIDs to auto-expand/fetch for pre-selected sub-folders.
    /// Populated after loading roots/children when ancestor markers are found.
    pending_fetches: VecDeque<String>,
    /// When `false`, the auto-expand queue is disabled.
    /// Use `with_auto_expand(false)` in browse contexts (Manage Folders) where
    /// the user wants to navigate freely without pre-selected folders expanding
    /// automatically and shifting the view.
    auto_expand: bool,
}

impl FolderTreeWidget {
    /// Create an empty widget. Call [`fetch_roots`] to start loading.
    pub fn new(auth: AuthState, handle: tokio::runtime::Handle) -> Self {
        Self {
            search_query: String::new(),
            is_loading: false,
            error: String::new(),
            roots: Vec::new(),
            selected_policies: HashMap::new(),
            explicit_selections: HashSet::new(),
            initial_selections: Vec::new(),
            result_rx: None,
            auth,
            handle,
            pending_fetches: VecDeque::new(),
            auto_expand: true,
        }
    }

    /// Disable auto-expansion of pre-selected folders (builder-style).
    ///
    /// Use in browse contexts (e.g. Manage Folders) where the user wants to
    /// navigate the tree freely.  Pre-selected folders still show their
    /// checkmarks/policy badges but won't expand automatically.
    pub fn with_auto_expand(mut self, enabled: bool) -> Self {
        self.auto_expand = enabled;
        self
    }

    /// Pre-populate selections from a saved config (builder-style, call before `fetch_roots`).
    pub fn with_initial_selections(mut self, selections: &[FolderSelection]) -> Self {
        for f in selections {
            self.selected_policies.insert(f.uuid.clone(), Some(f.policy.clone()));
            self.explicit_selections.insert(f.uuid.clone());
        }
        self.initial_selections = selections.to_vec();
        self
    }

    // ── Async fetch ──────────────────────────────────────────────────────────

    pub fn fetch_roots(&mut self) {
        self.is_loading = true;
        self.error.clear();
        self.roots.clear();
        let auth = self.auth.clone();
        let (tx, rx) = mpsc::channel();
        self.result_rx = Some(rx);
        self.handle.spawn(async move {
            let remote = RemoteClient::new(auth);
            match remote.get_roots().await {
                Ok(roots) => { let _ = tx.send(FetchResult::Roots(roots)); }
                Err(e)    => { let _ = tx.send(FetchResult::Error(e.to_string())); }
            }
        });
    }

    fn fetch_children(&mut self, uuid: &str) {
        let uuid = uuid.to_string();
        let auth = self.auth.clone();
        let (tx, rx) = mpsc::channel();
        self.result_rx = Some(rx);
        self.handle.spawn(async move {
            let remote = RemoteClient::new(auth);
            match remote.get_children(&uuid).await {
                Ok(children) => { let _ = tx.send(FetchResult::Children(uuid, children)); }
                Err(e)                  => { let _ = tx.send(FetchResult::Error(e.to_string())); }
            }
        });
    }

    /// Call every frame from the parent's `update()` to receive async results.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let result = match self.result_rx.as_ref() {
            Some(rx) => match rx.try_recv() {
                Ok(r) => Some(r),
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Always clear stale receiver — prevents the Disconnected branch
                    // from blocking the auto-expand queue on subsequent polls.
                    self.result_rx = None;
                    if self.is_loading {
                        self.is_loading = false;
                        self.error = "Connection lost while fetching folders".to_string();
                        self.pending_fetches.clear();
                    }
                    None
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if self.is_loading { ctx.request_repaint(); }
                    None
                }
            },
            None => None,
        };

        if let Some(result) = result {
            self.is_loading = false;
            match result {
                FetchResult::Roots(roots) => {
                    self.roots = roots.into_iter().map(TreeNode::from_cloud_file).collect();
                    // After loading roots, rebuild ancestor markers so that any root-level
                    // ancestor of a pre-loaded explicit selection shows as checked.
                    self.rebuild_ancestor_markers();
                    info!("FolderTree: loaded {} root folders", self.roots.len());
                    // Auto-expand roots that have ancestor markers so pre-selected
                    // sub-folders become visible without manual clicks.
                    if self.auto_expand { self.enqueue_ancestor_marked_nodes(); }
                }
                FetchResult::Children(parent_uuid, children) => {
                    insert_children(&mut self.roots, &parent_uuid, children);
                    // After loading children, rebuild ancestor markers: if any newly-loaded
                    // child is an explicit selection, its ancestors (including parent_uuid)
                    // will now be findable and get ancestor markers.
                    self.rebuild_ancestor_markers();
                    // Only propagate policy to newly-loaded children if the parent was
                    // explicitly selected (has a real policy + is in explicit_selections).
                    // Ancestor markers (None) must NOT trigger propagation — that was the
                    // sibling-selection bug.
                    if self.explicit_selections.contains(&parent_uuid) {
                        if let Some(Some(policy)) = self.selected_policies.get(&parent_uuid) {
                            let policy = policy.clone();
                            for uuid in collect_descendant_uuids(&self.roots, &parent_uuid) {
                                self.selected_policies.entry(uuid).or_insert(Some(policy.clone()));
                            }
                        }
                    }
                    // Continue auto-expanding deeper ancestor-marked nodes now that new
                    // children are visible.
                    if self.auto_expand { self.enqueue_ancestor_marked_nodes(); }
                }
                FetchResult::Error(msg) => {
                    error!("FolderTree fetch error: {msg}");
                    self.error = msg;
                    // Stop auto-expand on error; user can Retry manually.
                    self.pending_fetches.clear();
                }
            }
            ctx.request_repaint();
        }

        // Drain auto-expand queue: kick off the next fetch when the previous one finished.
        // Set is_loading = true so the Empty branch keeps requesting repaints while the
        // fetch is in progress — without this, children appear only after mouse interaction.
        if self.result_rx.is_none() && !self.is_loading && !self.pending_fetches.is_empty() {
            if let Some(uuid) = self.pending_fetches.pop_front() {
                info!("FolderTree: auto-expanding folder {uuid}");
                self.is_loading = true;
                self.fetch_children(&uuid);
                ctx.request_repaint();
            }
        }
    }

    /// Scan the loaded tree and add `None` ancestor markers for every ancestor
    /// of an explicit selection that is currently visible in the tree.
    /// Called after each tree load so reopening settings shows correct checkmarks.
    fn rebuild_ancestor_markers(&mut self) {
        // Tree-based: mark ancestors of explicit selections already visible in the tree.
        let uuids: Vec<String> = self.explicit_selections.iter().cloned().collect();
        for uuid in &uuids {
            for ancestor in find_ancestor_uuids(&self.roots, uuid) {
                if !self.explicit_selections.contains(&ancestor) {
                    self.selected_policies.entry(ancestor).or_insert(None);
                }
            }
        }

        // Path-based (multi-level): walk every path component and mark ALL intermediate
        // ancestors visible at any depth in the loaded tree.
        //
        // The old code only marked the root-level ancestor ("ROOT" from "ROOT / A / B").
        // After loading ROOT's children (revealing "A"), "A" never got a None marker,
        // so enqueue_ancestor_marked_nodes() never queued "A" for fetch and "B" was
        // never visible. This version walks deeper into each loaded level, allowing the
        // auto-expand cascade to proceed one level at a time.
        let initial_selections = self.initial_selections.clone(); // owned copy avoids borrow conflict
        for sel in &initial_selections {
            if !self.explicit_selections.contains(&sel.uuid) { continue; }
            let components: Vec<&str> = sel.path.split(" / ").map(str::trim).collect();
            let mut to_mark: Vec<String> = Vec::new();
            collect_path_ancestor_uuids(
                &self.roots,
                &components,
                &sel.uuid,
                &self.explicit_selections,
                &mut to_mark,
            );
            for uuid in to_mark {
                self.selected_policies.entry(uuid).or_insert(None);
            }
        }
    }

    // ── Selection logic ──────────────────────────────────────────────────────

    fn process_actions(&mut self, actions: Vec<TreeAction>) {
        for action in actions {
            match action {
                TreeAction::Select(uuid) => {
                    self.explicit_selections.insert(uuid.clone());
                    self.selected_policies.insert(uuid.clone(), Some(SyncPolicy::KeepSynced { interval_secs: 30 }));
                    for d in collect_descendant_uuids(&self.roots, &uuid) {
                        self.explicit_selections.insert(d.clone());
                        self.selected_policies.entry(d).or_insert(Some(SyncPolicy::KeepSynced { interval_secs: 30 }));
                    }
                    // Mark ancestors with None (marker only — prevents sibling propagation on child-load)
                    for a in find_ancestor_uuids(&self.roots, &uuid) {
                        self.selected_policies.entry(a).or_insert(None);
                    }
                }
                TreeAction::Deselect(uuid) => {
                    self.selected_policies.remove(&uuid);
                    self.explicit_selections.remove(&uuid);
                    for d in collect_descendant_uuids(&self.roots, &uuid) {
                        self.selected_policies.remove(&d);
                        self.explicit_selections.remove(&d);
                    }
                    // Clean up ancestor markers bottom-up when no selected descendants remain
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
                    // Propagate to already-selected descendants only
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

    // ── Rendering ────────────────────────────────────────────────────────────

    /// Render the tree body (spinner / error / scrollable tree).
    /// Returns the `CloudFile` the user clicked to preview (if any).
    /// Actions (select/deselect/fetch) are processed internally.
    pub fn render(&mut self, ui: &mut egui::Ui) -> Option<crate::models::CloudFile> {
        if self.is_loading && self.roots.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.spinner();
                ui.label(egui::RichText::new("Loading folders...").size(14.0).color(TEXT_SECONDARY));
            });
            return None;
        }

        if !self.error.is_empty() {
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_premultiplied(80, 20, 20, 200))
                .stroke(egui::Stroke::new(1.0, ERROR_COLOR))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(12.0))
                .show(ui, |ui: &mut egui::Ui| {
                    ui.horizontal(|ui: &mut egui::Ui| {
                        ui.label(egui::RichText::new("⚠").size(16.0).color(ERROR_COLOR));
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(&self.error).size(13.0).color(ERROR_COLOR));
                    });
                    ui.add_space(6.0);
                    if ui.add(
                        egui::Button::new(egui::RichText::new("Retry").size(12.0).color(TEXT_PRIMARY))
                            .fill(ERROR_COLOR.gamma_multiply(0.4))
                            .min_size(egui::vec2(60.0, 26.0))
                    ).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.fetch_roots();
                    }
                });
            return None;
        }

        let mut actions = Vec::new();
        let mut preview: Option<crate::models::CloudFile> = None;
        let search = self.search_query.clone();
        egui::ScrollArea::vertical()
            .id_salt("agb_folder_tree_scroll")     // stable ID so scroll offset persists across repaints
            .drag_to_scroll(false)                 // prevent clicks on checkboxes from being misread as scroll drags
            .show(ui, |ui| {
                render_tree(ui, &mut self.roots, &self.selected_policies, 0, &mut actions, &search, &mut preview);
            });
        self.process_actions(actions);
        preview
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// Total entries in the selection map (including ancestor markers).
    /// Use this for the "N item(s) selected" counter shown in the UI.
    pub fn map_len(&self) -> usize {
        self.selected_policies.len()
    }

    /// `true` if at least one folder has a real policy (not just an ancestor marker).
    pub fn has_real_selection(&self) -> bool {
        self.selected_policies.values().any(|p| p.is_some())
    }

    /// Count of explicitly-selected items that still have no sync policy assigned.
    ///
    /// Ancestor markers (`None` in `selected_policies` but NOT in `explicit_selections`)
    /// are intentionally excluded — they don't need a policy themselves; they just
    /// indicate that a descendant is selected.
    ///
    /// By construction `TreeAction::Select` always assigns `KeepSynced`, so this
    /// should normally return 0.  It would only be non-zero in an edge case where
    /// an explicit selection somehow ended up without a concrete policy.
    pub fn unset_count(&self) -> usize {
        self.explicit_selections
            .iter()
            .filter(|uuid| !matches!(
                self.selected_policies.get(*uuid),
                Some(Some(_))
            ))
            .count()
    }

    /// Estimated total size of all selected items.
    pub fn selected_size(&self) -> u64 {
        calc_selected_size(&self.roots, &self.selected_policies)
    }

    /// Build the final list of `FolderSelection` (only items with a real policy).
    ///
    /// Also includes items from `initial_selections` that are still in
    /// `explicit_selections` but whose tree nodes haven't been loaded yet
    /// (user hasn't expanded that branch). This prevents losing selections
    /// when the user opens Settings, makes no changes, and clicks Save.
    pub fn build_selections(&self) -> Vec<FolderSelection> {
        let mut out = Vec::new();
        collect_selections(&self.roots, &self.selected_policies, &self.roots, &mut out);

        // Fallback: add initial selections not yet visible in the loaded tree.
        // Collect owned Strings to avoid holding a borrow on `out`.
        let found: HashSet<String> = out.iter().map(|s| s.uuid.clone()).collect();
        for sel in &self.initial_selections {
            if found.contains(&sel.uuid) { continue; }
            if !self.explicit_selections.contains(&sel.uuid) { continue; }
            // Use current policy in case it was changed interactively.
            if let Some(Some(policy)) = self.selected_policies.get(&sel.uuid) {
                out.push(FolderSelection {
                    uuid: sel.uuid.clone(),
                    name: sel.name.clone(),
                    path: sel.path.clone(),
                    policy: policy.clone(),
                    completed: sel.completed,
                });
            }
        }
        out
    }

    // ── Auto-expand helpers ───────────────────────────────────────────────────

    /// Scan all currently-loaded tree nodes and, for any folder that has an
    /// ancestor marker (`None` policy), set it as expanded and queue its UUID
    /// for child fetching if the children haven't been loaded yet.
    ///
    /// This makes pre-selected sub-folders visible when opening the Folder
    /// Manager without the user needing to manually click expand arrows.
    fn enqueue_ancestor_marked_nodes(&mut self) {
        if self.explicit_selections.is_empty() {
            return; // Nothing to expand — skip the scan
        }

        // Two-pass to satisfy borrow checker:
        // 1. Read-only pass — collect UUIDs that need expanding / fetching.
        let mut to_expand: Vec<String> = Vec::new();
        let mut to_fetch: Vec<String> = Vec::new();
        find_ancestor_marked_for_expand(
            &self.roots,
            &self.selected_policies,
            &mut to_expand,
            &mut to_fetch,
        );

        // 2. Write pass — set expanded flag in the tree.
        for uuid in &to_expand {
            set_node_expanded(&mut self.roots, uuid);
        }

        // 3. Queue unique UUIDs for child fetching.
        for uuid in to_fetch {
            if !self.pending_fetches.contains(&uuid) {
                self.pending_fetches.push_back(uuid);
            }
        }
    }
}

// ── Private free functions for auto-expand ───────────────────────────────────

/// Recursively scan `nodes` and collect folders with ancestor markers
/// (`Some(None)` in `policies`) that should be auto-expanded / fetched.
///
/// * `to_expand` — UUIDs whose `expanded` flag should be set to `true`
/// * `to_fetch`  — UUIDs whose children should be fetched (not yet loaded)
fn find_ancestor_marked_for_expand(
    nodes: &[TreeNode],
    policies: &HashMap<String, Option<SyncPolicy>>,
    to_expand: &mut Vec<String>,
    to_fetch: &mut Vec<String>,
) {
    for node in nodes {
        if !node.file.folder { continue; }

        let is_ancestor_marker = matches!(policies.get(&node.file.uuid), Some(None));

        if is_ancestor_marker {
            to_expand.push(node.file.uuid.clone());
            if !node.children_loaded {
                // Children not fetched yet — queue this UUID for fetch.
                to_fetch.push(node.file.uuid.clone());
            } else {
                // Children already loaded — recurse to find deeper markers.
                find_ancestor_marked_for_expand(&node.children, policies, to_expand, to_fetch);
            }
        } else if node.expanded && node.children_loaded {
            // Node is already expanded but not a marker itself — scan its
            // children for deeper ancestor-marked nodes.
            find_ancestor_marked_for_expand(&node.children, policies, to_expand, to_fetch);
        }
    }
}

/// Walk the loaded tree following path components (`sel.path.split(" / ")`) and
/// collect the UUIDs of every intermediate ancestor that is currently visible.
///
/// Stops at the target `target_uuid` (doesn't add it — it's the selected item
/// itself) and also stops if a level's children haven't been loaded yet (we
/// can't go deeper than what the API has returned so far).  Called from
/// `rebuild_ancestor_markers()` after each tree load so that the
/// auto-expand queue can proceed level-by-level.
fn collect_path_ancestor_uuids(
    nodes: &[TreeNode],
    path: &[&str],
    target_uuid: &str,
    explicit_selections: &HashSet<String>,
    to_mark: &mut Vec<String>,
) {
    let (&head, tail) = match path.split_first() {
        Some(parts) => parts,
        None => return,
    };
    let Some(node) = nodes.iter().find(|n| n.file.name.trim() == head) else { return };

    // If this IS the selected node, stop — don't add it as an ancestor marker.
    if node.file.uuid == target_uuid { return; }

    // This node is an intermediate ancestor — mark it (unless explicitly selected).
    if !explicit_selections.contains(&node.file.uuid) {
        to_mark.push(node.file.uuid.clone());
    }

    // Recurse into children only if they have already been fetched.
    if node.children_loaded && !tail.is_empty() {
        collect_path_ancestor_uuids(&node.children, tail, target_uuid, explicit_selections, to_mark);
    }
}

/// Walk `nodes` recursively and set `expanded = true` for the node with
/// the given `uuid`.
fn set_node_expanded(nodes: &mut [TreeNode], uuid: &str) {
    for node in nodes.iter_mut() {
        if node.file.uuid == uuid {
            node.expanded = true;
            return;
        }
        // Only recurse into already-loaded subtrees to avoid searching
        // uninitialised child vecs.
        if node.children_loaded {
            set_node_expanded(&mut node.children, uuid);
        }
    }
}
