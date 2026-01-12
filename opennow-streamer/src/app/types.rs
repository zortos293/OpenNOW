//! Application Types
//!
//! Common types used across the application.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::config::{ColorQuality, VideoCodec, VideoDecoderBackend};
use crate::media::VideoFrame;

/// Shared frame holder for zero-latency frame delivery
/// Decoder writes latest frame, renderer reads it - no buffering
pub struct SharedFrame {
    frame: Mutex<Option<VideoFrame>>,
    frame_count: AtomicU64,
    last_read_count: AtomicU64,
}

impl SharedFrame {
    pub fn new() -> Self {
        Self {
            frame: Mutex::new(None),
            frame_count: AtomicU64::new(0),
            last_read_count: AtomicU64::new(0),
        }
    }

    /// Write a new frame (called by decoder)
    pub fn write(&self, frame: VideoFrame) {
        *self.frame.lock() = Some(frame);
        self.frame_count.fetch_add(1, Ordering::Release);
    }

    /// Check if there's a new frame since last read
    pub fn has_new_frame(&self) -> bool {
        let current = self.frame_count.load(Ordering::Acquire);
        let last = self.last_read_count.load(Ordering::Acquire);
        current > last
    }

    /// Read the latest frame (called by renderer)
    /// Returns None if no frame available or no new frame since last read
    /// Uses take() instead of clone() to avoid copying ~3MB per frame
    pub fn read(&self) -> Option<VideoFrame> {
        let current = self.frame_count.load(Ordering::Acquire);
        let last = self.last_read_count.load(Ordering::Acquire);

        if current > last {
            self.last_read_count.store(current, Ordering::Release);
            self.frame.lock().take() // Move instead of clone - zero copy
        } else {
            None
        }
    }

    /// Get frame count for stats
    pub fn frame_count(&self) -> u64 {
        self.frame_count.load(Ordering::Relaxed)
    }
}

impl Default for SharedFrame {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse resolution string (e.g., "1920x1080") into (width, height)
/// Returns (1920, 1080) as default if parsing fails
pub fn parse_resolution(res: &str) -> (u32, u32) {
    let parts: Vec<&str> = res.split('x').collect();
    if parts.len() == 2 {
        let width = parts[0].parse().unwrap_or(1920);
        let height = parts[1].parse().unwrap_or(1080);
        (width, height)
    } else {
        (1920, 1080) // Default to 1080p
    }
}

/// Game variant (platform/store option)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GameVariant {
    pub id: String,
    pub store: String,
    #[serde(default)]
    pub supported_controls: Vec<String>,
}

/// Game information
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GameInfo {
    pub id: String,
    pub title: String,
    pub publisher: Option<String>,
    pub image_url: Option<String>,
    pub store: String,
    pub app_id: Option<i64>,
    #[serde(default)]
    pub is_install_to_play: bool,
    #[serde(default)]
    pub play_type: Option<String>,
    #[serde(default)]
    pub membership_tier_label: Option<String>,
    #[serde(default)]
    pub playability_text: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Available platform variants (e.g., Steam, Epic, Xbox)
    #[serde(default)]
    pub variants: Vec<GameVariant>,
    /// Index of the currently selected variant
    #[serde(default)]
    pub selected_variant_index: usize,
}

/// Section of games with a title (e.g., "Trending", "Free to Play")
#[derive(Debug, Clone, Default)]
pub struct GameSection {
    pub id: Option<String>,
    pub title: String,
    pub games: Vec<GameInfo>,
}

