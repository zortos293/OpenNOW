//! Application State Management
//!
//! Central state machine for the OpenNow Streamer.

pub mod cache;
pub mod config;
pub mod session;
pub mod types;

pub use config::{AudioCodec, ColorQuality, Settings, StatsPosition, StreamQuality, VideoCodec};
pub use session::{ActiveSessionInfo, SessionInfo, SessionState};
pub use types::{
    parse_resolution, AppState, GameInfo, GameSection, GameVariant, GamesTab, ServerInfo,
    ServerStatus, SettingChange, SharedFrame, SubscriptionInfo, UiAction,
};

use log::{error, info, warn};
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::api::{self, DynamicServerRegion, GfnApiClient};
use crate::auth::{self, AuthTokens, LoginProvider, PkceChallenge, UserInfo};

use crate::input::InputHandler;

use crate::media::StreamStats;
use crate::webrtc::StreamingSession;

/// Cache for dynamic regions fetched from serverInfo API
static DYNAMIC_REGIONS_CACHE: RwLock<Option<Vec<DynamicServerRegion>>> = RwLock::new(None);

/// Main application structure
pub struct App {
    /// Current application state
    pub state: AppState,

    /// Tokio runtime handle for async operations
    pub runtime: Handle,

    /// User settings
    pub settings: Settings,

    /// Authentication tokens
    pub auth_tokens: Option<AuthTokens>,

    /// User info
    pub user_info: Option<UserInfo>,

    /// Current session info
    pub session: Option<SessionInfo>,

    /// Streaming session (WebRTC)
    pub streaming_session: Option<Arc<Mutex<StreamingSession>>>,

    /// Input handler for the current platform
    pub input_handler: Option<Arc<InputHandler>>,

    /// Whether cursor is captured
    pub cursor_captured: bool,

    /// Current video frame (for rendering)
    pub current_frame: Option<VideoFrame>,

    /// Shared frame holder for zero-latency frame delivery
    pub shared_frame: Option<Arc<SharedFrame>>,

    /// Stream statistics
    pub stats: StreamStats,

    /// Whether to show stats overlay
    pub show_stats: bool,

    /// Status message for UI
    pub status_message: String,

    /// Error message (if any)
    pub error_message: Option<String>,

    /// Games list (flat, for All Games tab)
    pub games: Vec<GameInfo>,

    /// Game sections (Home tab - Trending, Free to Play, etc.)
    pub game_sections: Vec<GameSection>,

    /// Search query
    pub search_query: String,

    /// Selected game
    pub selected_game: Option<GameInfo>,

    /// Channel for receiving stats updates
    stats_rx: Option<mpsc::Receiver<StreamStats>>,

    // === Login State ===
    /// Available login providers
    pub login_providers: Vec<LoginProvider>,

    /// Selected provider index
    pub selected_provider_index: usize,

    /// Whether settings panel is visible
    pub show_settings: bool,

    /// Loading state for async operations
    pub is_loading: bool,

    /// VPC ID for current provider
    pub vpc_id: Option<String>,

    /// API client
    api_client: GfnApiClient,

    /// Subscription info (hours, storage, etc.)
    pub subscription: Option<SubscriptionInfo>,

    /// User's library games
    pub library_games: Vec<GameInfo>,

    /// Current tab in Games view
    pub current_tab: GamesTab,

    /// Selected game for detail popup (None = popup closed)
    pub selected_game_popup: Option<GameInfo>,

    /// Available servers/regions
    pub servers: Vec<ServerInfo>,

    /// Selected server index
    pub selected_server_index: usize,

    /// Auto server selection (picks best ping)
    pub auto_server_selection: bool,

    /// Whether ping test is running
    pub ping_testing: bool,

    /// Whether settings modal is visible
    pub show_settings_modal: bool,

    /// Active sessions detected
    pub active_sessions: Vec<ActiveSessionInfo>,

    /// Whether showing session conflict dialog
    pub show_session_conflict: bool,

    /// Whether showing AV1 unsupported warning dialog
    pub show_av1_warning: bool,

    /// Whether showing Alliance experimental warning dialog
    pub show_alliance_warning: bool,

    /// Pending game launch (waiting for session conflict resolution)
    pub pending_game_launch: Option<GameInfo>,

    /// Last time we polled the session (for rate limiting)
    last_poll_time: std::time::Instant,

    /// Render FPS tracking
    render_frame_count: u64,
    last_render_fps_time: std::time::Instant,
    last_render_frame_count: u64,

    /// Number of times we've polled after session became ready (to ensure candidates)
    session_ready_poll_count: u32,

    /// Anti-AFK mode enabled (Ctrl+Shift+F10 to toggle)
    pub anti_afk_enabled: bool,

    /// Last time anti-AFK sent a key press
    anti_afk_last_send: std::time::Instant,

    /// Whether a token refresh is currently in progress
    token_refresh_in_progress: bool,
}

/// Poll interval for session status (2 seconds)
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

// Mutex re-export for streaming session
use parking_lot::Mutex;

// VideoFrame re-import for current_frame field
use crate::media::VideoFrame;

impl App {
    /// Create new application instance
    pub fn new(runtime: Handle) -> Self {
        // Load settings
        let settings = Settings::load().unwrap_or_default();
        let auto_server = settings.auto_server_selection; // Save before move

        // Try to load saved tokens
        let auth_tokens = cache::load_tokens();
        let has_token = auth_tokens
            .as_ref()
            .map(|t| !t.is_expired())
            .unwrap_or(false);

        // Load cached login provider (for Alliance persistence)
        if let Some(provider) = cache::load_login_provider() {
            auth::set_login_provider(provider);
        }

        let initial_state = if has_token {
            AppState::Games
        } else {
            AppState::Login
        };

        // Start fetching login providers
        let rt = runtime.clone();
        rt.spawn(async {
            if let Err(e) = auth::fetch_login_providers().await {
                warn!("Failed to fetch login providers: {}", e);
            }
        });

        // Start checking active sessions if we have a token
        if has_token {
            let rt = runtime.clone();
            let token = auth_tokens.as_ref().unwrap().jwt().to_string();
            rt.spawn(async move {
                let mut api_client = GfnApiClient::new();
                api_client.set_access_token(token);
                match api_client.get_active_sessions().await {
                    Ok(sessions) => {
                        info!(
                            "Checked active sessions at startup: found {}",
                            sessions.len()
                        );
                        cache::save_active_sessions_cache(&sessions);
                    }
                    Err(e) => {
                        warn!("Failed to check active sessions at startup: {}", e);
                    }
                }
            });

            // Also fetch subscription info to ensure dynamic resolutions are available
            let rt = runtime.clone();
            let token = auth_tokens.as_ref().unwrap().jwt().to_string();
            let user_id = auth_tokens.as_ref().unwrap().user_id().to_string();
            rt.spawn(async move {
                match crate::api::fetch_subscription(&token, &user_id).await {
                    Ok(sub) => {
                        info!("Fetched subscription startup: tier={}", sub.membership_tier);
                        cache::save_subscription_cache(&sub);
                    }
                    Err(e) => {
                        warn!("Failed to fetch subscription at startup: {}", e);
                    }
                }
            });
        }

        Self {
            state: initial_state,
            runtime,
            settings,
            auth_tokens,
            user_info: None,
            session: None,
            streaming_session: None,
            input_handler: None,
            cursor_captured: false,
            current_frame: None,
            shared_frame: None,
            stats: StreamStats::default(),
            show_stats: true,
            status_message: "Welcome to OpenNOW".to_string(),
            error_message: None,
            games: Vec::new(),
            game_sections: Vec::new(),
            search_query: String::new(),
            selected_game: None,
            stats_rx: None,
            login_providers: vec![LoginProvider::nvidia_default()],
            selected_provider_index: 0,
            show_settings: false,
            is_loading: false,
            vpc_id: None,
            api_client: GfnApiClient::new(),
            subscription: None,
            library_games: Vec::new(),
            current_tab: GamesTab::Home,
            selected_game_popup: None,
            servers: Vec::new(),
            selected_server_index: 0,
            auto_server_selection: auto_server, // Load from settings
            ping_testing: false,
            show_settings_modal: false,
            active_sessions: Vec::new(),
            show_session_conflict: false,
            show_av1_warning: false,
            show_alliance_warning: false,

            pending_game_launch: None,
            last_poll_time: std::time::Instant::now(),
            render_frame_count: 0,
            last_render_fps_time: std::time::Instant::now(),
            last_render_frame_count: 0,
            session_ready_poll_count: 0,
            anti_afk_enabled: false,
            anti_afk_last_send: std::time::Instant::now(),
            token_refresh_in_progress: false,
        }
    }

