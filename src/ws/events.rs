use anyhow::Result;
use rust_socketio::asynchronous::ClientBuilder;
use tracing::{error, info, warn};

use crate::auth::AuthState;
use crate::config::AppConfig;

/// WebSocket client for real-time cloud file events
pub struct WsClient;

impl WsClient {
    /// Connect to the WebSocket server and listen for events
    pub async fn connect(config: &AppConfig, _auth: &AuthState) -> Result<()> {
        let ws_url = format!("{}/cloud-files/ws", config.websocket_url);
        info!("Connecting to WebSocket: {ws_url}");

        let client = ClientBuilder::new(&ws_url)
            .on("CLOUDFILE.UPLOADED", |payload, _| {
                Box::pin(async move {
                    info!("WS: File uploaded: {:?}", payload);
                    // TODO: trigger download of new file
                })
            })
            .on("CLOUDFILE.DELETED", |payload, _| {
                Box::pin(async move {
                    info!("WS: File deleted: {:?}", payload);
                    // TODO: delete local file
                })
            })
            .on("CLOUDFILE.MOVED", |payload, _| {
                Box::pin(async move {
                    info!("WS: File moved: {:?}", payload);
                    // TODO: move local file
                })
            })
            .on("CLOUDFILE.RENAMED", |payload, _| {
                Box::pin(async move {
                    info!("WS: File renamed: {:?}", payload);
                    // TODO: rename local file
                })
            })
            .on("CLOUDFILE.FOLDER_CREATED", |payload, _| {
                Box::pin(async move {
                    info!("WS: Folder created: {:?}", payload);
                    // TODO: create local folder
                })
            })
            .on("error", |err, _| {
                Box::pin(async move {
                    error!("WS error: {:?}", err);
                })
            })
            .connect()
            .await;

        match client {
            Ok(_socket) => {
                info!("WebSocket connected successfully");
                // Keep alive loop
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                }
            }
            Err(e) => {
                Err(anyhow::anyhow!("WebSocket connection failed: {e}"))
            }
        }
    }
}
