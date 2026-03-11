//! Reusable login + 2FA widget shared by the wizard, the standalone login
//! window, and any session-expiry flow.
//!
//! Follows the same `poll` / `render` contract as [`FolderTreeWidget`]:
//!   1. Call `widget.poll(ctx)` every frame from `update()`.
//!   2. Call `widget.render(ui)` to paint the centred login card.
//!
//! `poll` returns `Some(LoginOutcome::Success)` exactly once when the user
//! has been authenticated.  The parent is responsible for reacting (advance
//! wizard step, close window, re-enable UI, etc.).

use std::sync::mpsc;
use eframe::egui;
use tracing::{error, info};

use crate::auth::{AuthState, CredentialStore};
use crate::models::LoginCredentials;
use crate::ui::common::*;

// ── Internal async channel type ───────────────────────────────────────────────

enum LoginResult {
    Success,
    Needs2FA(String),
    Error(String),
}

// ── Public outcome ────────────────────────────────────────────────────────────

/// Returned by [`LoginWidget::poll`] the frame authentication succeeds.
#[derive(Debug, Clone, PartialEq)]
pub enum LoginOutcome {
    Success,
}

// ── Widget ────────────────────────────────────────────────────────────────────

/// Self-contained login + 2FA form widget.
///
/// All async state, channel management, and egui rendering are encapsulated
/// here so callers only need to:
/// - Store a `LoginWidget` field in their struct.
/// - Call `widget.poll(ctx)` every frame and react to `LoginOutcome::Success`.
/// - Call `widget.render(ui)` to draw the centred login card.
pub struct LoginWidget {
    /// Current username — public so callers can pre-fill if desired.
    pub username: String,
    /// Current password.
    pub password: String,
    /// 2-FA verification code.
    pub verification_code: String,
    /// Non-empty when the last attempt failed.
    pub error: String,
    /// Info message shown below the form (e.g. "code sent to your email").
    pub info: String,
    /// `true` while an async login call is in flight.
    pub is_loading: bool,
    /// `true` once the server has sent a 2-FA code and is waiting for it.
    pub needs_2fa: bool,
    /// Whether to save credentials for auto-login on next startup.
    pub remember_me: bool,

    result_rx: Option<mpsc::Receiver<LoginResult>>,
    auth: AuthState,
    handle: tokio::runtime::Handle,
}

impl LoginWidget {
    pub fn new(auth: AuthState, handle: tokio::runtime::Handle) -> Self {
        // Pre-fill username from keychain if available.
        let saved_username = CredentialStore::get_last_username()
            .ok()
            .flatten()
            .unwrap_or_default();
        Self {
            username: saved_username,
            password: String::new(),
            verification_code: String::new(),
            error: String::new(),
            info: String::new(),
            is_loading: false,
            needs_2fa: false,
            remember_me: true,
            result_rx: None,
            auth,
            handle,
        }
    }