    /// Toggle anti-AFK mode
    pub fn toggle_anti_afk(&mut self) {
        self.anti_afk_enabled = !self.anti_afk_enabled;
        if self.anti_afk_enabled {
            self.anti_afk_last_send = std::time::Instant::now();
            info!("Anti-AFK mode ENABLED - sending F13 every 4 minutes");
        } else {
            info!("Anti-AFK mode DISABLED");
        }
    }

    /// Send anti-AFK key press (F13) if enabled and interval elapsed
    pub fn update_anti_afk(&mut self) {
        if !self.anti_afk_enabled || self.state != AppState::Streaming {
            return;
        }

        // Check if 4 minutes have passed
        if self.anti_afk_last_send.elapsed() >= std::time::Duration::from_secs(240) {
            if let Some(ref input_handler) = self.input_handler {
                // F13 virtual key code is 0x7C (124)
                const VK_F13: u16 = 0x7C;

                // Send key down then key up
                input_handler.handle_key(VK_F13, true, 0); // Key down
                input_handler.handle_key(VK_F13, false, 0); // Key up

                self.anti_afk_last_send = std::time::Instant::now();
                log::debug!("Anti-AFK: sent F13 key press");
            }
        }
    }

    /// Handle a UI action
    pub fn handle_action(&mut self, action: UiAction) {
        match action {
            UiAction::StartLogin => {
                self.start_oauth_login();
            }
            UiAction::SelectProvider(index) => {
                self.select_provider(index);
            }
            UiAction::Logout => {
                self.logout();
            }
            UiAction::LaunchGame(index) => {
                // Get game from appropriate list based on current tab
                let game = match self.current_tab {
                    GamesTab::Home => self.games.get(index).cloned(), // Use flat list for Home too
                    GamesTab::AllGames => self.games.get(index).cloned(),
                    GamesTab::MyLibrary => self.library_games.get(index).cloned(),
                };
                if let Some(game) = game {
                    self.launch_game(&game);
                }
            }
            UiAction::LaunchGameDirect(game) => {
                self.launch_game(&game);
            }
            UiAction::StopStreaming => {
                self.stop_streaming();
            }
            UiAction::ToggleStats => {
                self.toggle_stats();
            }
            UiAction::UpdateSearch(query) => {
                self.search_query = query;
            }
            UiAction::ToggleSettings => {
                self.show_settings = !self.show_settings;
            }
            UiAction::UpdateSetting(change) => {
                match change {
                    SettingChange::Resolution(res) => self.settings.resolution = res,
                    SettingChange::Fps(fps) => self.settings.fps = fps,
                    SettingChange::Codec(codec) => {
                        // Check if AV1 is supported before enabling it
                        if codec == VideoCodec::AV1 && !crate::media::is_av1_hardware_supported() {
                            self.show_av1_warning = true;
                        }
                        // Still set the codec - user can use software decode if they want
                        self.settings.codec = codec;
                    }
                    SettingChange::MaxBitrate(bitrate) => self.settings.max_bitrate_mbps = bitrate,
                    SettingChange::Fullscreen(fs) => self.settings.fullscreen = fs,
                    SettingChange::VSync(vsync) => self.settings.vsync = vsync,
                    SettingChange::LowLatency(ll) => self.settings.low_latency_mode = ll,
                    SettingChange::DecoderBackend(backend) => {
                        self.settings.decoder_backend = backend
                    }
                    SettingChange::ColorQuality(quality) => {
                        self.settings.color_quality = quality;
                        // Auto-switch codec based on color quality requirements
                        if quality.requires_hevc() && self.settings.codec == VideoCodec::H264 {
                            // 10-bit or 4:4:4 requires HEVC or AV1
                            self.settings.codec = VideoCodec::H265;
                        }
                    }
                    SettingChange::Hdr(enabled) => {
                        self.settings.hdr_enabled = enabled;
                        // HDR requires 10-bit and HEVC/AV1
                        if enabled {
                            // Switch to 10-bit if currently 8-bit
                            if !self.settings.color_quality.is_10bit() {
                                self.settings.color_quality = ColorQuality::Bit10Yuv420;
                            }
                            // Switch to HEVC if currently H.264
                            if self.settings.codec == VideoCodec::H264 {
                                self.settings.codec = VideoCodec::H265;
                            }
                        }
                    }
                }
                self.save_settings();
            }
            UiAction::RefreshGames => {
                self.fetch_games();
            }
            UiAction::SwitchTab(tab) => {
                self.current_tab = tab;
                // Fetch library if switching to My Library and it's empty
                if tab == GamesTab::MyLibrary && self.library_games.is_empty() {
                    self.fetch_library();
                }
                // Fetch sections if switching to Home and sections are empty
                if tab == GamesTab::Home && self.game_sections.is_empty() {
                    self.fetch_sections();
                }
            }
            UiAction::OpenGamePopup(game) => {
                self.selected_game_popup = Some(game.clone());

                // Spawn async task to fetch full details (Play Type, Membership, etc.) only if missing
                // User reports library games already have this info, so avoid redundant 400-prone fetches
                let mut needs_fetch = game.play_type.is_none();

                // If we have a description, we definitely don't need to fetch
                if game.description.is_some() {
                    needs_fetch = false;
                }

                let token = self.auth_tokens.as_ref().map(|t| t.jwt().to_string());
                let query_id = game.id.clone();
                let runtime = self.runtime.clone();

                if needs_fetch {
                    if let Some(token) = token {
                        runtime.spawn(async move {
                            let mut api_client = GfnApiClient::new();
                            api_client.set_access_token(token);

                            // Fetch details
                            match api_client.fetch_app_details(&query_id).await {
                                Ok(Some(details)) => {
                                    info!("Fetched details for popup: {}", details.title);
                                    cache::save_popup_game_details(&details);
                                }
                                Ok(None) => warn!("No details found for popup game: {}", query_id),
                                Err(e) => warn!("Failed to fetch popup details: {}", e),
                            }
                        });
                    }
                } else {
                    info!("Using existing details for popup: {}", game.title);
                }
            }
            UiAction::CloseGamePopup => {
                self.selected_game_popup = None;
            }
            UiAction::SelectVariant(index) => {
                // Update the selected variant for the game popup
                if let Some(ref mut game) = self.selected_game_popup {
                    if index < game.variants.len() {
                        game.selected_variant_index = index;
                        // Update the game's store and id to match the selected variant
                        if let Some(variant) = game.variants.get(index) {
                            game.store = variant.store.clone();
                            game.id = variant.id.clone();
                            game.app_id = variant.id.parse::<i64>().ok();
                            info!(
                                "Selected platform variant: {} ({})",
                                variant.store, variant.id
                            );
                        }
                    }
                }
            }
            UiAction::SelectServer(index) => {
                if index < self.servers.len() {
                    self.selected_server_index = index;
                    self.auto_server_selection = false; // Disable auto when manually selecting
                                                        // Save selected server and auto mode to settings
                    self.settings.selected_server = Some(self.servers[index].id.clone());
                    self.settings.auto_server_selection = false;
                    self.save_settings();
                    info!("Selected server: {}", self.servers[index].name);
                }
            }
            UiAction::SetAutoServerSelection(enabled) => {
                self.auto_server_selection = enabled;
                self.settings.auto_server_selection = enabled;
                self.save_settings();
                if enabled {
                    // Auto-select best server based on ping
                    self.select_best_server();
                }
            }
            UiAction::StartPingTest => {
                self.start_ping_test();
            }
            UiAction::ToggleSettingsModal => {
                self.show_settings_modal = !self.show_settings_modal;
                // Load servers when opening settings if not loaded
                if self.show_settings_modal && self.servers.is_empty() {
                    self.load_servers();
                }
            }
            UiAction::ResumeSession(session_info) => {
                self.resume_session(session_info);
            }
            UiAction::TerminateAndLaunch(session_id, game) => {
                self.terminate_and_launch(session_id, game);
            }
            UiAction::CloseSessionConflict => {
                self.show_session_conflict = false;
                self.pending_game_launch = None;
            }
            UiAction::CloseAV1Warning => {
                self.show_av1_warning = false;
            }
            UiAction::CloseAllianceWarning => {
                self.show_alliance_warning = false;
            }
            UiAction::ResetSettings => {
                info!("Resetting all settings to defaults");
                self.settings = Settings::default();
                if let Err(e) = self.settings.save() {
                    warn!("Failed to save default settings: {}", e);
                }
            }
        }
    }

