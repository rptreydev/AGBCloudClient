//! Standalone login window — thin wrapper around [`LoginWidget`].
//!
//! Adds the branding header ("AGBroadband / Cloud Files") and window chrome.
//! All form state, 2-FA handling, and async logic live in [`LoginWidget`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use eframe::egui;
use tokio::runtime::Runtime;

use crate::auth::AuthState;
use crate::ui::common::*;
use crate::ui::login_widget::{LoginOutcome, LoginWidget};

// ── App ───────────────────────────────────────────────────────────────────────

struct LoginApp {
    widget: LoginWidget,
    /// Set to `true` inside eframe before GPU teardown so the caller can read
    /// the auth result even if the GPU destructor crashes the process afterward.
    success_flag: Arc<AtomicBool>,
    /// If `Some`, spawn the current exe with these args on success — inside
    /// eframe before GPU teardown, to avoid losing the spawn to GPU crashes.
    ///
    /// - `Some(vec![])` → spawn tray (no args); used in `--login` mode.
    /// - `Some(vec!["--manage-folders"])` → relaunch manage-folders after re-auth.
    /// - `Some(vec!["--settings"])` → relaunch settings after re-auth.
    /// - `None` → don't spawn (main process regular login, caller continues).
    spawn_args: Option<Vec<String>>,
}

impl LoginApp {
    fn new(
        auth: AuthState,
        handle: tokio::runtime::Handle,
        success_flag: Arc<AtomicBool>,
        spawn_args: Option<Vec<String>>,
    ) -> Self {
        Self { widget: LoginWidget::new(auth, handle), success_flag, spawn_args }
    }
}

impl eframe::App for LoginApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        configure_visuals(ctx);

        // Close the window as soon as authentication succeeds.
        // Store success BEFORE sending Close — written before any GPU teardown.
        if let Some(LoginOutcome::Success) = self.widget.poll(ctx) {
            self.success_flag.store(true, Ordering::SeqCst);

            // Spawn the next process INSIDE eframe (before GPU teardown).
            // Code after eframe::run_native may never execute on Windows.
            if let Some(ref args) = self.spawn_args {
                match std::env::current_exe() {
                    Ok(exe) => match std::process::Command::new(&exe).args(args).spawn() {
                        Ok(_) => tracing::info!("Login succeeded — spawned process with args: {args:?}"),
                        Err(e) => tracing::error!("Failed to spawn process after login: {e}"),
                    },
                    Err(e) => tracing::error!("Cannot determine exe path: {e}"),
                }
            }

            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(DARK_BG))
            .show(ctx, |ui| {
                ui.add_space(40.0);

                // ── Branding header ───────────────────────────────────────────
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("AGBroadband")
                            .size(32.0).color(ACCENT).strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Cloud Files")
                            .size(15.0).color(TEXT_SECONDARY),
                    );
                });

                ui.add_space(28.0);

                // ── Login card ────────────────────────────────────────────────
                self.widget.render(ui);

                // ── Footer ────────────────────────────────────────────────────
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Secure connection · api.agbroadband.net")
                            .size(11.0)
                            .color(egui::Color32::from_rgb(80, 100, 130)),
                    );
                });
            });
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Open the standalone login window and block until the user authenticates
/// or closes the window.
///
/// Returns `true` if login succeeded, `false` if the user cancelled.
///
/// `spawn_args`:
/// - `None` — don't spawn anything; caller handles what to do next (main process).
/// - `Some(vec![])` — spawn `exe` with no args on success (fresh tray, `--login` mode).
/// - `Some(vec!["--manage-folders"])` — relaunch manage-folders after re-auth.
/// - `Some(vec!["--settings"])` — relaunch settings after re-auth.
///
/// When `spawn_args` is `Some`, the spawn happens INSIDE eframe before GPU teardown.
/// In that case call `std::process::exit(0)` unconditionally after this function
/// regardless of the return value.
pub fn show_login_window(auth: &AuthState, rt: &Runtime, spawn_args: Option<Vec<String>>) -> bool {
    let handle = rt.handle().clone();
    let auth_clone = auth.clone();

    // Shared flag: written inside eframe before GPU teardown.
    let success_flag = Arc::new(AtomicBool::new(false));
    let flag_for_app = success_flag.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([440.0, 520.0])
            .with_min_inner_size([400.0, 480.0])
            .with_title("AGB Cloud Client — Sign In")
            .with_icon(Arc::new(crate::ui::icon::app_icon_data())),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "AGB Cloud Client — Sign In",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(LoginApp::new(auth_clone, handle, flag_for_app, spawn_args)))
        }),
    );

    // Read the flag AFTER eframe::run_native returns.
    // Even if the GPU destructor is about to crash, this read is safe because
    // it happens in caller stack frames above the GPU teardown.
    success_flag.load(Ordering::SeqCst)
}
