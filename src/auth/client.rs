use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use anyhow::Result;
use reqwest::Client;
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
            .user_agent("AGB Cloud Client Desktop")
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

    /// Try to restore session from stored credentials
    pub async fn try_restore_session(&self) -> bool {
        let username = match CredentialStore::get_last_username() {
            Ok(Some(u)) => u,
            _ => return false,
        };

        let refresh_token = match CredentialStore::get_refresh_token(&username) {
            Ok(Some(t)) => t,
            _ => return false,
        };

        info!("Attempting session restore for: {username}");

        let url = format!("{}/authentication/refresh", self.config.server_url);
        let resp = match self.client
            .post(&url)
            .header("Cookie", format!("refresh_token={refresh_token}"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("Session restore failed: {e}");
                return false;
            }
        };

        let status = resp.status();
        let (jwt, new_refresh) = Self::extract_cookies(&resp);

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!("Session restore failed: HTTP {status} — {body}");
            return false;
        }

        if let Some(token) = jwt {
            let mut inner = self.inner.write().await;
            inner.access_token = Some(token.clone());
            inner.is_authenticated = true;
            let _ = CredentialStore::store_token(&username, &token);
            if let Some(rt) = new_refresh {
                let _ = CredentialStore::store_refresh_token(&username, &rt);
            }
            true
        } else {
            warn!("Session restore failed: no JWT in response cookies");
            false
        }
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
                let mut inner = self.inner.write().await;
                inner.access_token = Some(token.clone());
                inner.user = response.user;
                inner.is_authenticated = true;

                // Store credentials using username from response if available,
                // or from credentials if provided
                let store_username = if !credentials.username.is_empty() {
                    credentials.username.clone()
                } else if let Some(ref user) = inner.user {
                    user.username.clone()
                } else {
                    "device".to_string()
                };
                CredentialStore::store_token(&store_username, &token)?;
                CredentialStore::store_username(&store_username)?;
                if let Some(rt) = refresh {
                    CredentialStore::store_refresh_token(&store_username, &rt)?;
                }
                return Ok(());
            }

            return Err(anyhow::anyhow!(
                response.message.unwrap_or_else(|| "Invalid verification code".to_string())
            ));
        }

        // Initial login request (may trigger 2FA or direct login)
        let url = format!("{}/authentication/login", self.config.server_url);
        let resp = self.client
            .post(&url)
            .json(&body)
            .send()
            .await?;

        let (jwt, refresh) = Self::extract_cookies(&resp);

        let text = resp.text().await?;
        let response: LoginResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Invalid login response: {e}\n{text}"))?;

        // Direct login — server returned JWT cookie (e.g. Dart-like clients)
        if let Some(token) = jwt {
            let mut inner = self.inner.write().await;
            inner.access_token = Some(token.clone());
            inner.user = response.user;
            inner.is_authenticated = true;

            CredentialStore::store_token(&credentials.username, &token)?;
            CredentialStore::store_username(&credentials.username)?;
            if let Some(rt) = refresh {
                CredentialStore::store_refresh_token(&credentials.username, &rt)?;
            }
            return Ok(());
        }

        // 2FA required — server sent verification code to user's email
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

    /// Refresh the access token (tokens come as cookies)
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

    /// Get current user
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