/// Subscription information
#[derive(Debug, Clone, Default)]
pub struct SubscriptionInfo {
    pub membership_tier: String,
    pub remaining_hours: f32,
    pub total_hours: f32,
    pub has_persistent_storage: bool,
    pub storage_size_gb: Option<u32>,
    pub is_unlimited: bool, // true if subType is UNLIMITED (no hour cap)
    pub entitled_resolutions: Vec<EntitledResolution>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct EntitledResolution {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Current tab in Games view
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GamesTab {
    Home,       // Sectioned home view (like official GFN client)
    AllGames,   // Flat grid view
    MyLibrary,  // User's library
    QueueTimes, // Queue times for games (hidden, for free tier users)
    ZNow,       // ZNow portable apps launcher
}

impl Default for GamesTab {
    fn default() -> Self {
        GamesTab::Home // Default to sectioned home view
    }
}

/// Sort mode for queue times display
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueSortMode {
    #[default]
    BestValue, // Balanced score of ping + queue time (recommended)
    QueueTime,    // Shortest queue first
    Ping,         // Lowest ping first
    Alphabetical, // A-Z by server name
}

impl QueueSortMode {
    pub fn label(&self) -> &'static str {
        match self {
            QueueSortMode::BestValue => "Best Value",
            QueueSortMode::QueueTime => "Shortest Queue",
            QueueSortMode::Ping => "Lowest Ping",
            QueueSortMode::Alphabetical => "A-Z",
        }
    }
}

/// Filter mode for queue times display
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum QueueRegionFilter {
    #[default]
    All,
    Region(String), // Filter by specific region
}

/// Server/Region information
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub id: String,
    pub name: String,
    pub region: String,
    pub url: Option<String>,
    pub ping_ms: Option<u32>,
    pub status: ServerStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Online,
    Testing,
    Offline,
    Unknown,
}

/// UI actions that can be triggered from the renderer
#[derive(Debug, Clone)]
pub enum UiAction {
    /// Start OAuth login flow
    StartLogin,
    /// Select a login provider
    SelectProvider(usize),
    /// Logout
    Logout,
    /// Launch a game by index
    LaunchGame(usize),
    /// Launch a specific game
    LaunchGameDirect(GameInfo),
    /// Stop streaming
    StopStreaming,
    /// Toggle stats overlay
    ToggleStats,
    /// Update search query
    UpdateSearch(String),
    /// Toggle settings panel
    ToggleSettings,
    /// Update a setting
    UpdateSetting(SettingChange),
    /// Refresh games list
    RefreshGames,
    /// Switch to a tab
    SwitchTab(GamesTab),
    /// Open game detail popup
    OpenGamePopup(GameInfo),
    /// Close game detail popup
    CloseGamePopup,
    /// Select a platform variant for the current game popup
    SelectVariant(usize),
    /// Select a server/region
    SelectServer(usize),
    /// Enable auto server selection (best ping)
    SetAutoServerSelection(bool),
    /// Start ping test for all servers
    StartPingTest,
    /// Toggle settings modal
    ToggleSettingsModal,
    /// Resume an active session
    ResumeSession(super::session::ActiveSessionInfo),
    /// Terminate existing session and start new game
    TerminateAndLaunch(String, GameInfo),
    /// Close session conflict dialog
    CloseSessionConflict,
    /// Close AV1 warning dialog
    CloseAV1Warning,
    /// Close Alliance experimental warning dialog
    CloseAllianceWarning,
    /// Close welcome popup
    CloseWelcomePopup,
    /// Reset all settings to defaults
    ResetSettings,
    /// Set queue sort mode
    SetQueueSortMode(QueueSortMode),
    /// Set queue region filter
    SetQueueRegionFilter(QueueRegionFilter),
    /// Show server selection modal (for free tier users)
    ShowServerSelection(GameInfo),
    /// Close server selection modal
    CloseServerSelection,
    /// Select a queue server for launching
    SelectQueueServer(Option<String>),
    /// Launch game with selected queue server
    LaunchWithServer(GameInfo, Option<String>),
    /// Refresh queue times
    RefreshQueueTimes,
    /// Update window size (width, height) - saved to settings
    UpdateWindowSize(u32, u32),
    // ZNow actions
    /// Refresh ZNow apps list
    RefreshZNowApps,
    /// Select a ZNow app to install/launch
    SelectZNowApp(ZNowApp),
    /// Launch ZNow session (start GFN with placeholder game)
    LaunchZNowSession(ZNowApp),
    /// Install app via ZNow
    ZNowInstallApp(String),
    /// Launch app via ZNow
    ZNowLaunchApp(String),
    /// Connect to ZNow relay server
    ZNowConnect,
    /// Disconnect from ZNow relay
    ZNowDisconnect,
    /// ZNow pairing complete (QR detected)
    ZNowPaired(String),
    /// ZNow status update from relay
    ZNowStatusUpdate(ZNowConnectionState),
    // File transfer actions
    /// File dropped on window (path)
    FileDropped(std::path::PathBuf),
    /// Cancel a file transfer
    CancelFileTransfer(String),
    /// Dismiss completed/failed transfer notification
    DismissFileTransfer(String),
}

