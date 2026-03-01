use std::sync::Arc;
use eframe::egui;
use tokio::runtime::Runtime;
use tracing::{error, info};

use crate::auth::AuthState;
use crate::models::LoginCredentials;
use crate::ui::common::*;

/// Result from async login attempt
enum LoginResult {
    Success,
    Needs2FA(String),
    Error(String),
}

struct LoginApp {
    username: String,
    password: String,
    verification_code: String,
    error_message: String,
    info_message: String,
    is_loading: bool,
    needs_2fa: bool,
    login_complete: bool,
    auth: AuthState,
    handle: tokio::runtime::Handle,
    result_rx: Option<std::sync::mpsc::Receiver<LoginResult>>,
}

impl LoginApp {
    fn new(auth: AuthState, handle: tokio::runtime::Handle) -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            verification_code: String::new(),
            error_message: String::new(),
            info_message: String::new(),
            is_loading: false,
            needs_2fa: false,
            login_complete: false,
            auth,
            handle,
            result_rx: None,
        }
    }

    fn attempt_login(&mut self) {
        self.is_loading = true;
        self.error_message.clear();
        self.info_message.clear();

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
        self.result_rx = Some(rx);

        self.handle.spawn(async move {
            match auth.login(credentials).await {
                Ok(()) => {
                    let _ = tx.send(LoginResult::Success);
                }
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

    fn poll_result(&mut self) {
        let result = self
            .result_rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok());

        if let Some(result) = result {
            self.is_loading = false;
            self.result_rx = None;
            match result {
                LoginResult::Success => {
                    info!("Login successful");
                    self.login_complete = true;
                }
                LoginResult::Needs2FA(msg) => {
                    self.needs_2fa = true;
                    self.info_message = msg;
                }
                LoginResult::Error(msg) => {
                    error!("Login error: {msg}");
                    self.error_message = msg;
                }
            }
        }
    }
}

impl eframe::App for LoginApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        configure_visuals(ctx);
        self.poll_result();

        if self.login_complete {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        if self.is_loading {
            ctx.request_repaint();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(DARK_BG))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);

                    // Branding
                    ui.label(
                        egui::RichText::new("AGBroadband")
                            .size(32.0)
                            .color(ACCENT)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Cloud Files Setup")
                            .size(15.0)
                            .color(TEXT_SECONDARY),
                    );
                    ui.add_space(35.0);

                    // Login card
                    egui::Frame::default()
                        .fill(CARD_BG)
                        .rounding(10.0)
                        .inner_margin(egui::Margin::same(28.0))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(45, 70, 105)))
                        .show(ui, |ui| {
                            ui.set_width(320.0);

                            ui.label(
                                egui::RichText::new("Sign In")
                                    .size(22.0)
                                    .color(TEXT_PRIMARY)
                                    .strong(),
                            );
                            ui.add_space(20.0);

                            // Username field
                            ui.label(
                                egui::RichText::new("Username")
                                    .size(13.0)
                                    .color(TEXT_SECONDARY),
                            );
                            ui.add_space(4.0);
                            let _username_re = ui.add(
                                egui::TextEdit::singleline(&mut self.username)
                                    .desired_width(300.0)
                                    .hint_text("Enter your username")
                                    .margin(egui::Margin::symmetric(8.0, 6.0)),
                            );
                            ui.add_space(14.0);

                            // Password field
                            ui.label(
                                egui::RichText::new("Password")
                                    .size(13.0)
                                    .color(TEXT_SECONDARY),
                            );
                            ui.add_space(4.0);
                            let _password_re = ui.add(
                                egui::TextEdit::singleline(&mut self.password)
                                    .desired_width(300.0)
                                    .password(true)
                                    .hint_text("Enter your password")
                                    .margin(egui::Margin::symmetric(8.0, 6.0)),
                            );
                            ui.add_space(14.0);

                            // 2FA code field (appears after initial login triggers it)
                            if self.needs_2fa {
                                ui.label(
                                    egui::RichText::new("Verification Code")
                                        .size(13.0)
                                        .color(ACCENT),
                                );
                                ui.add_space(4.0);
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.verification_code)
                                        .desired_width(300.0)
                                        .hint_text("Enter code from email")
                                        .margin(egui::Margin::symmetric(8.0, 6.0)),
                                );
                                ui.add_space(14.0);
                            }

                            // Info message (e.g., "code sent to email")
                            if !self.info_message.is_empty() {
                                ui.label(
                                    egui::RichText::new(&self.info_message)
                                        .size(12.0)
                                        .color(ACCENT),
                                );
                                ui.add_space(6.0);
                            }

                            // Error message
                            if !self.error_message.is_empty() {
                                ui.label(
                                    egui::RichText::new(&self.error_message)
                                        .size(12.0)
                                        .color(ERROR_COLOR),
                                );
                                ui.add_space(6.0);
                            }

                            // Login button
                            ui.add_space(6.0);
                            let button_text = if self.is_loading {
                                "Signing in..."
                            } else if self.needs_2fa {
                                "Verify"
                            } else {
                                "Sign In"
                            };

                            let can_submit = !self.is_loading
                                && !self.username.is_empty()
                                && !self.password.is_empty()
                                && (!self.needs_2fa || !self.verification_code.is_empty());

                            let btn = egui::Button::new(
                                egui::RichText::new(button_text)
                                    .size(15.0)
                                    .color(TEXT_PRIMARY),
                            )
                            .min_size(egui::vec2(300.0, 38.0))
                            .rounding(6.0)
                            .fill(if can_submit { ACCENT } else { egui::Color32::from_rgb(60, 80, 110) });

                            let btn_response = ui.add_enabled(can_submit, btn);

                            // Submit on Enter key or button click
                            let enter_pressed = ui.input(|i: &egui::InputState| i.key_pressed(egui::Key::Enter));
                            if (btn_response.clicked() || enter_pressed) && can_submit {
                                self.attempt_login();
                            }

                            // Hover effect feedback
                            if btn_response.hovered() && can_submit {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                        });

                    ui.add_space(20.0);
                    ui.label(
                        egui::RichText::new("Secure connection to api.agbroadband.net")
                            .size(11.0)
                            .color(egui::Color32::from_rgb(80, 100, 130)),
                    );
                });
            });

        // Auto-focus username field on first frame
        if !self.needs_2fa && self.username.is_empty() {
            // Nothing — egui handles focus automatically for the first text field
        }
    }
}

/// Show the login window. Blocks until user logs in or closes the window.
pub fn show_login_window(auth: &AuthState, rt: &Runtime) {
    let handle = rt.handle().clone();
    let auth_clone = auth.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([440.0, 520.0])
            .with_min_inner_size([400.0, 480.0])
            .with_title("AGB Cloud Client — Login")
            .with_icon(Arc::new(crate::ui::icon::app_icon_data())),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "AGB Cloud Client — Login",
        options,
        Box::new(move |_cc| Ok(Box::new(LoginApp::new(auth_clone, handle)))),
    );
}
