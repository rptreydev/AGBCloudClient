use std::sync::{Arc, Mutex};

use egui::{Context, RichText};
use tracing::error;

use crate::update::{download_and_install, DownloadProgress, DownloadState, UpdateInfo};
use crate::ui::common::*;

// ── App ───────────────────────────────────────────────────────────────────────

struct UpdateApp {
    info: UpdateInfo,
    jwt: String,
    progress: Arc<Mutex<DownloadProgress>>,
    rt_handle: tokio::runtime::Handle,
    download_started: bool,
}

impl eframe::App for UpdateApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(SURFACE))
            .show(ctx, |ui| self.render(ui));

        // Keep repainting so the progress bar stays live.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

impl UpdateApp {
    fn render(&mut self, ui: &mut egui::Ui) {
        let p = self.progress.lock().unwrap().clone();

        ui.add_space(28.0);

        // ── Header ──────────────────────────────────────────────────────────
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("⬆").size(44.0).color(ACCENT));
            ui.add_space(6.0);
            ui.label(RichText::new("Update Required").size(20.0).color(TEXT_PRIMARY).strong());
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("Version {} is available", self.info.version))
                    .size(13.0)
                    .color(TEXT_SECONDARY),
            );
            ui.label(
                RichText::new(format!("You are running v{}", env!("CARGO_PKG_VERSION")))
                    .size(11.0)
                    .color(TEXT_SECONDARY),
            );
        });

        ui.add_space(20.0);

        // ── Warning banner ───────────────────────────────────────────────────
        egui::Frame::default()
            .fill(ERROR_COLOR.linear_multiply(0.12))
            .stroke(egui::Stroke::new(1.0, ERROR_COLOR))
            .rounding(8.0)
            .inner_margin(egui::Margin::symmetric(16.0, 10.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "⚠  Your current version will stop working until the update \
                         is installed.",
                    )
                    .size(12.0)
                    .color(ERROR_COLOR),
                );
            });

        ui.add_space(22.0);

        // ── Download / progress area ─────────────────────────────────────────
        match &p.state {
            DownloadState::Idle => {
                ui.vertical_centered(|ui| {
                    let btn = egui::Button::new(
                        RichText::new("  Install Now  ")
                            .size(15.0)
                            .color(egui::Color32::WHITE),
                    )
                    .fill(ACCENT)
                    .min_size(egui::vec2(180.0, 42.0));

                    if ui.add(btn).clicked() && !self.download_started {
                        self.download_started = true;
                        let info = self.info.clone();
                        let jwt = self.jwt.clone();
                        let progress = self.progress.clone();
                        let handle = self.rt_handle.clone();
                        std::thread::spawn(move || {
                            handle.block_on(async move {
                                if let Err(e) =
                                    download_and_install(&info, &jwt, progress.clone()).await
                                {
                                    error!("Update download failed: {e}");
                                    let mut p = progress.lock().unwrap();
                                    p.state = DownloadState::Failed(e.to_string());
                                }
                            });
                        });
                    }
                });
            }

            DownloadState::Downloading => {
                let frac = if p.total_bytes > 0 {
                    p.downloaded_bytes as f32 / p.total_bytes as f32
                } else {
                    0.0
                };
                let avail = ui.available_width() - 40.0;
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(avail)
                        .animate(true),
                );
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    let dl = p.downloaded_bytes as f64 / 1_048_576.0;
                    let tot = p.total_bytes as f64 / 1_048_576.0;
                    ui.label(
                        RichText::new(format!("Downloading… {dl:.1} MB / {tot:.1} MB"))
                            .size(12.0)
                            .color(TEXT_SECONDARY),
                    );
                });
            }

            DownloadState::Installing => {
                ui.vertical_centered(|ui| {
                    ui.spinner();
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Installing update…")
                            .size(13.0)
                            .color(TEXT_SECONDARY),
                    );
                    ui.label(
                        RichText::new("The app will restart automatically.")
                            .size(11.0)
                            .color(TEXT_SECONDARY),
                    );
                });
            }

            DownloadState::Failed(err) => {
                let msg = err.clone();
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(format!("Download failed: {msg}"))
                            .size(12.0)
                            .color(ERROR_COLOR),
                    );
                    ui.add_space(8.0);
                    if ui.button("Retry").clicked() {
                        let mut prog = self.progress.lock().unwrap();
                        *prog = DownloadProgress::default();
                        self.download_started = false;
                    }
                });
            }
        }

        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("The app will restart automatically after installation.")
                    .size(11.0)
                    .color(TEXT_SECONDARY),
            );
            ui.add_space(10.0);
            // Allow dismissing the window — the flag file remains, so the update
            // will be shown again on next launch.
            if matches!(&p.state, DownloadState::Idle | DownloadState::Failed(_)) {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Remind me later")
                                .size(11.0)
                                .color(TEXT_SECONDARY),
                        )
                        .frame(false),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    std::process::exit(0);
                }
            }
        });
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Show the update window. The user can dismiss it ("Remind me later") —
/// the flag file stays on disk so the update is offered again on next launch.
/// The window exits via `std::process::exit(0)` after the installer is launched.
pub fn show_update_window(info: UpdateInfo, jwt: String, rt: &tokio::runtime::Runtime) {
    let progress = Arc::new(Mutex::new(DownloadProgress::default()));
    let app = UpdateApp {
        info,
        jwt,
        progress,
        rt_handle: rt.handle().clone(),
        download_started: false,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AGB Cloud Client — Update Required")
            .with_inner_size([420.0, 360.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_minimize_button(false)
            .with_always_on_top()
            .with_icon(crate::ui::icon::app_icon_data()),
        centered: true,
        ..Default::default()
    };

    eframe::run_native("agb-update", options, Box::new(|_cc| Ok(Box::new(app)))).ok();
}
