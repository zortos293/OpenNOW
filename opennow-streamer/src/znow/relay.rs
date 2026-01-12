//! ZNow Relay Client
//!
//! WebSocket client for communicating with the ZNow relay server.
//! Handles pairing with znow-runner and sending commands.

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn, error};

const DEFAULT_RELAY_WS_URL: &str = "wss://znow.zortos.me/ws/client";

/// Messages sent to the relay server
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum OutgoingMessage {
    #[serde(rename = "CLIENT_CONNECT")]
    ClientConnect { sessionCode: String },
    #[serde(rename = "PAIR_REQUEST")]
    PairRequest { clientCode: String, exeCode: String },
    #[serde(rename = "INSTALL_APP")]
    InstallApp { appId: String },
    #[serde(rename = "LAUNCH_APP")]
    LaunchApp { appId: String },
    #[serde(rename = "HEARTBEAT")]
    Heartbeat,
    // File transfer messages
    #[serde(rename = "FILE_UPLOAD_START")]
    FileUploadStart {
        transferId: String,
        fileName: String,
        fileSize: u64,
        mimeType: Option<String>,
    },
    #[serde(rename = "FILE_UPLOAD_CHUNK")]
    FileUploadChunk {
        transferId: String,
        chunkIndex: u32,
        data: String, // base64 encoded
    },
    #[serde(rename = "FILE_UPLOAD_COMPLETE")]
    FileUploadComplete { transferId: String },
    #[serde(rename = "FILE_UPLOAD_CANCEL")]
    FileUploadCancel { transferId: String },
}

/// Messages received from the relay server
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum IncomingMessage {
    #[serde(rename = "CONNECTED")]
    Connected { sessionId: Option<String> },
    #[serde(rename = "PAIRED")]
    Paired { success: bool, sessionId: Option<String> },
    #[serde(rename = "COMMAND_SENT")]
    CommandSent { action: String, appId: String },
    #[serde(rename = "STATUS_UPDATE")]
    StatusUpdate {
        state: String,
        progress: Option<u8>,
        message: Option<String>,
    },
    #[serde(rename = "EXE_DISCONNECTED")]
    ExeDisconnected,
    #[serde(rename = "HEARTBEAT_ACK")]
    HeartbeatAck,
    #[serde(rename = "ERROR")]
    Error { message: String },
    // File transfer responses
    #[serde(rename = "FILE_UPLOAD_ACK")]
    FileUploadAck { transferId: String },
    #[serde(rename = "FILE_UPLOAD_PROGRESS")]
    FileUploadProgress {
        transferId: String,
        bytesReceived: u64,
    },
    #[serde(rename = "FILE_UPLOAD_SUCCESS")]
    FileUploadSuccess {
        transferId: String,
        savedPath: String,
    },
    #[serde(rename = "FILE_UPLOAD_ERROR")]
    FileUploadError {
        transferId: String,
        error: String,
    },
}

/// Events emitted by the relay client
#[derive(Debug, Clone)]
pub enum RelayEvent {
    Connected,
    Paired { session_id: Option<String> },
    StatusUpdate {
        state: String,
        progress: Option<u8>,
        message: Option<String>,
    },
    ExeDisconnected,
    Error(String),
    Disconnected,
    // File transfer events
    FileUploadAck { transfer_id: String },
    FileUploadProgress { transfer_id: String, bytes_received: u64 },
    FileUploadSuccess { transfer_id: String, saved_path: String },
    FileUploadError { transfer_id: String, error: String },
}

pub struct ZNowRelayClient {
    url: String,
    session_code: String,
}

impl ZNowRelayClient {
    pub fn new(session_code: &str) -> Self {
        Self::with_url(DEFAULT_RELAY_WS_URL, session_code)
    }

    pub fn with_url(url: &str, session_code: &str) -> Self {
        Self {
            url: url.to_string(),
            session_code: session_code.to_string(),
        }
    }

    /// Connect to the relay server and run the message loop
    /// Returns a channel for sending commands and a channel for receiving events
    pub async fn connect(
        &self,
    ) -> Result<
        (
            mpsc::Sender<OutgoingMessage>,
            mpsc::Receiver<RelayEvent>,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        info!("Connecting to ZNow relay: {}", self.url);

        let (ws_stream, _) = connect_async(&self.url).await?;
        let (mut write, mut read) = ws_stream.split();

        info!("Connected to ZNow relay server");

        // Create channels
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<OutgoingMessage>(32);
        let (event_tx, event_rx) = mpsc::channel::<RelayEvent>(32);

        // Send initial connection message
        let connect_msg = OutgoingMessage::ClientConnect {
            sessionCode: self.session_code.clone(),
        };
        let json = serde_json::to_string(&connect_msg)?;
        write.send(Message::Text(json)).await?;

        // Clone for the read task
        let event_tx_read = event_tx.clone();

        // Spawn task to handle incoming messages
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<IncomingMessage>(&text) {
                            Ok(incoming) => {
                                let event = match incoming {
                                    IncomingMessage::Connected { .. } => {
                                        Some(RelayEvent::Connected)
                                    }
                                    IncomingMessage::Paired { success, sessionId } => {
                                        if success {
                                            Some(RelayEvent::Paired { session_id: sessionId })
                                        } else {
                                            Some(RelayEvent::Error("Pairing failed".to_string()))
                                        }
                                    }
                                    IncomingMessage::StatusUpdate { state, progress, message } => {
                                        Some(RelayEvent::StatusUpdate { state, progress, message })
                                    }
                                    IncomingMessage::ExeDisconnected => {
                                        Some(RelayEvent::ExeDisconnected)
                                    }
                                    IncomingMessage::Error { message } => {
                                        Some(RelayEvent::Error(message))
                                    }
                                    IncomingMessage::CommandSent { .. } => None,
                                    IncomingMessage::HeartbeatAck => None,
                                    // File transfer responses
                                    IncomingMessage::FileUploadAck { transferId } => {
                                        Some(RelayEvent::FileUploadAck { transfer_id: transferId })
                                    }
                                    IncomingMessage::FileUploadProgress { transferId, bytesReceived } => {
                                        Some(RelayEvent::FileUploadProgress {
                                            transfer_id: transferId,
                                            bytes_received: bytesReceived,
                                        })
                                    }
                                    IncomingMessage::FileUploadSuccess { transferId, savedPath } => {
                                        Some(RelayEvent::FileUploadSuccess {
                                            transfer_id: transferId,
                                            saved_path: savedPath,
                                        })
                                    }
                                    IncomingMessage::FileUploadError { transferId, error } => {
                                        Some(RelayEvent::FileUploadError {
                                            transfer_id: transferId,
                                            error,
                                        })
                                    }
                                };

                                if let Some(event) = event {
                                    if event_tx_read.send(event).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to parse relay message: {} - {}", text, e);
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        info!("Relay server closed connection");
                        let _ = event_tx_read.send(RelayEvent::Disconnected).await;
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket error: {}", e);
                        let _ = event_tx_read.send(RelayEvent::Error(e.to_string())).await;
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Spawn task to handle outgoing messages
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if let Ok(json) = serde_json::to_string(&cmd) {
                    if write.send(Message::Text(json)).await.is_err() {
                        break;
                    }
                }
            }
        });

        Ok((cmd_tx, event_rx))
    }
}