/// Setting changes
#[derive(Debug, Clone)]
pub enum SettingChange {
    Resolution(String),
    Fps(u32),
    Codec(VideoCodec),
    MaxBitrate(u32),
    Fullscreen(bool),
    VSync(bool),
    LowLatency(bool),
    DecoderBackend(VideoDecoderBackend),
    ColorQuality(ColorQuality),
    Hdr(bool),
    ClipboardPasteEnabled(bool),
}

/// Application state enum
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    /// Login screen
    Login,
    /// Browsing games library
    Games,
    /// Session being set up (queue, launching)
    Session,
    /// Active streaming
    Streaming,
}

// ============================================================================
// ZNow Types
// ============================================================================

/// ZNow portable app information
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ZNowApp {
    #[serde(rename = "_id")]
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "iconUrl")]
    pub icon_url: String,
    pub category: String,
    #[serde(rename = "gameId")]
    pub game_id: String, // GFN game ID to launch
    #[serde(rename = "portableAppUrl")]
    pub portable_app_url: String,
    #[serde(rename = "exePath", default)]
    pub exe_path: String,
    pub version: String,
    #[serde(default)]
    pub size: Option<String>,
}

/// ZNow connection state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZNowConnectionState {
    /// Not connected to relay server
    Disconnected,
    /// Connecting to relay server
    Connecting,
    /// Connected to relay, waiting for GFN session to start
    WaitingForSession,
    /// GFN session started, scanning for QR code
    WaitingForQR,
    /// QR code detected, pairing with znow-runner
    Pairing,
    /// Paired and ready to send commands
    Connected,
    /// Installing an app
    Installing { app_name: String, progress: u8 },
    /// Launching an app
    Launching { app_name: String },
    /// App is running
    Running { app_name: String },
    /// Error occurred
    Error(String),
}

impl Default for ZNowConnectionState {
    fn default() -> Self {
        ZNowConnectionState::Disconnected
    }
}

/// ZNow session information
#[derive(Debug, Clone, Default)]
pub struct ZNowSession {
    pub client_code: Option<String>,
    pub exe_code: Option<String>,
    pub state: ZNowConnectionState,
    pub selected_app: Option<ZNowApp>,
}

// ============================================================================
// File Transfer Types
// ============================================================================

/// File transfer state for drag & drop uploads
#[derive(Debug, Clone)]
pub struct FileTransfer {
    /// Unique transfer ID
    pub id: String,
    /// Original file name
    pub file_name: String,
    /// Total file size in bytes
    pub total_bytes: u64,
    /// Bytes transferred so far
    pub transferred_bytes: u64,
    /// Transfer state
    pub state: FileTransferState,
    /// Transfer speed in bytes per second
    pub speed_bps: u64,
    /// Start time for speed calculation
    pub start_time: std::time::Instant,
}

impl FileTransfer {
    pub fn new(id: String, file_name: String, total_bytes: u64) -> Self {
        Self {
            id,
            file_name,
            total_bytes,
            transferred_bytes: 0,
            state: FileTransferState::Pending,
            speed_bps: 0,
            start_time: std::time::Instant::now(),
        }
    }

    /// Get progress as percentage (0-100)
    pub fn progress_percent(&self) -> u8 {
        if self.total_bytes == 0 {
            return 100;
        }
        ((self.transferred_bytes as f64 / self.total_bytes as f64) * 100.0) as u8
    }

    /// Get formatted speed string (e.g., "5.2 MB/s")
    pub fn speed_string(&self) -> String {
        let mb_per_sec = self.speed_bps as f64 / (1024.0 * 1024.0);
        if mb_per_sec >= 1.0 {
            format!("{:.1} MB/s", mb_per_sec)
        } else {
            let kb_per_sec = self.speed_bps as f64 / 1024.0;
            format!("{:.0} KB/s", kb_per_sec)
        }
    }

    /// Get formatted file size
    pub fn size_string(&self) -> String {
        format_bytes(self.total_bytes)
    }
}

/// File transfer state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTransferState {
    /// Waiting to start
    Pending,
    /// Currently uploading
    Uploading,
    /// Upload complete
    Complete,
    /// Upload failed
    Failed(String),
}

/// Format bytes to human readable string
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}