    /// Clear all form state.  Call when a session expires while the parent
    /// window is already open and a fresh login is required.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.username.clear();
        self.password.clear();
        self.verification_code.clear();
        self.error.clear();
        self.info.clear();
        self.is_loading = false;
        self.needs_2fa = false;
        self.result_rx = None;
    }

    // ── Async submit ──────────────────────────────────────────────────────────

    fn attempt_login(&mut self) {
        self.is_loading = true;
        self.error.clear();
        self.info.clear();

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
        let (tx, rx) = mpsc::channel();
        self.result_rx = Some(rx);

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

    // ── Poll ──────────────────────────────────────────────────────────────────

    /// Call every frame from the parent's `update()`.
    /// Returns `Some(LoginOutcome::Success)` exactly once when authentication
    /// completes.  Keeps repainting the frame while a request is in flight.
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<LoginOutcome> {
        let result = match self.result_rx.as_ref() {
            Some(rx) => match rx.try_recv() {
                Ok(r) => {
                    self.result_rx = None;
                    Some(r)
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.is_loading = false;
                    self.error = "Connection lost".to_string();
                    self.result_rx = None;
                    None
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if self.is_loading { ctx.request_repaint(); }
                    None
                }
            },
            None => None,
        };

        if let Some(r) = result {
            self.is_loading = false;
            match r {
                LoginResult::Success => {
                    info!("LoginWidget: authenticated successfully");
                    // Save credentials for auto-login if the user opted in.
                    if self.remember_me && !self.password.is_empty() {
                        let _ = CredentialStore::store_password(&self.username, &self.password);
                    } else if !self.remember_me {
                        // User unchecked — clear any previously stored password.
                        CredentialStore::clear_password(&self.username);
                    }
                    ctx.request_repaint();
                    return Some(LoginOutcome::Success);
                }
                LoginResult::Needs2FA(msg) => {
                    self.needs_2fa = true;
                    self.info = msg;
                    ctx.request_repaint();
                }
                LoginResult::Error(msg) => {
                    error!("LoginWidget: {msg}");
                    self.error = msg;
                    ctx.request_repaint();
                }
            }
        }
        None
    }

    // ── Render ────────────────────────────────────────────────────────────────

    /// Render the login card centred in the available space.
    ///
    /// Does **not** include a branding header — add one above in the caller if
    /// desired (standalone window adds "AGBroadband / Cloud Files"; the wizard
    /// relies on the step header instead).
    pub fn render(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            egui::Frame::default()
                .fill(CARD_BG)
                .rounding(16.0)
                .inner_margin(egui::Margin::same(32.0))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(38, 50, 72)))
                .show(ui, |ui: &mut egui::Ui| {
                    ui.set_width(340.0);

                    if !self.needs_2fa {
                        // ── Step 1: credentials ───────────────────────────────
                        ui.label(
                            egui::RichText::new("Sign In")
                                .size(24.0).color(TEXT_PRIMARY).strong(),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Enter your AGBroadband credentials")
                                .size(13.0).color(TEXT_SECONDARY),
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
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.remember_me, "");
                            ui.label(
                                egui::RichText::new("Remember me (auto sign-in on startup)")
                                    .size(12.0)
                                    .color(TEXT_SECONDARY),
            );
                        });
                        ui.add_space(8.0);
                    } else {
                        // ── Step 2: 2FA code ─────────────────────────────────
                        ui.label(
                            egui::RichText::new("Verification")
                                .size(24.0).color(TEXT_PRIMARY).strong(),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Enter the code sent to your email")
                                .size(13.0).color(TEXT_SECONDARY),
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

                    // ── Feedback cards ────────────────────────────────────────
                    if !self.info.is_empty() {
                        egui::Frame::default()
                            .fill(egui::Color32::from_rgb(0, 50, 70))
                            .rounding(8.0)
                            .inner_margin(egui::Margin::same(10.0))
                            .show(ui, |ui: &mut egui::Ui| {
                                ui.label(
                                    egui::RichText::new(&self.info).size(12.0).color(ACCENT),
                                );
                            });
                        ui.add_space(8.0);
                    }

                    if !self.error.is_empty() {
                        egui::Frame::default()
                            .fill(egui::Color32::from_rgb(60, 20, 20))
                            .rounding(8.0)
                            .inner_margin(egui::Margin::same(10.0))
                            .show(ui, |ui: &mut egui::Ui| {
                                ui.label(
                                    egui::RichText::new(&self.error).size(12.0).color(ERROR_COLOR),
                                );
                            });
                        ui.add_space(8.0);
                    }

                    // ── Submit button ─────────────────────────────────────────
                    ui.add_space(8.0);
                    let button_text = if self.is_loading {
                        if self.needs_2fa { "Verifying..." } else { "Signing in..." }
                    } else if self.needs_2fa {
                        "Verify"
                    } else {
                        "Sign In"
                    };

                    let can_submit = !self.is_loading && if self.needs_2fa {
                        !self.verification_code.is_empty()
                    } else {
                        !self.username.is_empty() && !self.password.is_empty()
                    };

                    let btn = filled_button(button_text, can_submit)
                        .min_size(egui::vec2(320.0, 42.0));
                    let btn_response = ui.add_enabled(can_submit, btn);

                    let enter_pressed = ui.input(|i: &egui::InputState| {
                        i.key_pressed(egui::Key::Enter)
                    });
                    if (btn_response.clicked() || enter_pressed) && can_submit {
                        self.attempt_login();
                    }
                    if btn_response.hovered() && can_submit {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                });
        });
    }
}
