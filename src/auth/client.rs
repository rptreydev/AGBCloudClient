use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use anyhow::Result;
use reqwest::Client;

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
use tracing::{debug, info, warn};

use crate::config::AppConfig;
use crate::models::{LoginCredentials, LoginResponse, User};
use super::store::CredentialStore;

/// Manages authentication state and API communication
#[derive(Clone)]
pub struct AuthState {
    config: AppConfig,
    client: Client,
    inner: Arc<RwLock<AuthInner>>,
}

struct AuthInner {
    user: Option<User>,
    access_token: Option<String>,
    is_authenticated: bool,
}

impl AuthState {
    pub fn new(config: AppConfig) -> Self {
        // Build HTTP client with cookie support
        let client = Client::builder()
            .danger_accept_invalid_certs(true) // Dev self-signed certs
            .cookie_store(true)
            .user_agent("AGB-Desktop/0.1.0")
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            config,
            client,
            inner: Arc::new(RwLock::new(AuthInner {
                user: None,
                access_token: None,
                is_authenticated: false,
            })),
        }
    }

    /// Try to restore session from stored credentials.
    ///
    /// Strategy:
    /// 1. If a `refresh_token` is stored, call `/authentication/refresh` to get a
    ///    fresh JWT (preferred — JWT may have expired).
    /// 2. Fallback: if only a plain JWT is stored (e.g. right after a fresh login
    ///    where the server didn't issue a refresh_token), use it directly.  The
    ///    sync engine will detect expiry on the first API call and prompt re-login.
    pub async fn try_restore_session(&self) -> bool {
        let username = match CredentialStore::get_last_username() {
            Ok(Some(u)) => u,
            _ => return false,
        };

        // ── Path 1: refresh_token available → hit the refresh endpoint ─────────
        if let Ok(Some(refresh_token)) = CredentialStore::get_refresh_token(&username) {
            info!("Attempting session restore via refresh token for: {username}");

            let url = format!("{}/authentication/refresh", self.config.server_url);
            let resp = match self.client
                .post(&url)
                .header("Cookie", format!("refresh_token={refresh_token}"))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!("Session restore (refresh) failed: {e}");
                    // Don't return yet — fall through to JWT fallback below.
                    return self.restore_from_stored_jwt(&username).await;
                }
            };

            let status = resp.status();
            let (jwt, new_refresh) = Self::extract_cookies(&resp);

            if status.is_success() {
                if let Some(token) = jwt {
                    let mut inner = self.inner.write().await;
                    inner.access_token = Some(token.clone());
                    inner.is_authenticated = true;
                    // Rebuild minimal user (with role) from stored credentials.
                    inner.user = Self::rebuild_user_from_store(&username);
                    if let Err(e) = CredentialStore::store_token(&username, &token) {
                        warn!("Failed to cache refreshed JWT (non-critical): {e}");
                    }
                    if let Some(rt) = new_refresh {
                        if let Err(e) = CredentialStore::store_refresh_token(&username, &rt) {
                            warn!("Failed to cache new refresh token (non-critical): {e}");
                        }
                    }
                    info!("Session restored via refresh token for: {username}");
                    return true;
                }
            }

            let body = resp.text().await.unwrap_or_default();
            warn!("Session restore (refresh) failed: HTTP {status} — {body}");
            // Fall through to JWT fallback.
        }

        // ── Path 2: no refresh_token (e.g. fresh login) → use stored JWT ──────
        if self.restore_from_stored_jwt(&username).await {
            return true;
        }

        // ── Path 3: JWT also expired → auto-login with stored password ──────────
        self.auto_login_with_stored_password(&username).await
    }

    /// Restore session using only a stored JWT (no refresh call).
    /// Validates the token against the server before accepting it — a stored JWT
    /// may be expired (30-min TTL) even though it exists in the keychain.
    async fn restore_from_stored_jwt(&self, username: &str) -> bool {
        let token = match CredentialStore::get_token(username) {
            Ok(Some(t)) => t,
            _ => {
                warn!("No stored JWT for: {username} — session restore failed");
                return false;
            }
        };

        // Validate the token with a lightweight call before accepting it.
        // If the server returns 401 the token is expired; fall through to Path 3.
        let profile_url = format!("{}/authentication/profile", self.config.server_url);
        match self.client
            .get(&profile_url)
            .header("Cookie", format!("jwt={token}"))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let mut inner = self.inner.write().await;
                inner.access_token = Some(token);
                inner.is_authenticated = true;
                // Rebuild a minimal User with the stored role so role-based routing
                // (e.g. COMPANY_SUPERVISOR → filtered folder endpoint) works after restart.
                inner.user = Self::rebuild_user_from_store(username);
                info!("Session restored from stored JWT for: {username}");
                true
            }
            Ok(resp) => {
                warn!(
                    "Stored JWT is invalid/expired (HTTP {}) for: {username} — falling through to auto-login",
                    resp.status()
                );
                false
            }
            Err(e) => {
                // Network error — accept the stored JWT optimistically so the app
                // can start offline; API calls will fail individually if server is down.
                warn!("JWT validation call failed (network): {e} — accepting stored JWT optimistically");
                let mut inner = self.inner.write().await;
                inner.access_token = Some(token);
                inner.is_authenticated = true;
                inner.user = Self::rebuild_user_from_store(username);
                true
            }
        }
    }

    /// Build a minimal `User` from credentials stored in the keychain.
    /// Only the `username` and `role` fields are populated; everything else is `None`.
    fn rebuild_user_from_store(username: &str) -> Option<crate::models::User> {
        let role_str = CredentialStore::get_user_role(username).ok()??;
        // Deserialize the SCREAMING_SNAKE_CASE role name from the stored string.
        let role = serde_json::from_str::<crate::models::UserRole>(
            &format!("\"{}\"", role_str)
        ).ok()?;
        Some(crate::models::User {
            id: None,
            username: username.to_string(),
            email: None,
            first_name: None,
            last_name: None,
            role: Some(role),
            active: None,
        })
    }

    /// Returns the username of the currently authenticated user, if any.
    pub async fn current_username(&self) -> Option<String> {
        self.inner.read().await.user.as_ref().map(|u| u.username.clone())
    }

    /// Returns `true` when the current session belongs to a `COMPANY_SUPERVISOR`
    /// or `SUPERVISOR` user — used to select the appropriate folder fetch endpoint.
    pub async fn is_company_supervisor(&self) -> bool {
        use crate::models::UserRole;
        matches!(
            self.inner.read().await.user.as_ref().and_then(|u| u.role.as_ref()),
            Some(UserRole::COMPANY_SUPERVISOR) | Some(UserRole::SUPERVISOR)
        )
    }

    /// Auto-login using stored password (Path 3).
    /// Only used when both refresh_token and JWT paths fail.
    /// Safe because the password is stored encrypted in the OS keychain (DPAPI).
    async fn auto_login_with_stored_password(&self, username: &str) -> bool {
        let password = match CredentialStore::get_password(username) {
            Ok(Some(p)) => p,
            _ => return false,
        };
        info!("Attempting auto-login with stored credentials for: {username}");
        let credentials = crate::models::LoginCredentials {
            username: username.to_string(),
            password,
            verification_code: None,
        };
        match self.login(credentials).await {
            Ok(()) => {
                info!("Auto-login successful for: {username}");
                true
            }
            Err(e) => {
                warn!("Auto-login failed for {username}: {e}");
                false
            }
        }
    }

    /// Store auth state + persist credentials to keychain after a successful login.
    /// Centralises the duplicate logic shared by the direct-login and verify paths.
    async fn persist_session(
        &self,
        username: &str,
        token: &str,
        refresh: Option<String>,
        user: Option<crate::models::User>,
    ) -> Result<()> {
        // Store user role so it can be restored across process restarts.
        if let Some(u) = &user {
            if let Some(role) = &u.role {
                // UserRole serializes to its SCREAMING_SNAKE_CASE name via serde.
                if let Ok(role_json) = serde_json::to_string(role) {
                    // Strip surrounding quotes from the JSON string value.
                    let role_str = role_json.trim_matches('"');
                    if let Err(e) = CredentialStore::store_user_role(username, role_str) {
                        warn!("Failed to store user role (non-critical): {e}");
                    }
                }
            }
        }
        {
            let mut inner = self.inner.write().await;
            inner.access_token = Some(token.to_string());
            inner.user = user;
            inner.is_authenticated = true;
        }
        CredentialStore::store_token(username, token)?;
        CredentialStore::store_username(username)?;
        if let Some(rt) = refresh {
            CredentialStore::store_refresh_token(username, &rt)?;
        }
        Ok(())
    }

    /// Login with credentials (handles 2FA flow).
    /// Tokens come as HTTP-only cookies from the NestJS API, not in the JSON body.
    pub async fn login(&self, credentials: LoginCredentials) -> Result<()> {
        let body = serde_json::json!({
            "username": credentials.username,
            "password": credentials.password,
        });

        // If verification code is provided, use the verify endpoint
        if let Some(code) = &credentials.verification_code {
            let verify_url = format!("{}/authentication/verify", self.config.server_url);
            // If username/password are provided, include them (legacy 2FA flow).
            // Otherwise send code-only (device pairing flow from wizard).
            let verify_body = if credentials.username.is_empty() {
                serde_json::json!({ "code": code })
            } else {
                serde_json::json!({
                    "username": credentials.username,
                    "password": credentials.password,
                    "code": code,
                })
            };

            let resp = self.client
                .post(&verify_url)
                .json(&verify_body)
                .send()
                .await?;

            // Extract JWT and refresh_token from Set-Cookie headers
            let (jwt, refresh) = Self::extract_cookies(&resp);

            let text = resp.text().await?;
            let response: LoginResponse = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("Invalid verify response: {e}\n{text}"))?;

            if let Some(token) = jwt {
                // Determine the canonical username: prefer the one the user typed;
                // fall back to the username returned in the response body.
                let store_username = if !credentials.username.is_empty() {
                    credentials.username.clone()
                } else {
                    response.user.as_ref()
                        .map(|u| u.username.clone())
                        .unwrap_or_else(|| "device".to_string())
                };
                self.persist_session(&store_username, &token, refresh, response.user).await?;
                return Ok(());
            }

            return Err(anyhow::anyhow!(
                response.message.unwrap_or_else(|| "Invalid verification code".to_string())
            ));
        }

        // Initial login request — native endpoint, direct login (no 2FA)
        let url = format!("{}/authentication/native/login", self.config.server_url);
        let resp = self.client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    anyhow::anyhow!("Unable to connect to server. Check your connection.")
                } else if e.is_timeout() {
                    anyhow::anyhow!("Connection timed out. Try again.")
                } else {
                    anyhow::anyhow!("Network error. Please try again.")
                }
            })?;

        // Save status before the response body is consumed.
        let status = resp.status();
        let (jwt, refresh) = Self::extract_cookies(&resp);

        let text = resp.text().await?;
        let response: LoginResponse = serde_json::from_str(&text)
            .map_err(|_| anyhow::anyhow!("Unexpected server response. Please try again."))?;

        // Direct login — server returned JWT cookie (e.g. Dart-like clients)
        if let Some(token) = jwt {
            self.persist_session(&credentials.username, &token, refresh, response.user).await?;
            return Ok(());
        }

        // HTTP error (wrong credentials, account locked, etc.) — surface the
        // backend message but never expose internal route paths.
        if !status.is_success() {
            let raw = response.message
                .unwrap_or_else(|| format!("Login failed ({})", status.as_u16()));
            // Strip internal paths like "Cannot POST /api/v2/..." that the server
            // may return on 404 — they reveal infrastructure and confuse users.
            let msg = if raw.starts_with("Cannot ") || raw.contains("/api/") {
                format!("Authentication service unavailable ({}). Try again later.", status.as_u16())
            } else {
                raw
            };
            return Err(anyhow::anyhow!("{}", msg));
        }

        // HTTP 2xx with no JWT → server sent a verification code to the user's email.
        if response.message.is_some() {
            return Err(anyhow::anyhow!("2FA_REQUIRED"));
        }

        Err(anyhow::anyhow!("Login failed: unexpected response"))
    }

    /// Extract jwt and refresh_token from response cookies
    fn extract_cookies(resp: &reqwest::Response) -> (Option<String>, Option<String>) {
        let mut jwt = None;
        let mut refresh = None;

        // Log response status for debugging
        debug!("Response status: {}", resp.status());

        // First try resp.cookies() (parsed Set-Cookie headers)
        for cookie in resp.cookies() {
            debug!("Cookie found: {}={}", cookie.name(), &cookie.value()[..cookie.value().len().min(20)]);
            match cookie.name() {
                "jwt" => jwt = Some(cookie.value().to_string()),
                "refresh_token" => refresh = Some(cookie.value().to_string()),
                _ => {}
            }
        }

        // Fallback: parse raw Set-Cookie headers (in case cookie_store consumed them)
        if jwt.is_none() {
            for value in resp.headers().get_all("set-cookie") {
                if let Ok(s) = value.to_str() {
                    debug!("Raw Set-Cookie: {}", &s[..s.len().min(60)]);
                    if let Some(token) = Self::parse_cookie_value(s, "jwt") {
                        jwt = Some(token);
                    }
                    if let Some(token) = Self::parse_cookie_value(s, "refresh_token") {
                        refresh = Some(token);
                    }
                }
            }
        }

        debug!("Extracted jwt: {}, refresh: {}", jwt.is_some(), refresh.is_some());
        (jwt, refresh)
    }

    /// Parse a cookie value from a raw Set-Cookie header string
    fn parse_cookie_value(header: &str, name: &str) -> Option<String> {
        let prefix = format!("{}=", name);
        if header.starts_with(&prefix) {
            let rest = &header[prefix.len()..];
            let value = rest.split(';').next().unwrap_or("");
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        None
    }

    /// Refresh the access token (tokens come as cookies).
    /// Reserved for explicit token-refresh calls (Phase 4).
    #[allow(dead_code)]
    pub async fn refresh_token(&self) -> Result<()> {
        let username = CredentialStore::get_last_username()?
            .ok_or_else(|| anyhow::anyhow!("No stored username"))?;

        let refresh_token = CredentialStore::get_refresh_token(&username)?
            .ok_or_else(|| anyhow::anyhow!("No stored refresh token"))?;

        let url = format!("{}/authentication/refresh", self.config.server_url);
        let resp = self.client
            .post(&url)
            .header("Cookie", format!("refresh_token={refresh_token}"))
            .send()
            .await?;

        let (jwt, new_refresh) = Self::extract_cookies(&resp);

        if let Some(token) = jwt {
            let mut inner = self.inner.write().await;
            inner.access_token = Some(token.clone());
            CredentialStore::store_token(&username, &token)?;
            if let Some(rt) = new_refresh {
                CredentialStore::store_refresh_token(&username, &rt)?;
            }
            info!("Token refreshed successfully");
        }
        Ok(())
    }

    /// Logout and clear credentials
    pub async fn logout(&self) -> Result<()> {
        let inner = self.inner.read().await;
        if let Some(token) = &inner.access_token {
            let url = format!("{}/authentication/logout", self.config.server_url);
            let _ = self.client
                .post(&url)
                .bearer_auth(token)
                .send()
                .await;
        }
        drop(inner);

        let mut inner = self.inner.write().await;
        inner.user = None;
        inner.access_token = None;
        inner.is_authenticated = false;

        CredentialStore::clear_all()?;
        info!("Logged out successfully");
        Ok(())
    }

    /// Get current access token for API calls
    pub async fn get_token(&self) -> Option<String> {
        self.inner.read().await.access_token.clone()
    }

    /// Get current user (reserved for Settings / profile display in Phase 4).
    #[allow(dead_code)]
    pub async fn get_user(&self) -> Option<User> {
        self.inner.read().await.user.clone()
    }

    /// Check if authenticated
    pub async fn is_authenticated(&self) -> bool {
        self.inner.read().await.is_authenticated
    }

    /// Get the HTTP client (with cookies)
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Get server URL
    pub fn server_url(&self) -> &str {
        &self.config.server_url
    }
}
