use serde::{Deserialize, Serialize};

/// User model mirroring the NestJS API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, rename = "firstName")]
    pub first_name: Option<String>,
    #[serde(default, rename = "lastName")]
    pub last_name: Option<String>,
    #[serde(default)]
    pub role: Option<UserRole>,
    #[serde(default)]
    pub active: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserRole {
    SADMIN,
    ADMIN,
    TECH,
    SUPERVISOR,
    COMPANY_SUPERVISOR,
    INSTALLER,
    VIEWER,
    /// Catch-all for unknown roles the API may add in the future
    #[serde(other)]
    Unknown,
}

/// Login credentials sent to the API
#[derive(Debug, Clone)]
pub struct LoginCredentials {
    pub username: String,
    pub password: String,
    pub verification_code: Option<String>,
}

/// Auth tokens (stored as HTTP-only cookies by the API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// Login response from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub message: Option<String>,
}
