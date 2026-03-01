use anyhow::Result;
use reqwest::multipart;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info};

use crate::auth::AuthState;
use crate::models::CloudFile;

/// Handles all remote API operations for cloud files.
///
/// Auth is handled via cookies — the shared `reqwest::Client` in `AuthState`
/// has `cookie_store(true)`, so the JWT cookie set during login is sent
/// automatically with every request. No `bearer_auth()` needed.
pub struct RemoteClient {
    auth: AuthState,
}

impl RemoteClient {
    pub fn new(auth: AuthState) -> Self {
        Self { auth }
    }

    /// Fetch root folders from the API.
    /// `GET /cloud-file` — requires auth (JWT cookie).
    pub async fn get_roots(&self) -> Result<Vec<CloudFile>> {
        let url = format!("{}/cloud-file", self.auth.server_url());
        debug!("GET {url}");

        let resp = self.auth.client()
            .get(&url)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            error!("get_roots failed ({status}): {text}");
            anyhow::bail!("get_roots failed with status {status}");
        }

        serde_json::from_str(&text).map_err(|e| {
            error!("get_roots parse error: {e}");
            error!("Response body: {text}");
            anyhow::anyhow!("Failed to parse roots response: {e}")
        })
    }

    /// Fetch children of a folder by UUID.
    /// `GET /cloud-file/:uuid` — public endpoint, returns `{"uuid":"...","children":[...]}`.
    pub async fn get_children(&self, folder_uuid: &str) -> Result<Vec<CloudFile>> {
        let url = format!("{}/cloud-file/{}", self.auth.server_url(), folder_uuid);
        debug!("GET {url}");

        let resp = self.auth.client()
            .get(&url)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            error!("get_children({folder_uuid}) failed ({status}): {text}");
            anyhow::bail!("get_children failed with status {status}");
        }

        // API returns a single object with a `children` array, not a flat array
        let parent: CloudFile = serde_json::from_str(&text).map_err(|e| {
            error!("get_children({folder_uuid}) parse error: {e}");
            error!("Response body (first 500 chars): {}", &text[..text.len().min(500)]);
            anyhow::anyhow!("Failed to parse children response: {e}")
        })?;
        Ok(parent.children.unwrap_or_default())
    }

    /// Download a file by UUID.
    /// `GET /cloud-file/download/:uuid` — public endpoint with Range support.
    pub async fn download_file(&self, uuid: &str, dest_path: &str) -> Result<()> {
        let url = format!("{}/cloud-file/download/{}", self.auth.server_url(), uuid);
        debug!("GET {url} -> {dest_path}");

        let resp = self.auth.client()
            .get(&url)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            error!("download_file({uuid}) failed ({status}): {text}");
            anyhow::bail!("Download failed with status {status}");
        }

        let bytes = resp.bytes().await?;
        tokio::fs::write(dest_path, &bytes).await?;
        info!("Downloaded file to: {dest_path}");
        Ok(())
    }

    /// Upload a file to a specific folder.
    /// `POST /cloud-file/folder/:uuid` — requires auth.
    pub async fn upload_file(&self, folder_uuid: &str, file_path: &str) -> Result<CloudFile> {
        let mut file = File::open(file_path).await?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).await?;

        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        let part = multipart::Part::bytes(buffer)
            .file_name(file_name)
            .mime_str("application/octet-stream")?;

        let form = multipart::Form::new().part("files", part);

        let url = format!("{}/cloud-file/folder/{}", self.auth.server_url(), folder_uuid);
        debug!("POST {url}");

        let resp = self.auth.client()
            .post(&url)
            .multipart(form)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            error!("upload_file({file_path}) failed ({status}): {text}");
            anyhow::bail!("Upload failed with status {status}");
        }

        let response: CloudFile = serde_json::from_str(&text).map_err(|e| {
            error!("upload_file parse error: {e}");
            anyhow::anyhow!("Failed to parse upload response: {e}")
        })?;

        info!("Uploaded file: {file_path}");
        Ok(response)
    }

    /// Delete a file by UUID.
    /// `DELETE /cloud-file/:uuid` — requires auth.
    pub async fn delete_file(&self, uuid: &str) -> Result<()> {
        let url = format!("{}/cloud-file/{}", self.auth.server_url(), uuid);
        debug!("DELETE {url}");

        let resp = self.auth.client()
            .delete(&url)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            error!("delete_file({uuid}) failed ({status}): {text}");
            anyhow::bail!("Delete failed with status {status}");
        }

        info!("Deleted remote file: {uuid}");
        Ok(())
    }

    /// Get full tree structure for a folder.
    /// `GET /cloud-file/folders/:uuid` — public endpoint, returns `[{...with children...}]`.
    pub async fn get_tree(&self, root_uuid: &str) -> Result<CloudFile> {
        let url = format!("{}/cloud-file/folders/{}", self.auth.server_url(), root_uuid);
        debug!("GET {url}");

        let resp = self.auth.client()
            .get(&url)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            error!("get_tree({root_uuid}) failed ({status}): {text}");
            anyhow::bail!("get_tree failed with status {status}");
        }

        // API returns an array with the root node (including nested children).
        // Parse as array and take the first element.
        let nodes: Vec<CloudFile> = serde_json::from_str(&text).map_err(|e| {
            error!("get_tree({root_uuid}) parse error: {e}");
            error!("Response body (first 500 chars): {}", &text[..text.len().min(500)]);
            anyhow::anyhow!("Failed to parse tree response: {e}")
        })?;
        nodes.into_iter().next()
            .ok_or_else(|| anyhow::anyhow!("Empty tree response for {root_uuid}"))
    }
}