    /// Get filtered games based on search query
    pub fn filtered_games(&self) -> Vec<(usize, &GameInfo)> {
        let query = self.search_query.to_lowercase();
        self.games
            .iter()
            .enumerate()
            .filter(|(_, game)| query.is_empty() || game.title.to_lowercase().contains(&query))
            .collect()
    }

    /// Select a login provider
    pub fn select_provider(&mut self, index: usize) {
        // Update cached providers from global state
        self.login_providers = auth::get_cached_providers();
        if self.login_providers.is_empty() {
            self.login_providers = vec![LoginProvider::nvidia_default()];
        }

        if index < self.login_providers.len() {
            self.selected_provider_index = index;
            let provider = self.login_providers[index].clone();
            auth::set_login_provider(provider.clone());
            info!(
                "Selected provider: {}",
                provider.login_provider_display_name
            );
        }
    }

    /// Start OAuth login flow
    pub fn start_oauth_login(&mut self) {
        // Find available port
        let port = match auth::find_available_port() {
            Some(p) => p,
            None => {
                self.error_message = Some("No available ports for OAuth callback".to_string());
                return;
            }
        };

        self.is_loading = true;
        self.status_message = "Opening browser for login...".to_string();

        // Clear old caches when switching accounts
        self.subscription = None;
        self.games.clear();
        self.game_sections.clear();
        self.library_games.clear();
        cache::clear_games_cache();

        let pkce = PkceChallenge::new();
        let auth_url = auth::build_auth_url(&pkce, port);
        let verifier = pkce.verifier.clone();

        // Open browser
        if let Err(e) = open::that(&auth_url) {
            self.error_message = Some(format!("Failed to open browser: {}", e));
            self.is_loading = false;
            return;
        }

        info!("Opened browser for OAuth login");

        // Spawn task to wait for callback
        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            match auth::start_callback_server(port).await {
                Ok(code) => {
                    info!("Received OAuth code");
                    match auth::exchange_code(&code, &verifier, port).await {
                        Ok(tokens) => {
                            info!("Token exchange successful!");
                            // Tokens will be picked up in update()
                            // For now, we store them in a temp file
                            cache::save_tokens(&tokens);
                        }
                        Err(e) => {
                            error!("Token exchange failed: {}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("OAuth callback failed: {}", e);
                }
            }
        });
    }

    /// Update application state (called each frame)
    pub fn update(&mut self) {
        // Track render FPS
        self.render_frame_count += 1;
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_render_fps_time).as_secs_f64();
        if elapsed >= 1.0 {
            let frames_this_period = self.render_frame_count - self.last_render_frame_count;
            self.stats.render_fps = (frames_this_period as f64 / elapsed) as f32;
            self.stats.frames_rendered = self.render_frame_count;
            self.last_render_frame_count = self.render_frame_count;
            self.last_render_fps_time = now;
        }

        // Update anti-AFK (sends F13 every 4 minutes when enabled)
        self.update_anti_afk();

        // Proactive token refresh: refresh before expiration to avoid session interruption
        if !self.token_refresh_in_progress {
            if let Some(ref tokens) = self.auth_tokens {
                if tokens.should_refresh() && tokens.can_refresh() {
                    info!("Token nearing expiry, proactively refreshing...");
                    self.token_refresh_in_progress = true;

                    let refresh_token = tokens.refresh_token.clone().unwrap();
                    let runtime = self.runtime.clone();
                    runtime.spawn(async move {
                        match auth::refresh_token(&refresh_token).await {
                            Ok(new_tokens) => {
                                info!("Proactive token refresh successful!");
                                cache::save_tokens(&new_tokens);
                            }
                            Err(e) => {
                                warn!("Proactive token refresh failed: {}", e);
                            }
                        }
                    });
                }
            }
        }

        // Check for refreshed tokens from async refresh task
        if self.token_refresh_in_progress {
            if let Some(new_tokens) = cache::load_tokens() {
                if let Some(ref old_tokens) = self.auth_tokens {
                    // Check if tokens were actually refreshed (new expires_at)
                    if new_tokens.expires_at > old_tokens.expires_at {
                        info!("Loaded refreshed tokens");
                        self.auth_tokens = Some(new_tokens.clone());
                        self.api_client
                            .set_access_token(new_tokens.jwt().to_string());
                        self.token_refresh_in_progress = false;
                    }
                }
            }
        }

        // Check for new video frames from shared frame holder
        if let Some(ref shared) = self.shared_frame {
            if let Some(frame) = shared.read() {
                // Only log the first frame (when current_frame is None)
                if self.current_frame.is_none() {
                    log::info!(
                        "First video frame received: {}x{}",
                        frame.width,
                        frame.height
                    );
                }
                self.current_frame = Some(frame);
            }
        }

        // Check for stats updates
        if let Some(ref mut rx) = self.stats_rx {
            while let Ok(mut stats) = rx.try_recv() {
                // Preserve render_fps from our local tracking
                stats.render_fps = self.stats.render_fps;
                stats.frames_rendered = self.stats.frames_rendered;
                self.stats = stats;
            }
        }

        // Update cached providers
        let cached = auth::get_cached_providers();
        if !cached.is_empty() && cached.len() != self.login_providers.len() {
            self.login_providers = cached;
        }

        // Check if tokens were saved by OAuth callback
        if self.state == AppState::Login && self.is_loading {
            if let Some(tokens) = cache::load_tokens() {
                if !tokens.is_expired() {
                    info!("OAuth login successful!");
                    self.auth_tokens = Some(tokens.clone());
                    self.api_client.set_access_token(tokens.jwt().to_string());
                    self.is_loading = false;
                    self.state = AppState::Games;
                    self.status_message = "Login successful!".to_string();
                    self.fetch_games();
                    self.fetch_sections(); // Fetch sections for Home tab
                    self.fetch_subscription(); // Also fetch subscription info
                    self.load_servers(); // Load servers (fetches dynamic regions)

                    // Check for active sessions after login
                    self.check_active_sessions();

                    // Show Alliance experimental warning if using an Alliance partner
                    if auth::get_selected_provider().is_alliance_partner() {
                        self.show_alliance_warning = true;
                    }
                }
            }
        }

        // Check if games were fetched and saved to cache
        if self.state == AppState::Games && self.is_loading && self.games.is_empty() {
            if let Some(games) = cache::load_games_cache() {
                if !games.is_empty() {
                    // Check if cache has images - if not, it's old cache that needs refresh
                    let has_images = games.iter().any(|g| g.image_url.is_some());
                    if has_images {
                        info!("Loaded {} games from cache (with images)", games.len());
                        self.games = games;
                        self.is_loading = false;
                        self.status_message = format!("Loaded {} games", self.games.len());
                    } else {
                        info!(
                            "Cache has {} games but no images - forcing refresh",
                            games.len()
                        );
                        cache::clear_games_cache();
                        self.fetch_games();
                    }
                }
            }
        }

        // Check if library was fetched and saved to cache
        if self.state == AppState::Games
            && self.current_tab == GamesTab::MyLibrary
            && self.library_games.is_empty()
        {
            if let Some(games) = cache::load_library_cache() {
                if !games.is_empty() {
                    info!("Loaded {} games from library cache", games.len());
                    self.library_games = games;
                    self.is_loading = false;
                    self.status_message =
                        format!("Your Library: {} games", self.library_games.len());
                }
            }
        }

        // Check if sections were fetched and saved to cache (Home tab)
        if self.state == AppState::Games
            && self.current_tab == GamesTab::Home
            && self.game_sections.is_empty()
        {
            if let Some(sections) = cache::load_sections_cache() {
                if !sections.is_empty() {
                    info!("Loaded {} sections from cache", sections.len());
                    self.game_sections = sections;
                    self.is_loading = false;
                    self.status_message = format!("Loaded {} sections", self.game_sections.len());
                }
            }
        }

        // Check if subscription was fetched and saved to cache
        if self.state == AppState::Games && self.subscription.is_none() {
            if let Some(sub) = cache::load_subscription_cache() {
                info!("Loaded subscription from cache: {}", sub.membership_tier);
                self.subscription = Some(sub);
            }
        }

        // Check for dynamic regions from serverInfo API
        self.check_dynamic_regions();

        // Check for ping test results
        if self.ping_testing {
            self.load_ping_results();
        }

        // Check for active sessions from async check
        if let Some(sessions) = cache::load_active_sessions_cache() {
            self.active_sessions = sessions.clone();
            if let Some(pending) = cache::load_pending_game_cache() {
                self.pending_game_launch = Some(pending);
                self.show_session_conflict = true;
                cache::clear_active_sessions_cache();
            } else if !self.active_sessions.is_empty() {
                // Auto-resume logic: no pending game, but active sessions exist -> resume the first one
                if let Some(session) = self.active_sessions.first() {
                    info!("Auto-resuming active session found: {}", session.session_id);
                    let session_clone = session.clone();
                    self.resume_session(session_clone);
                    cache::clear_active_sessions_cache();
                }
            }
        }

        // Check for launch proceed flag (after session termination)
        if cache::check_launch_proceed_flag() {
            if let Some(game) = cache::load_pending_game_cache() {
                cache::clear_pending_game_cache();
                self.start_new_session(&game);
            }
        }

        // Poll session status when in session state
        if self.state == AppState::Session && self.is_loading {
            self.poll_session_status();
        }
    }

