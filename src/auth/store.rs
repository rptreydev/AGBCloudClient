use anyhow::Result;
use keyring::Entry;
use tracing::{debug, info, warn};

const SERVICE: &str = "agb-cloud-client";

/// Secure credential storage using OS keychain (Windows Credential Manager / Linux libsecret)
pub struct CredentialStore;

impl CredentialStore {
    /// Store access token for a user
    pub fn store_token(username: &str, token: &str) -> Result<()> {
        let entry = Entry::new(SERVICE, username)?;
        entry.set_password(token)?;
        info!("Token stored securely for user: {username}");
        Ok(())
    }

    /// Get stored access token for a user
    pub fn get_token(username: &str) -> Result<Option<String>> {
        let entry = Entry::new(SERVICE, username)?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {e}")),
        }
    }

    /// Store refresh token for a user
    pub fn store_refresh_token(username: &str, token: &str) -> Result<()> {
        let key = format!("{username}_refresh");
        let entry = Entry::new(SERVICE, &key)?;
        entry.set_password(token)?;
        debug!("Refresh token stored for user: {username}");
        Ok(())
    }

    /// Get stored refresh token for a user
    pub fn get_refresh_token(username: &str) -> Result<Option<String>> {
        let key = format!("{username}_refresh");
        let entry = Entry::new(SERVICE, &key)?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {e}")),
        }
    }

    /// Store the last logged-in username
    pub fn store_username(username: &str) -> Result<()> {
        let entry = Entry::new(SERVICE, "last_user")?;
        entry.set_password(username)?;
        Ok(())
    }

    /// Get the last logged-in username
    pub fn get_last_username() -> Result<Option<String>> {
        let entry = Entry::new(SERVICE, "last_user")?;
        match entry.get_password() {
            Ok(u) => Ok(Some(u)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {e}")),
        }
    }

    /// Store the user's password for auto-login.
    /// Secured by Windows Credential Manager (DPAPI-encrypted, never stored in plaintext).
    pub fn store_password(username: &str, password: &str) -> Result<()> {
        let key = format!("{username}_password");
        let entry = Entry::new(SERVICE, &key)?;
        entry.set_password(password)?;
        debug!("Password stored securely for auto-login: {username}");
        Ok(())
    }

    /// Get the stored password for auto-login.
    pub fn get_password(username: &str) -> Result<Option<String>> {
        let key = format!("{username}_password");
        let entry = Entry::new(SERVICE, &key)?;
        match entry.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("Keyring error: {e}")),
        }
    }

    /// Remove stored password (called on logout).
    pub fn clear_password(username: &str) {
        let key = format!("{username}_password");
        let _ = Entry::new(SERVICE, &key).and_then(|e| e.delete_credential());
        debug!("Auto-login password cleared for: {username}");
    }

    /// Clear credentials for a specific user
    pub fn clear_user(username: &str) -> Result<()> {
        let _ = Entry::new(SERVICE, username).and_then(|e| e.delete_credential());
        let key = format!("{username}_refresh");
        let _ = Entry::new(SERVICE, &key).and_then(|e| e.delete_credential());
        Self::clear_password(username);
        warn!("Credentials cleared for user: {username}");
        Ok(())
    }

    /// Clear all stored credentials
    pub fn clear_all() -> Result<()> {
        if let Ok(Some(username)) = Self::get_last_username() {
            Self::clear_user(&username)?;
        }
        let _ = Entry::new(SERVICE, "last_user").and_then(|e| e.delete_credential());
        Ok(())
    }
}