    /// Logout and return to login screen
    pub fn logout(&mut self) {
        self.auth_tokens = None;
        self.user_info = None;
        self.subscription = None;
        auth::clear_login_provider();
        cache::clear_login_provider(); // Clear persisted provider too
        cache::clear_tokens();
        cache::clear_games_cache(); // Clear cached games
        self.state = AppState::Login;
        self.games.clear();
        self.game_sections.clear();
        self.library_games.clear();
        self.status_message = "Logged out".to_string();
    }

    /// Fetch games library
    pub fn fetch_games(&mut self) {
        if self.auth_tokens.is_none() {
            return;
        }

        self.is_loading = true;
        self.status_message = "Loading games...".to_string();

        let token = self.auth_tokens.as_ref().unwrap().jwt().to_string();
        let mut api_client = GfnApiClient::new();
        api_client.set_access_token(token.clone());

        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            // Fetch games from GraphQL MAIN panel (has images)
            // This is the same approach as the official GFN client
            match api_client.fetch_main_games(None).await {
                Ok(games) => {
                    info!(
                        "Fetched {} games from GraphQL MAIN panel (with images)",
                        games.len()
                    );
                    cache::save_games_cache(&games);
                }
                Err(e) => {
                    error!("Failed to fetch main games from GraphQL: {}", e);

                    // Fallback: try public games list (no images, but has all games)
                    info!("Falling back to public games list...");
                    match api_client.fetch_public_games().await {
                        Ok(games) => {
                            info!("Fetched {} games from public list (fallback)", games.len());
                            cache::save_games_cache(&games);
                        }
                        Err(e2) => {
                            error!("Failed to fetch public games: {}", e2);
                        }
                    }
                }
            }
        });
    }

    /// Fetch user's library games
    pub fn fetch_library(&mut self) {
        if self.auth_tokens.is_none() {
            return;
        }

        self.is_loading = true;
        self.status_message = "Loading your library...".to_string();

        let token = self.auth_tokens.as_ref().unwrap().jwt().to_string();
        let mut api_client = GfnApiClient::new();
        api_client.set_access_token(token.clone());

        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            match api_client.fetch_library(None).await {
                Ok(games) => {
                    info!("Fetched {} games from LIBRARY panel", games.len());
                    cache::save_library_cache(&games);
                }
                Err(e) => {
                    error!("Failed to fetch library: {}", e);
                }
            }
        });
    }

    /// Fetch game sections for Home tab (Trending, Free to Play, etc.)
    pub fn fetch_sections(&mut self) {
        if self.auth_tokens.is_none() {
            return;
        }

        self.is_loading = true;
        self.status_message = "Loading sections...".to_string();

        let token = self.auth_tokens.as_ref().unwrap().jwt().to_string();
        let mut api_client = GfnApiClient::new();
        api_client.set_access_token(token.clone());

        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            match api_client.fetch_sectioned_games(None).await {
                Ok(sections) => {
                    info!("Fetched {} sections from GraphQL", sections.len());
                    cache::save_sections_cache(&sections);
                }
                Err(e) => {
                    error!("Failed to fetch sections: {}", e);
                }
            }
        });
    }

    /// Fetch subscription info (hours, addons, etc.)
    pub fn fetch_subscription(&mut self) {
        if self.auth_tokens.is_none() {
            return;
        }

        // Clear current subscription so update loop will reload from cache after fetch completes
        self.subscription = None;

        let token = self.auth_tokens.as_ref().unwrap().jwt().to_string();
        let user_id = self.auth_tokens.as_ref().unwrap().user_id().to_string();

        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            match crate::api::fetch_subscription(&token, &user_id).await {
                Ok(sub) => {
                    info!("Fetched subscription: tier={}, hours={:.1}/{:.1}, storage={}, unlimited={}",
                        sub.membership_tier,
                        sub.remaining_hours,
                        sub.total_hours,
                        sub.has_persistent_storage,
                        sub.is_unlimited
                    );
                    cache::save_subscription_cache(&sub);
                }
                Err(e) => {
                    warn!("Failed to fetch subscription: {}", e);
                }
            }
        });
    }

    /// Load available servers/regions (tries dynamic fetch first, falls back to hardcoded)
    pub fn load_servers(&mut self) {
        info!("Loading servers...");

        let runtime = self.runtime.clone();
        let token = self.auth_tokens.as_ref().map(|t| t.jwt().to_string());

        // Spawn async task to fetch dynamic regions
        runtime.spawn(async move {
            let client = reqwest::Client::new();
            let regions = api::fetch_dynamic_regions(&client, token.as_deref()).await;

            // Store the results for the main thread to pick up
            DYNAMIC_REGIONS_CACHE.write().replace(regions);
        });

        // For now, start with hardcoded servers (will update when dynamic fetch completes)
        self.load_hardcoded_servers();
    }

    /// Load hardcoded servers as fallback
    fn load_hardcoded_servers(&mut self) {
        let server_zones: Vec<(&str, &str, &str)> = vec![
            // Europe
            ("eu-netherlands-north", "Netherlands North", "Europe"),
            ("eu-netherlands-south", "Netherlands South", "Europe"),
            ("eu-united-kingdom-1", "United Kingdom", "Europe"),
            ("eu-germany-frankfurt-1", "Frankfurt", "Europe"),
            ("eu-france-paris-1", "Paris", "Europe"),
            ("eu-finland-helsinki-1", "Helsinki", "Europe"),
            ("eu-norway-oslo-1", "Oslo", "Europe"),
            ("eu-sweden-stockholm-1", "Stockholm", "Europe"),
            ("eu-poland-warsaw-1", "Warsaw", "Europe"),
            ("eu-italy-rome-1", "Rome", "Europe"),
            ("eu-spain-madrid-1", "Madrid", "Europe"),
            // North America
            ("us-california-north", "California North", "North America"),
            ("us-california-south", "California South", "North America"),
            ("us-texas-dallas-1", "Dallas", "North America"),
            ("us-virginia-north", "Virginia North", "North America"),
            ("us-illinois-chicago-1", "Chicago", "North America"),
            ("us-washington-seattle-1", "Seattle", "North America"),
            ("us-arizona-phoenix-1", "Phoenix", "North America"),
            // Canada
            ("ca-quebec", "Quebec", "Canada"),
            // Asia-Pacific
            ("ap-japan-tokyo-1", "Tokyo", "Asia-Pacific"),
            ("ap-japan-osaka-1", "Osaka", "Asia-Pacific"),
            ("ap-south-korea-seoul-1", "Seoul", "Asia-Pacific"),
            ("ap-australia-sydney-1", "Sydney", "Asia-Pacific"),
            ("ap-singapore-1", "Singapore", "Asia-Pacific"),
        ];

        self.servers = server_zones
            .iter()
            .map(|(id, name, region)| ServerInfo {
                id: id.to_string(),
                name: name.to_string(),
                region: region.to_string(),
                url: None,
                ping_ms: None,
                status: ServerStatus::Unknown,
            })
            .collect();

        // Restore selected server from settings
        if let Some(ref selected_id) = self.settings.selected_server {
            if let Some(idx) = self.servers.iter().position(|s| s.id == *selected_id) {
                self.selected_server_index = idx;
            }
        }

        info!("Loaded {} hardcoded servers", self.servers.len());
    }

    /// Update servers from dynamic region cache (call this periodically from update loop)
    pub fn check_dynamic_regions(&mut self) {
        let dynamic_regions = DYNAMIC_REGIONS_CACHE.write().take();

        if let Some(regions) = dynamic_regions {
            if !regions.is_empty() {
                info!("[serverInfo] Applying {} dynamic regions", regions.len());

                // Convert dynamic regions to ServerInfo
                // Group by region based on URL pattern
                self.servers = regions
                    .iter()
                    .map(|r| {
                        // Extract server ID from URL hostname
                        // e.g., "https://eu-netherlands-south.cloudmatchbeta.nvidiagrid.net" -> "eu-netherlands-south"
                        let hostname = r
                            .url
                            .trim_start_matches("https://")
                            .trim_start_matches("http://")
                            .split('.')
                            .next()
                            .unwrap_or(&r.name);

                        // Determine region from name or hostname
                        let region = if hostname.starts_with("eu-")
                            || r.name.contains("Europe")
                            || r.name.contains("UK")
                            || r.name.contains("France")
                            || r.name.contains("Germany")
                        {
                            "Europe"
                        } else if hostname.starts_with("us-")
                            || r.name.contains("US")
                            || r.name.contains("California")
                            || r.name.contains("Texas")
                        {
                            "North America"
                        } else if hostname.starts_with("ca-")
                            || r.name.contains("Canada")
                            || r.name.contains("Quebec")
                        {
                            "Canada"
                        } else if hostname.starts_with("ap-")
                            || r.name.contains("Japan")
                            || r.name.contains("Korea")
                            || r.name.contains("Singapore")
                        {
                            "Asia-Pacific"
                        } else {
                            "Other"
                        };

                        ServerInfo {
                            id: hostname.to_string(),
                            name: r.name.clone(),
                            region: region.to_string(),
                            url: Some(r.url.clone()),
                            ping_ms: None,
                            status: ServerStatus::Unknown,
                        }
                    })
                    .collect();

                // Restore selected server
                if let Some(ref selected_id) = self.settings.selected_server {
                    if let Some(idx) = self.servers.iter().position(|s| s.id == *selected_id) {
                        self.selected_server_index = idx;
                    }
                }

                info!("[serverInfo] Now have {} servers", self.servers.len());

                // Auto-start ping test after loading dynamic servers
                self.start_ping_test();
            }
        }
    }

    /// Check for active sessions explicitly
    pub fn check_active_sessions(&mut self) {
        if self.auth_tokens.is_none() {
            return;
        }

        let token = self.auth_tokens.as_ref().unwrap().jwt().to_string();
        let mut api_client = GfnApiClient::new();
        api_client.set_access_token(token);

        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            match api_client.get_active_sessions().await {
                Ok(sessions) => {
                    info!("Checked active sessions: found {}", sessions.len());
                    if !sessions.is_empty() {
                        cache::save_active_sessions_cache(&sessions);
                    }
                }
                Err(e) => {
                    warn!("Failed to check active sessions: {}", e);
                }
            }
        });
    }

    /// Start ping test for all servers
    pub fn start_ping_test(&mut self) {
        if self.ping_testing {
            return; // Already running
        }

        self.ping_testing = true;
        info!("Starting ping test for {} servers", self.servers.len());

        // Mark all servers as testing
        for server in &mut self.servers {
            server.status = ServerStatus::Testing;
            server.ping_ms = None;
        }

        // Collect server info with URLs for pinging
        let server_data: Vec<(String, Option<String>)> = self
            .servers
            .iter()
            .map(|s| (s.id.clone(), s.url.clone()))
            .collect();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let mut results: Vec<(String, Option<u32>, ServerStatus)> = Vec::new();

            for (server_id, url_opt) in server_data {
                // Extract hostname from URL or construct from server_id
                let hostname = if let Some(url) = url_opt {
                    url.trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .split('/')
                        .next()
                        .unwrap_or(&format!("{}.cloudmatchbeta.nvidiagrid.net", server_id))
                        .to_string()
                } else {
                    format!("{}.cloudmatchbeta.nvidiagrid.net", server_id)
                };

                // TCP ping to port 443
                let ping_result = Self::tcp_ping(&hostname, 443).await;

                let (ping_ms, status) = match ping_result {
                    Some(ms) => (Some(ms), ServerStatus::Online),
                    None => (None, ServerStatus::Offline),
                };

                results.push((server_id, ping_ms, status));
            }

            // Save results to cache
            cache::save_ping_results(&results);
        });
    }

    /// TCP ping to measure latency
    async fn tcp_ping(hostname: &str, port: u16) -> Option<u32> {
        use std::time::Instant;
        use tokio::net::TcpStream;
        use tokio::time::{timeout, Duration};

        // Resolve hostname first
        let addr = format!("{}:{}", hostname, port);

        let start = Instant::now();
        let result = timeout(Duration::from_secs(3), TcpStream::connect(&addr)).await;

        match result {
            Ok(Ok(_stream)) => {
                let elapsed = start.elapsed().as_millis() as u32;
                Some(elapsed)
            }
            _ => None,
        }
    }

    /// Load ping results from cache
    fn load_ping_results(&mut self) {
        if let Some(results) = cache::load_ping_results() {
            for result in results {
                if let Some(id) = result.get("id").and_then(|v| v.as_str()) {
                    if let Some(server) = self.servers.iter_mut().find(|s| s.id == id) {
                        server.ping_ms = result
                            .get("ping_ms")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32);
                        server.status = match result.get("status").and_then(|v| v.as_str()) {
                            Some("Online") => ServerStatus::Online,
                            Some("Offline") => ServerStatus::Offline,
                            _ => ServerStatus::Unknown,
                        };
                    }
                }
            }

            self.ping_testing = false;

            // Sort servers by ping (online first, then by ping)
            self.servers.sort_by(|a, b| match (&a.status, &b.status) {
                (ServerStatus::Online, ServerStatus::Online) => {
                    a.ping_ms.unwrap_or(9999).cmp(&b.ping_ms.unwrap_or(9999))
                }
                (ServerStatus::Online, _) => std::cmp::Ordering::Less,
                (_, ServerStatus::Online) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            });

            // Update selected index after sort
            if self.auto_server_selection {
                // Auto-select best server
                self.select_best_server();
            } else if let Some(ref selected_id) = self.settings.selected_server {
                if let Some(idx) = self.servers.iter().position(|s| s.id == *selected_id) {
                    self.selected_server_index = idx;
                }
            }
        }
    }

    /// Select the best server based on ping (lowest ping online server)
    fn select_best_server(&mut self) {
        // Find the server with the lowest ping that is online
        let best_server = self
            .servers
            .iter()
            .enumerate()
            .filter(|(_, s)| s.status == ServerStatus::Online && s.ping_ms.is_some())
            .min_by_key(|(_, s)| s.ping_ms.unwrap_or(9999));

        if let Some((idx, server)) = best_server {
            self.selected_server_index = idx;
            info!(
                "Auto-selected best server: {} ({}ms)",
                server.name,
                server.ping_ms.unwrap_or(0)
            );
        }
    }

    /// Launch a game session
    pub fn launch_game(&mut self, game: &GameInfo) {
        info!("Launching game: {} (ID: {})", game.title, game.id);

        // Get token first
        let token = match &self.auth_tokens {
            Some(t) => t.jwt().to_string(),
            None => {
                self.error_message = Some("Not logged in".to_string());
                return;
            }
        };

        let game_clone = game.clone();

        let mut api_client = GfnApiClient::new();
        api_client.set_access_token(token);

        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            match api_client.get_active_sessions().await {
                Ok(sessions) => {
                    if !sessions.is_empty() {
                        info!("Found {} active session(s)", sessions.len());
                        cache::save_active_sessions_cache(&sessions);
                        cache::save_pending_game_cache(&game_clone);
                    } else {
                        info!("No active sessions, proceeding with launch");
                        cache::clear_active_sessions_cache();
                        cache::save_pending_game_cache(&game_clone);
                        cache::save_launch_proceed_flag();
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to check active sessions: {}, proceeding with launch",
                        e
                    );
                    cache::clear_active_sessions_cache();
                    cache::save_pending_game_cache(&game_clone);
                    cache::save_launch_proceed_flag();
                }
            }
        });
    }

    /// Start creating a new session (after checking for conflicts)
    fn start_new_session(&mut self, game: &GameInfo) {
        info!("Starting new session for: {}", game.title);

        cache::clear_session_cache();
        cache::clear_session_error();

        self.selected_game = Some(game.clone());
        self.state = AppState::Session;
        self.status_message = format!("Starting {}...", game.title);
        self.error_message = None;
        self.is_loading = true;
        self.last_poll_time = std::time::Instant::now() - POLL_INTERVAL;

        let token = match &self.auth_tokens {
            Some(t) => t.jwt().to_string(),
            None => {
                self.error_message = Some("Not logged in".to_string());
                self.is_loading = false;
                return;
            }
        };

        let app_id = game.id.clone();
        let game_title = game.title.clone();
        let settings = self.settings.clone();

        let zone = self
            .servers
            .get(self.selected_server_index)
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "eu-netherlands-south".to_string());

        let is_install_to_play = game.is_install_to_play;

        let mut api_client = GfnApiClient::new();
        api_client.set_access_token(token);

        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            // Fetch latest app details to check for playType="INSTALL_TO_PLAY"
            // This is critical for demos which require account_linked=false
            let mut account_linked = !is_install_to_play;

            match api_client.fetch_app_details(&app_id).await {
                Ok(Some(details)) => {
                    info!(
                        "Fetched fresh app details: is_install_to_play={}",
                        details.is_install_to_play
                    );
                    account_linked = !details.is_install_to_play;
                }
                Ok(None) => warn!(
                    "App details not found, using cached info: is_install_to_play={}",
                    is_install_to_play
                ),
                Err(e) => warn!("Failed to fetch app details ({}): {}", app_id, e),
            }

            info!(
                "Starting session for '{}' with account_linked: {}",
                game_title, account_linked
            );

            match api_client
                .create_session(&app_id, &game_title, &settings, &zone, account_linked)
                .await
            {
                Ok(session) => {
                    info!(
                        "Session created: {} (state: {:?})",
                        session.session_id, session.state
                    );
                    cache::save_session_cache(&session);
                }
                Err(e) => {
                    error!("Failed to create session: {}", e);
                    cache::save_session_error(&format!("Failed to create session: {}", e));
                }
            }
        });
    }

    /// Resume an existing session
    fn resume_session(&mut self, session_info: ActiveSessionInfo) {
        info!("Resuming session: {}", session_info.session_id);

        self.show_session_conflict = false;
        self.pending_game_launch = None;
        self.state = AppState::Session;
        self.status_message = "Resuming session...".to_string();
        self.error_message = None;
        self.is_loading = true;
        self.last_poll_time = std::time::Instant::now() - POLL_INTERVAL;

        let token = match &self.auth_tokens {
            Some(t) => t.jwt().to_string(),
            None => {
                self.error_message = Some("Not logged in".to_string());
                self.is_loading = false;
                return;
            }
        };

        let server_ip = match session_info.server_ip {
            Some(ip) => ip,
            None => {
                self.error_message = Some("Session has no server IP".to_string());
                self.is_loading = false;
                return;
            }
        };

        let app_id = session_info.app_id.to_string();
        let settings = self.settings.clone();

        let mut api_client = GfnApiClient::new();
        api_client.set_access_token(token);

        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            match api_client
                .claim_session(&session_info.session_id, &server_ip, &app_id, &settings)
                .await
            {
                Ok(session) => {
                    info!(
                        "Session claimed: {} (state: {:?})",
                        session.session_id, session.state
                    );
                    cache::save_session_cache(&session);
                }
                Err(e) => {
                    error!("Failed to claim session: {}", e);
                    cache::save_session_error(&format!("Failed to resume session: {}", e));
                }
            }
        });
    }

    /// Terminate existing session and start new game
    fn terminate_and_launch(&mut self, session_id: String, game: GameInfo) {
        info!(
            "Terminating session {} and launching {}",
            session_id, game.title
        );

        self.show_session_conflict = false;
        self.pending_game_launch = None;
        self.status_message = "Ending previous session...".to_string();

        let token = match &self.auth_tokens {
            Some(t) => t.jwt().to_string(),
            None => {
                self.error_message = Some("Not logged in".to_string());
                return;
            }
        };

        let mut api_client = GfnApiClient::new();
        api_client.set_access_token(token);

        let runtime = self.runtime.clone();
        let game_for_launch = game.clone();
        runtime.spawn(async move {
            match api_client.stop_session(&session_id, "", None).await {
                Ok(_) => {
                    info!("Session terminated, waiting before launching new session");
                    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                    cache::save_launch_proceed_flag();
                    cache::save_pending_game_cache(&game_for_launch);
                }
                Err(e) => {
                    warn!("Session termination failed ({}), proceeding anyway", e);
                    cache::save_launch_proceed_flag();
                    cache::save_pending_game_cache(&game_for_launch);
                }
            }
        });
    }

    /// Poll session state and update UI
    fn poll_session_status(&mut self) {
        // First check cache for state updates (from in-flight or completed requests)
        // First check cache for state updates (from in-flight or completed requests)
        if let Some(session) = cache::load_session_cache() {
            if session.state == SessionState::Ready {
                // User requested: "make it pull few times before connecting to it so you can get the candidates"
                // We delay streaming start until we've polled a few times in Ready state
                if self.session_ready_poll_count < 3 {
                    self.status_message = format!(
                        "Session ready, finalizing connection ({}/3)...",
                        self.session_ready_poll_count + 1
                    );
                    // Don't return, allow fall-through to polling logic
                } else {
                    info!(
                        "Session ready! GPU: {:?}, Server: {}",
                        session.gpu_type, session.server_ip
                    );

                    // Update status message
                    if let Some(gpu) = &session.gpu_type {
                        self.status_message = format!("Connecting to GPU: {}", gpu);
                    } else {
                        self.status_message =
                            format!("Connecting to server: {}", session.server_ip);
                    }

                    cache::clear_session_cache();
                    self.start_streaming(session);
                    return;
                }
            } else if let SessionState::InQueue { position, eta_secs } = session.state {
                self.status_message = format!("Queue position: {} (ETA: {}s)", position, eta_secs);
            } else if let SessionState::Error(ref msg) = session.state {
                self.error_message = Some(msg.clone());
                self.is_loading = false;
                cache::clear_session_cache();
                return;
            } else if session.state == SessionState::Connecting {
                self.status_message = "Connecting to server...".to_string();
            } else if session.state == SessionState::CleaningUp {
                self.status_message = "Cleaning up previous session...".to_string();
            } else if session.state == SessionState::WaitingForStorage {
                self.status_message = "Waiting for storage to be ready...".to_string();
            } else {
                self.status_message = "Setting up session...".to_string();
            }
        }

        // Rate limit polling - only poll every POLL_INTERVAL (2 seconds)
        let now = std::time::Instant::now();
        if now.duration_since(self.last_poll_time) < POLL_INTERVAL {
            return;
        }

        if let Some(session) = cache::load_session_cache() {
            let mut should_poll = matches!(
                session.state,
                SessionState::Requesting
                    | SessionState::Launching
                    | SessionState::Connecting
                    | SessionState::CleaningUp
                    | SessionState::WaitingForStorage
                    | SessionState::InQueue { .. }
            );

            // Also poll if Ready but count < 3
            if session.state == SessionState::Ready && self.session_ready_poll_count < 3 {
                should_poll = true;
                // Increment poll count here, as we are about to poll
                self.session_ready_poll_count += 1;
            }

            if should_poll {
                // Update timestamp to rate limit next poll
                self.last_poll_time = now;

                let token = match &self.auth_tokens {
                    Some(t) => t.jwt().to_string(),
                    None => return,
                };

                let session_id = session.session_id.clone();
                let zone = session.zone.clone();
                let server_ip = if session.server_ip.is_empty() {
                    None
                } else {
                    Some(session.server_ip.clone())
                };

                let mut api_client = GfnApiClient::new();
                api_client.set_access_token(token);

                let runtime = self.runtime.clone();
                runtime.spawn(async move {
                    match api_client
                        .poll_session(&session_id, &zone, server_ip.as_deref())
                        .await
                    {
                        Ok(updated_session) => {
                            info!("Session poll: state={:?}", updated_session.state);
                            cache::save_session_cache(&updated_session);
                        }
                        Err(e) => {
                            error!("Session poll failed: {}", e);
                        }
                    }
                });
            }
        }

        // Check for session errors
        if let Some(error) = cache::load_session_error() {
            self.error_message = Some(error);
            self.is_loading = false;
            cache::clear_session_error();
        }

        // Check for popup game details updates
        if let Some(detailed_game) = cache::load_popup_game_details() {
            // Only update if we still have the popup open for the same game
            if let Some(current_popup) = &self.selected_game_popup {
                if current_popup.id == detailed_game.id {
                    info!(
                        "Updating popup with detailed info for: {}",
                        detailed_game.title
                    );
                    self.selected_game_popup = Some(detailed_game);
                }
            }
        }
    }

    /// Start streaming once session is ready
    pub fn start_streaming(&mut self, session: SessionInfo) {
        info!("Starting streaming to {}", session.server_ip);
        info!("Session Info Debug: {:?}", session);

        self.session = Some(session.clone());
        self.state = AppState::Streaming;
        self.cursor_captured = true;
        self.is_loading = false;

        // Reset session ready poll count for this new session
        self.session_ready_poll_count = 0;

        // Initialize session timing for proper input timestamps
        // This must be called BEFORE any input events are sent
        crate::input::init_session_timing();

        // Set local cursor dimensions for instant visual feedback
        // Parse resolution from settings (e.g., "1920x1080" -> width, height)
        let (width, height) = parse_resolution(&self.settings.resolution);
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        crate::input::set_local_cursor_dimensions(width, height);

        // Reset coalescing state to ensure clean input state for new session
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        crate::input::reset_coalescing();

        info!(
            "Input system initialized: session timing + local cursor {}x{}",
            width, height
        );

        // Create shared frame holder for zero-latency frame delivery
        // No buffering - decoder writes latest frame, renderer reads it immediately
        let shared_frame = Arc::new(SharedFrame::new());
        self.shared_frame = Some(shared_frame.clone());

        // Stats channel (small buffer is fine for stats)
        let (stats_tx, stats_rx) = mpsc::channel(8);
        info!(
            "Using zero-latency shared frame delivery for {}fps",
            self.settings.fps
        );

        self.stats_rx = Some(stats_rx);

        // Create input handler with clean state
        let input_handler = Arc::new(InputHandler::new());
        self.input_handler = Some(input_handler.clone());

        self.status_message = "Connecting...".to_string();

        // Clone settings for the async task
        let settings = self.settings.clone();

        // Spawn the streaming task
        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            match crate::webrtc::run_streaming(
                session,
                settings,
                shared_frame,
                stats_tx,
                input_handler,
            )
            .await
            {
                Ok(()) => {
                    info!("Streaming ended normally");
                }
                Err(e) => {
                    error!("Streaming error: {}", e);
                }
            }
        });
    }

    /// Terminate current session via API and stop streaming
    pub fn terminate_current_session(&mut self) {
        if let Some(session) = &self.session {
            info!(
                "Ctrl+Shift+Q: Terminating active session: {}",
                session.session_id
            );

            let token = match &self.auth_tokens {
                Some(t) => t.jwt().to_string(),
                None => {
                    self.stop_streaming();
                    return;
                }
            };

            let session_id = session.session_id.clone();
            let zone = session.zone.clone();
            let server_ip = if session.server_ip.is_empty() {
                None
            } else {
                Some(session.server_ip.clone())
            };

            let mut api_client = GfnApiClient::new();
            api_client.set_access_token(token);

            let runtime = self.runtime.clone();
            runtime.spawn(async move {
                match api_client
                    .stop_session(&session_id, &zone, server_ip.as_deref())
                    .await
                {
                    Ok(_) => info!("Session {} terminated successfully", session_id),
                    Err(e) => warn!("Failed to stop session {}: {}", session_id, e),
                }
            });
        }

        self.stop_streaming();
    }

    /// Stop streaming and return to games
    pub fn stop_streaming(&mut self) {
        info!("Stopping streaming");

        // Clear session cache first to prevent stale session data
        cache::clear_session_cache();

        // Reset input timing for next session
        crate::input::reset_session_timing();

        // Reset input coalescing and local cursor state
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        crate::input::reset_coalescing();

        // Clear raw input sender to prevent stale events from being processed
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        crate::input::clear_raw_input_sender();

        self.cursor_captured = false;
        self.state = AppState::Games;
        self.streaming_session = None;
        self.session = None; // Clear session info
        self.input_handler = None;
        self.current_frame = None;
        self.shared_frame = None;
        self.stats_rx = None;
        self.selected_game = None;
        self.is_loading = false;
        self.error_message = None;

        // Reset session ready poll count for next session
        self.session_ready_poll_count = 0;

        self.status_message = "Stream ended".to_string();
    }

    /// Toggle stats overlay
    pub fn toggle_stats(&mut self) {
        self.show_stats = !self.show_stats;
    }

    /// Save settings
    pub fn save_settings(&self) {
        if let Err(e) = self.settings.save() {
            error!("Failed to save settings: {}", e);
        }
    }

    /// Get current user display name
    pub fn user_display_name(&self) -> &str {
        self.user_info
            .as_ref()
            .map(|u| u.display_name.as_str())
            .unwrap_or("User")
    }

    /// Get current membership tier
    pub fn membership_tier(&self) -> &str {
        self.user_info
            .as_ref()
            .map(|u| u.membership_tier.as_str())
            .unwrap_or("FREE")
    }
}
