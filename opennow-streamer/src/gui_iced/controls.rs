//! Iced UI Controls
//!
//! Main UI state and message handling for iced integration.

use iced_wgpu::Renderer;
use iced_widget::{
    button, column, container, image, row, scrollable, text, text_input, slider, Space,
    checkbox, pick_list, stack,
};
use iced_winit::core::{Alignment, Color, Element, Length, Padding, Theme, Font, Background, Gradient};
use super::icons;

/// Safely truncate a string to at most `max_chars` characters.
/// Returns a slice that respects UTF-8 char boundaries.
fn truncate_string(s: &str, max_chars: usize) -> &str {
    if s.chars().count() <= max_chars {
        s
    } else {
        // Find the byte index of the nth character
        s.char_indices()
            .nth(max_chars)
            .map(|(idx, _)| &s[..idx])
            .unwrap_or(s)
    }
}

use crate::app::{
    AspectRatio, AppState, GameInfo, GameSection, GamesTab, ServerInfo, Settings, SettingChange,
    SubscriptionInfo, UiAction,
};
use crate::auth::LoginProvider;
use crate::app::config::{
    FPS_OPTIONS, VideoCodec, VideoDecoderBackend, ColorQuality, GameLanguage,
};

/// Get platform-specific decoder options
fn get_platform_decoder_options() -> Vec<VideoDecoderBackend> {
    #[cfg(target_os = "windows")]
    {
        vec![
            VideoDecoderBackend::Auto,
            VideoDecoderBackend::Dxva,         // D3D11 via GStreamer
            VideoDecoderBackend::Cuvid,        // NVIDIA NVDEC
            VideoDecoderBackend::Qsv,          // Intel QuickSync
            VideoDecoderBackend::Software,
        ]
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            VideoDecoderBackend::Auto,
            VideoDecoderBackend::VideoToolbox,
            VideoDecoderBackend::Software,
        ]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            VideoDecoderBackend::Auto,
            VideoDecoderBackend::VulkanVideo,  // GStreamer HW
            VideoDecoderBackend::Vaapi,        // VA-API
            VideoDecoderBackend::Software,
        ]
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        vec![
            VideoDecoderBackend::Auto,
            VideoDecoderBackend::Software,
        ]
    }
}

/// Messages for the iced UI
#[derive(Debug, Clone)]
pub enum Message {
    // Login screen
    LoginWithNvidia,
    SelectProvider(usize),
    
    // Games screen
    SearchChanged(String),
    SearchSubmit,
    TabSelected(GamesTab),
    GameClicked(GameInfo),
    GameLaunch(GameInfo),
    GamePopupClose,
    VariantSelected(usize),
    
    // Settings
    OpenSettings,
    CloseSettings,
    BitrateChanged(f32),
    AspectRatioChanged(String),
    ResolutionChanged(String),
    FpsChanged(u32),
    CodecChanged(String),
    DecoderChanged(String),
    ColorQualityChanged(String),
    HdrChanged(bool),
    LowLatencyChanged(bool),
    BorderlessChanged(bool),
    FullscreenChanged(bool),
    VsyncChanged(bool),
    SurroundChanged(bool),
    ClipboardPasteChanged(bool),
    GameLanguageChanged(String),
    ServerSelected(usize),
    AutoServerChanged(bool),
    StartPingTest,
    ResetSettings,
    
    // Session
    CancelSession,
    
    // Server selection (free tier)
    OpenServerSelection,
    CloseServerSelection,
    QueueServerSelected(Option<String>),
    LaunchWithServer,
    
    // Dialogs
    CloseSessionConflict,
    ResumeSession,
    TerminateAndLaunch,
    CloseAV1Warning,
    CloseAllianceWarning,
    CloseWelcome,
    
    // General
    RefreshGames,
    RefreshQueueTimes,
    Logout,
    ToggleStats,
    
    // Internal
    Tick,
    ImageLoaded(String, Vec<u8>),
}

/// Controls state for the iced UI
pub struct Controls {
    // Login state
    pub login_error: Option<String>,
    pub selected_provider: usize,
    
    // Games state
    pub search_query: String,
    pub current_tab: GamesTab,
    pub selected_game_popup: Option<GameInfo>,
    pub show_settings: bool,
    
    // Server selection (free tier)
    pub show_server_selection: bool,
    pub pending_game: Option<GameInfo>,
    pub selected_queue_server: Option<String>,
    
    // Dialogs
    pub show_session_conflict: bool,
    pub show_av1_warning: bool,
    pub show_alliance_warning: bool,
    pub show_welcome: bool,
    
    // Image cache for game thumbnails
    pub loaded_images: std::collections::HashMap<String, iced_widget::image::Handle>,
    
    // Settings cache
    pub settings: Settings,
    pub bitrate_value: f32,
}

impl Default for Controls {
    fn default() -> Self {
        Self::new()
    }
}

impl Controls {
    pub fn new() -> Self {
        Self {
            login_error: None,
            selected_provider: 0,
            search_query: String::new(),
            current_tab: GamesTab::Home,
            selected_game_popup: None,
            show_settings: false,
            show_server_selection: false,
            pending_game: None,
            selected_queue_server: None,
            show_session_conflict: false,
            show_av1_warning: false,
            show_alliance_warning: false,
            show_welcome: false,
            loaded_images: std::collections::HashMap::new(),
            settings: Settings::default(),
            bitrate_value: 50.0,
        }
    }
    
    /// Sync state from app
    pub fn sync_from_app(
        &mut self,
        settings: &Settings,
        show_settings: bool,
        selected_game_popup: Option<&GameInfo>,
        show_session_conflict: bool,
        show_av1_warning: bool,
        show_alliance_warning: bool,
        show_welcome: bool,
    ) {
        self.settings = settings.clone();
        self.bitrate_value = settings.max_bitrate_mbps as f32;
        self.show_settings = show_settings;
        self.selected_game_popup = selected_game_popup.cloned();
        self.show_session_conflict = show_session_conflict;
        self.show_av1_warning = show_av1_warning;
        self.show_alliance_warning = show_alliance_warning;
        self.show_welcome = show_welcome;
    }
    
    /// Handle a message and return any UI actions needed
    pub fn update(&mut self, message: Message) -> Option<UiAction> {
        match message {
            // Login
            Message::LoginWithNvidia => {
                Some(UiAction::StartLogin)
            }
            Message::SelectProvider(idx) => {
                self.selected_provider = idx;
                Some(UiAction::SelectProvider(idx))
            }
            
            // Games
            Message::SearchChanged(query) => {
                self.search_query = query.clone();
                Some(UiAction::UpdateSearch(query))
            }
            Message::SearchSubmit => {
                Some(UiAction::UpdateSearch(self.search_query.clone()))
            }
            Message::TabSelected(tab) => {
                self.current_tab = tab;
                Some(UiAction::SwitchTab(tab))
            }
            Message::GameClicked(game) => {
                self.selected_game_popup = Some(game.clone());
                Some(UiAction::OpenGamePopup(game))
            }
            Message::GameLaunch(game) => {
                self.selected_game_popup = None;
                Some(UiAction::LaunchGameDirect(game))
            }
            Message::GamePopupClose => {
                self.selected_game_popup = None;
                Some(UiAction::CloseGamePopup)
            }
            Message::VariantSelected(idx) => {
                Some(UiAction::SelectVariant(idx))
            }
            
            // Settings
            Message::OpenSettings => {
                self.show_settings = true;
                Some(UiAction::ToggleSettingsModal)
            }
            Message::CloseSettings => {
                self.show_settings = false;
                Some(UiAction::ToggleSettingsModal)
            }
            Message::BitrateChanged(value) => {
                self.bitrate_value = value;
                self.settings.max_bitrate_mbps = value as u32;
                Some(UiAction::UpdateSetting(SettingChange::MaxBitrate(value as u32)))
            }
            Message::AspectRatioChanged(ratio_str) => {
                let ratio = AspectRatio::all()
                    .iter()
                    .find(|r| r.display_name() == ratio_str)
                    .copied()
                    .unwrap_or(AspectRatio::Ratio16x9);
                self.settings.aspect_ratio = ratio;
                // Also update resolution to first available for this aspect ratio
                if let Some((res, _)) = ratio.resolutions().first() {
                    self.settings.resolution = res.to_string();
                }
                Some(UiAction::UpdateSetting(SettingChange::AspectRatio(ratio)))
            }
            Message::ResolutionChanged(res) => {
                self.settings.resolution = res.clone();
                Some(UiAction::UpdateSetting(SettingChange::Resolution(res)))
            }
            Message::FpsChanged(fps) => {
                self.settings.fps = fps;
                Some(UiAction::UpdateSetting(SettingChange::Fps(fps)))
            }
            Message::CodecChanged(codec_str) => {
                let codec = match codec_str.as_str() {
                    "H265" => VideoCodec::H265,
                    "AV1" => VideoCodec::AV1,
                    _ => VideoCodec::H264,
                };
                self.settings.codec = codec;
                Some(UiAction::UpdateSetting(SettingChange::Codec(codec)))
            }
            Message::DecoderChanged(decoder_str) => {
                let decoder = VideoDecoderBackend::all()
                    .iter()
                    .find(|d| d.as_str() == decoder_str)
                    .copied()
                    .unwrap_or(VideoDecoderBackend::Auto);
                self.settings.decoder_backend = decoder;
                Some(UiAction::UpdateSetting(SettingChange::DecoderBackend(decoder)))
            }
            Message::ColorQualityChanged(color_str) => {
                let color = ColorQuality::all()
                    .iter()
                    .find(|c| c.display_name() == color_str)
                    .copied()
                    .unwrap_or(ColorQuality::Bit10Yuv420);
                self.settings.color_quality = color;
                Some(UiAction::UpdateSetting(SettingChange::ColorQuality(color)))
            }
            Message::HdrChanged(hdr) => {
                self.settings.hdr_enabled = hdr;
                Some(UiAction::UpdateSetting(SettingChange::Hdr(hdr)))
            }
            Message::LowLatencyChanged(low_latency) => {
                self.settings.low_latency_mode = low_latency;
                Some(UiAction::UpdateSetting(SettingChange::LowLatency(low_latency)))
            }
            Message::BorderlessChanged(borderless) => {
                self.settings.borderless = borderless;
                Some(UiAction::UpdateSetting(SettingChange::Borderless(borderless)))
            }
            Message::FullscreenChanged(fs) => {
                self.settings.fullscreen = fs;
                Some(UiAction::UpdateSetting(SettingChange::Fullscreen(fs)))
            }
            Message::VsyncChanged(vs) => {
                self.settings.vsync = vs;
                Some(UiAction::UpdateSetting(SettingChange::VSync(vs)))
            }
            Message::SurroundChanged(surround) => {
                self.settings.surround = surround;
                // No specific UiAction for surround yet, just update local settings
                None
            }
            Message::ClipboardPasteChanged(enabled) => {
                self.settings.clipboard_paste_enabled = enabled;
                Some(UiAction::UpdateSetting(SettingChange::ClipboardPasteEnabled(enabled)))
            }
            Message::GameLanguageChanged(lang_str) => {
                let lang = GameLanguage::all()
                    .iter()
                    .find(|l| l.display_name() == lang_str)
                    .copied()
                    .unwrap_or(GameLanguage::EnglishUS);
                self.settings.game_language = lang;
                Some(UiAction::UpdateSetting(SettingChange::GameLanguage(lang)))
            }
            Message::ServerSelected(idx) => {
                Some(UiAction::SelectServer(idx))
            }
            Message::AutoServerChanged(auto) => {
                Some(UiAction::SetAutoServerSelection(auto))
            }
            Message::StartPingTest => {
                Some(UiAction::StartPingTest)
            }
            Message::ResetSettings => {
                Some(UiAction::ResetSettings)
            }
            
            // Session
            Message::CancelSession => Some(UiAction::StopStreaming),
            
            // Server selection
            Message::OpenServerSelection => {
                self.show_server_selection = true;
                None
            }
            Message::CloseServerSelection => {
                self.show_server_selection = false;
                self.pending_game = None;
                Some(UiAction::CloseServerSelection)
            }
            Message::QueueServerSelected(server) => {
                self.selected_queue_server = server.clone();
                Some(UiAction::SelectQueueServer(server))
            }
            Message::LaunchWithServer => {
                if let Some(game) = self.pending_game.take() {
                    self.show_server_selection = false;
                    Some(UiAction::LaunchWithServer(game, self.selected_queue_server.clone()))
                } else {
                    None
                }
            }
            
            // Dialogs
            Message::CloseSessionConflict => {
                self.show_session_conflict = false;
                Some(UiAction::CloseSessionConflict)
            }
            Message::ResumeSession => {
                self.show_session_conflict = false;
                // TODO: Need session info here
                None
            }
            Message::TerminateAndLaunch => {
                self.show_session_conflict = false;
                // TODO: Need session info here
                None
            }
            Message::CloseAV1Warning => {
                self.show_av1_warning = false;
                Some(UiAction::CloseAV1Warning)
            }
            Message::CloseAllianceWarning => {
                self.show_alliance_warning = false;
                Some(UiAction::CloseAllianceWarning)
            }
            Message::CloseWelcome => {
                self.show_welcome = false;
                Some(UiAction::CloseWelcomePopup)
            }
            
            // General
            Message::RefreshGames => Some(UiAction::RefreshGames),
            Message::RefreshQueueTimes => Some(UiAction::RefreshQueueTimes),
            Message::Logout => Some(UiAction::Logout),
            Message::ToggleStats => Some(UiAction::ToggleStats),
            
            // Internal
            Message::Tick => None,
            Message::ImageLoaded(url, data) => {
                let handle = iced_widget::image::Handle::from_bytes(data);
                self.loaded_images.insert(url, handle);
                None
            }
        }
    }
    
    /// Build the main view
    pub fn view<'a>(
        &'a self,
        app_state: AppState,
        games: &'a [GameInfo],
        library_games: &'a [GameInfo],
        game_sections: &'a [GameSection],
        status_message: &'a str,
        user_name: Option<&'a str>,
        servers: &'a [ServerInfo],
        selected_server_index: usize,
        subscription: Option<&'a SubscriptionInfo>,
        login_providers: &'a [LoginProvider],
        selected_provider_index: usize,
    ) -> Element<'a, Message, Theme, Renderer> {
        let main_content: Element<'a, Message, Theme, Renderer> = match app_state {
            AppState::Login => self.view_login(status_message, login_providers, selected_provider_index),
            AppState::Games => self.view_games(games, library_games, game_sections, user_name, subscription, servers, selected_server_index),
            AppState::Session => self.view_session(status_message),
            AppState::Streaming => {
                // Minimal UI during streaming - just stats overlay if enabled
                container(Space::new().width(Length::Fill).height(Length::Fill))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
        };
        
        // Layer modals on top
        if self.show_settings {
            self.view_with_settings_modal(main_content, servers, selected_server_index, subscription)
        } else if let Some(ref game) = self.selected_game_popup {
            self.view_with_game_popup(main_content, game)
        } else {
            main_content
        }
    }
    
    fn view_login<'a>(
        &'a self,
        status_message: &'a str,
        login_providers: &'a [LoginProvider],
        selected_provider_index: usize,
    ) -> Element<'a, Message, Theme, Renderer> {
        let title = text("OpenNOW")
            .size(48)
            .color(Color::from_rgb(0.467, 0.784, 0.196)); // GFN green
        
        let subtitle = text("Open Source GeForce NOW Client")
            .size(16)
            .color(Color::from_rgb(0.7, 0.7, 0.7));
        
        // Provider selection dropdown
        let provider_names: Vec<String> = login_providers
            .iter()
            .map(|p| p.login_provider_display_name.clone())
            .collect();
        
        let selected_provider_name = login_providers
            .get(selected_provider_index)
            .map(|p| p.login_provider_display_name.clone());
        
        let provider_label = text("Alliance Partner")
            .size(14)
            .color(Color::from_rgb(0.7, 0.7, 0.7));
        
        let provider_picker: Element<'a, Message, Theme, Renderer> = if provider_names.len() > 1 {
            pick_list(
                provider_names,
                selected_provider_name,
                |name| {
                    // Find the index of the selected provider
                    Message::SelectProvider(
                        login_providers
                            .iter()
                            .position(|p| p.login_provider_display_name == name)
                            .unwrap_or(0)
                    )
                },
            )
            .width(250)
            .text_size(14)
            .padding(Padding::from([10, 16]))
            .style(|_theme: &Theme, _status| {
                pick_list::Style {
                    background: Color::from_rgb(0.137, 0.137, 0.176).into(),
                    text_color: Color::WHITE,
                    placeholder_color: Color::from_rgb(0.5, 0.5, 0.5),
                    handle_color: Color::from_rgb(0.467, 0.784, 0.196),
                    border: iced_core::Border::default()
                        .rounded(8)
                        .color(Color::from_rgb(0.235, 0.235, 0.294))
                        .width(1.0),
                }
            })
            .into()
        } else {
            // Show loading text while providers are being fetched
            text("Loading providers...")
                .size(14)
                .color(Color::from_rgb(0.5, 0.5, 0.5))
                .into()
        };
        
        // Get the button text based on selected provider
        let selected_provider = login_providers.get(selected_provider_index);
        let button_text = if let Some(provider) = selected_provider {
            if provider.is_alliance_partner() {
                format!("Sign in with {}", provider.login_provider_display_name)
            } else {
                "Sign in with NVIDIA".to_string()
            }
        } else {
            "Sign in with NVIDIA".to_string()
        };
        
        let sign_in_btn = button(
            container(text(button_text).size(16))
                .padding(Padding::from([12, 32]))
        )
        .style(|_theme: &Theme, _status| {
            button::Style {
                background: Some(Color::from_rgb(0.467, 0.784, 0.196).into()),
                text_color: Color::BLACK,
                border: iced_core::Border::default().rounded(8),
                ..button::Style::default()
            }
        })
        .on_press(Message::LoginWithNvidia);
        
        let status: Element<'a, Message, Theme, Renderer> = if !status_message.is_empty() {
            text(status_message)
                .size(14)
                .color(Color::from_rgb(0.7, 0.7, 0.7))
                .into()
        } else {
            Space::new().into()
        };
        
        let error: Element<'a, Message, Theme, Renderer> = if let Some(ref err) = self.login_error {
            text(err)
                .size(14)
                .color(Color::from_rgb(0.9, 0.3, 0.3))
                .into()
        } else {
            Space::new().into()
        };
        
        let content = column![
            Space::new().height(Length::FillPortion(1)),
            title,
            Space::new().height(8),
            subtitle,
            Space::new().height(48),
            provider_label,
            Space::new().height(8),
            provider_picker,
            Space::new().height(24),
            sign_in_btn,
            Space::new().height(32),
            status,
            error,
            Space::new().height(Length::FillPortion(1)),
        ]
        .align_x(Alignment::Center);
        
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_| container::Style {
                background: Some(Color::from_rgb(0.078, 0.078, 0.118).into()),
                ..container::Style::default()
            })
            .into()
    }
    
    fn view_games<'a>(
        &'a self,
        games: &'a [GameInfo],
        library_games: &'a [GameInfo],
        game_sections: &'a [GameSection],
        user_name: Option<&'a str>,
        subscription: Option<&'a SubscriptionInfo>,
        servers: &'a [ServerInfo],
        selected_server_index: usize,
    ) -> Element<'a, Message, Theme, Renderer> {
        // Combined header with tabs in single navbar
        let header = self.view_header_with_tabs(user_name);
        
        // Content based on tab - use correct games list for each tab
        // If search query is active, show filtered results from all games
        let content: Element<'a, Message, Theme, Renderer> = if !self.search_query.is_empty() {
            // Search is active - show filtered results from all games
            self.view_games_grid(games)
        } else {
            match self.current_tab {
                GamesTab::Home => self.view_home_sections(game_sections),
                GamesTab::AllGames => self.view_games_grid(games),
                GamesTab::MyLibrary => self.view_games_grid(library_games),
                GamesTab::QueueTimes => self.view_queue_times(),
            }
        };
        
        // Bottom bar with subscription info
        let bottom_bar = self.view_bottom_bar(subscription, servers, selected_server_index);
        
        let main = column![
            header,
            scrollable(content).height(Length::Fill),
            bottom_bar,
        ];
        
        container(main)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(Color::from_rgb(0.078, 0.078, 0.118).into()),
                ..container::Style::default()
            })
            .into()
    }
    
    fn view_header_with_tabs<'a>(
        &'a self,
        user_name: Option<&'a str>,
    ) -> Element<'a, Message, Theme, Renderer> {
        let logo = text("OpenNOW")
            .size(24)
            .color(Color::from_rgb(0.463, 0.725, 0.0)) // GFN green
            .font(iced_core::Font::with_name("Inter")); // Try to use a better font if available, or default
        
        // Tab buttons (in header)
        let active_bg = Color::from_rgb(0.463, 0.725, 0.0); // GFN green
        let inactive_bg = Color::TRANSPARENT; // Minimalist look
        let active_text = Color::WHITE;
        let inactive_text = Color::from_rgb(0.7, 0.7, 0.7);
        
        let home_selected = matches!(self.current_tab, GamesTab::Home);
        let all_selected = matches!(self.current_tab, GamesTab::AllGames);
        let library_selected = matches!(self.current_tab, GamesTab::MyLibrary);
        let queue_selected = matches!(self.current_tab, GamesTab::QueueTimes);
        
        let home_btn = button(text("Home").size(14))
            .padding(Padding::from([8, 16]))
            .on_press(Message::TabSelected(GamesTab::Home))
            .style(move |_, status| {
                let hovered = status == iced_widget::button::Status::Hovered;
                button::Style {
                    background: Some(if home_selected { active_bg } else if hovered { Color::from_rgba(1.0, 1.0, 1.0, 0.1) } else { inactive_bg }.into()),
                    text_color: if home_selected || hovered { active_text } else { inactive_text },
                    border: iced_core::Border::default().rounded(20),
                    ..button::Style::default()
                }
            });
        
        let all_btn = button(text("All Games").size(14))
            .padding(Padding::from([8, 16]))
            .on_press(Message::TabSelected(GamesTab::AllGames))
            .style(move |_, status| {
                let hovered = status == iced_widget::button::Status::Hovered;
                button::Style {
                    background: Some(if all_selected { active_bg } else if hovered { Color::from_rgba(1.0, 1.0, 1.0, 0.1) } else { inactive_bg }.into()),
                    text_color: if all_selected || hovered { active_text } else { inactive_text },
                    border: iced_core::Border::default().rounded(20),
                    ..button::Style::default()
                }
            });
        
        let library_btn = button(text("My Library").size(14))
            .padding(Padding::from([8, 16]))
            .on_press(Message::TabSelected(GamesTab::MyLibrary))
            .style(move |_, status| {
                let hovered = status == iced_widget::button::Status::Hovered;
                button::Style {
                    background: Some(if library_selected { active_bg } else if hovered { Color::from_rgba(1.0, 1.0, 1.0, 0.1) } else { inactive_bg }.into()),
                    text_color: if library_selected || hovered { active_text } else { inactive_text },
                    border: iced_core::Border::default().rounded(20),
                    ..button::Style::default()
                }
            });
        
        let queue_btn = button(text("Queue").size(14))
            .padding(Padding::from([8, 16]))
            .on_press(Message::TabSelected(GamesTab::QueueTimes))
            .style(move |_, status| {
                let hovered = status == iced_widget::button::Status::Hovered;
                button::Style {
                    background: Some(if queue_selected { active_bg } else if hovered { Color::from_rgba(1.0, 1.0, 1.0, 0.1) } else { inactive_bg }.into()),
                    text_color: if queue_selected || hovered { active_text } else { inactive_text },
                    border: iced_core::Border::default().rounded(20),
                    ..button::Style::default()
                }
            });
        
        let tabs = row![home_btn, all_btn, library_btn, queue_btn].spacing(4).align_y(Alignment::Center);
        
        // Search box - use simple text character instead of emoji for consistent sizing
        let search_icon = text("\u{2315}") // ⌕ APL functional symbol circle stile (simple magnifying glass)
            .size(14)
            .color(Color::from_rgb(0.5, 0.5, 0.5));

        let search_input = text_input("Search games...", &self.search_query)
            .on_input(Message::SearchChanged)
            .on_submit(Message::SearchSubmit)
            .padding(Padding::from([8, 0])) // Vertical padding for text alignment
            .width(Length::Fill)
            .style(|_theme: &Theme, _status| {
                text_input::Style {
                    background: Color::TRANSPARENT.into(),
                    border: iced_core::Border::default(),
                    placeholder: Color::from_rgb(0.5, 0.5, 0.5),
                    value: Color::WHITE,
                    selection: Color::from_rgb(0.463, 0.725, 0.0),
                    icon: Color::TRANSPARENT,
                }
            });

        let search_bar = container(
            row![
                Space::new().width(12),
                search_icon,
                Space::new().width(8),
                search_input,
                Space::new().width(12),
            ].align_y(Alignment::Center)
        )
        .width(300)
        .height(36)
        .style(|_| container::Style {
            background: Some(Color::from_rgb(0.137, 0.137, 0.176).into()),
            border: iced_core::Border::default().rounded(18).color(Color::from_rgb(0.235, 0.235, 0.294)).width(1.0),
            ..container::Style::default()
        });
        
        // Wrap search bar with vertical padding so it doesn't touch navbar edges
        let search_bar_padded = container(search_bar)
            .padding(Padding::from([12, 0])); // 12px top and bottom padding
        
        // User display name
        let user_text = text(user_name.unwrap_or("User"))
            .size(13)
            .color(Color::WHITE);
        
        // Settings icon button - wrap icon in centered container
        let settings_icon = container(
            text(icons::SETTINGS).size(16).color(Color::from_rgb(0.8, 0.8, 0.8))
        )
        .width(24)
        .height(24)
        .center_x(Length::Fill)
        .center_y(Length::Fill);
        
        let settings_btn = button(settings_icon)
            .width(36)
            .height(36)
            .padding(0)
            .on_press(Message::OpenSettings)
            .style(|_, status| button::Style {
                background: Some(if status == iced_widget::button::Status::Hovered {
                    Color::from_rgba(1.0, 1.0, 1.0, 0.1).into()
                } else {
                    Color::TRANSPARENT.into()
                }),
                text_color: if status == iced_widget::button::Status::Hovered {
                    Color::WHITE
                } else {
                    Color::from_rgb(0.8, 0.8, 0.8)
                },
                border: iced_core::Border::default().rounded(18),
                ..button::Style::default()
            });
            
        let logout_btn = button(
            text("Logout").size(13)
        )
            .padding(Padding::from([8, 16]))
            .on_press(Message::Logout)
            .style(|_, status| button::Style {
                background: Some(if status == iced_widget::button::Status::Hovered {
                    Color::from_rgba(1.0, 0.3, 0.3, 0.15).into()
                } else {
                    Color::from_rgb(0.196, 0.196, 0.255).into()
                }),
                text_color: Color::WHITE,
                border: iced_core::Border::default().rounded(6),
                ..button::Style::default()
            });
        
        // Layout: Logo | Tabs | [flex space] | Search | [flex space] | User | Settings | Logout
        // This centers the search bar between the left (logo+tabs) and right (user+buttons) sections
        let header_row = row![
            Space::new().width(24),
            logo,
            Space::new().width(32),
            tabs,
            Space::new().width(Length::Fill), // Left flex space
            search_bar_padded,
            Space::new().width(Length::Fill), // Right flex space
            user_text,
            Space::new().width(12),
            settings_btn,
            Space::new().width(8),
            logout_btn,
            Space::new().width(24),
        ]
        .align_y(Alignment::Center)
        .height(60); // Taller header
        
        container(header_row)
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(Color::from_rgb(0.086, 0.086, 0.118).into()), // rgb(22, 22, 30)
                border: iced_core::Border {
                    color: Color::from_rgb(0.05, 0.05, 0.08),
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..container::Style::default()
            })
            .into()
    }
    
    fn view_bottom_bar<'a>(
        &'a self,
        subscription: Option<&'a SubscriptionInfo>,
        servers: &'a [ServerInfo],
        selected_server_index: usize,
    ) -> Element<'a, Message, Theme, Renderer> {
        // Left side: subscription info
        let left_content: Element<'a, Message, Theme, Renderer> = if let Some(sub) = subscription {
            // Tier badge colors
            let (tier_bg, tier_fg) = match sub.membership_tier.as_str() {
                "ULTIMATE" => (
                    Color::from_rgb(0.314, 0.235, 0.039), // rgb(80, 60, 10)
                    Color::from_rgb(1.0, 0.843, 0.0),     // rgb(255, 215, 0) gold
                ),
                "PERFORMANCE" | "PRIORITY" => (
                    Color::from_rgb(0.275, 0.157, 0.078), // rgb(70, 40, 20)
                    Color::from_rgb(0.804, 0.686, 0.584), // rgb(205, 175, 149)
                ),
                _ => (
                    Color::from_rgb(0.176, 0.176, 0.176), // rgb(45, 45, 45)
                    Color::from_rgb(0.706, 0.706, 0.706), // rgb(180, 180, 180)
                ),
            };
            
            // Tier badge
            let tier_badge = container(
                text(&sub.membership_tier).size(11).color(tier_fg)
            )
            .padding(Padding::from([4, 8]))
            .style(move |_| container::Style {
                background: Some(tier_bg.into()),
                border: iced_core::Border::default().rounded(4),
                ..container::Style::default()
            });
            
            // Hours display
            let hours_display: Element<'a, Message, Theme, Renderer> = if sub.is_unlimited {
                row![
                    text(icons::CLOCK).size(14).color(Color::from_rgb(0.5, 0.5, 0.5)),
                    Space::new().width(6),
                    text(icons::INFINITY).size(15).color(Color::from_rgb(0.463, 0.725, 0.0)),
                ].align_y(Alignment::Center).into()
            } else {
                let hours_color = if sub.remaining_hours > 5.0 {
                    Color::from_rgb(0.463, 0.725, 0.0) // green
                } else if sub.remaining_hours > 1.0 {
                    Color::from_rgb(1.0, 0.784, 0.196) // yellow
                } else {
                    Color::from_rgb(1.0, 0.314, 0.314) // red
                };
                
                row![
                    text(icons::CLOCK).size(14).color(Color::from_rgb(0.5, 0.5, 0.5)),
                    Space::new().width(6),
                    text(format!("{:.1}h", sub.remaining_hours)).size(13).color(hours_color),
                    text(format!(" / {:.0}h", sub.total_hours)).size(12).color(Color::from_rgb(0.5, 0.5, 0.5)),
                ].align_y(Alignment::Center).into()
            };
            
            // Storage display (if available)
            let storage_display: Element<'a, Message, Theme, Renderer> = if sub.has_persistent_storage {
                if let Some(storage_gb) = sub.storage_size_gb {
                    row![
                        text(icons::STORAGE).size(14).color(Color::from_rgb(0.5, 0.5, 0.5)),
                        Space::new().width(6),
                        text(format!("{} GB", storage_gb)).size(13).color(Color::from_rgb(0.392, 0.706, 1.0)),
                    ].align_y(Alignment::Center).into()
                } else {
                    Space::new().into()
                }
            } else {
                Space::new().into()
            };
            
            row![
                tier_badge,
                Space::new().width(20),
                hours_display,
                Space::new().width(20),
                storage_display,
            ]
            .align_y(Alignment::Center)
            .into()
        } else {
            text("Loading subscription info...")
                .size(12)
                .color(Color::from_rgb(0.5, 0.5, 0.5))
                .into()
        };
        
        // Right side: server info
        let server_display: Element<'a, Message, Theme, Renderer> = if let Some(server) = servers.get(selected_server_index) {
            let ping_text = server.ping_ms
                .map(|p| format!(" ({}ms)", p))
                .unwrap_or_default();
            
            row![
                text(icons::SERVER).size(14).color(Color::from_rgb(0.392, 0.706, 1.0)),
                Space::new().width(6),
                text(format!("{}{}", server.name, ping_text))
                    .size(12)
                    .color(Color::from_rgb(0.392, 0.706, 1.0))
            ].align_y(Alignment::Center).into()
        } else {
            row![
                text(icons::SERVER).size(14).color(Color::from_rgb(0.5, 0.5, 0.5)),
                Space::new().width(6),
                text("Auto (waiting for ping)")
                    .size(12)
                    .color(Color::from_rgb(0.5, 0.5, 0.5))
            ].align_y(Alignment::Center).into()
        };
        
        let bar_row = row![
            Space::new().width(24),
            left_content,
            Space::new().width(Length::Fill),
            server_display,
            Space::new().width(24),
        ]
        .align_y(Alignment::Center)
        .height(32); // Fixed height for bottom bar
        
        container(bar_row)
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(Color::from_rgb(0.086, 0.086, 0.118).into()), // rgb(22, 22, 30)
                border: iced_core::Border {
                    color: Color::from_rgb(0.05, 0.05, 0.08),
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..container::Style::default()
            })
            .into()
    }
    
    
    fn view_home_sections<'a>(&'a self, sections: &'a [GameSection]) -> Element<'a, Message, Theme, Renderer> {
        // Show loading message if no sections yet
        if sections.is_empty() {
            return container(
                text("Loading game sections...")
                    .size(16)
                    .color(Color::from_rgb(0.5, 0.5, 0.5))
            )
            .width(Length::Fill)
            .height(300)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
        }
        
        let mut content = column![].spacing(24).padding(16);
        
        for section in sections {
            if section.games.is_empty() {
                continue;
            }
            
            let title = text(&section.title)
                .size(20)
                .color(Color::WHITE);
            
            let games_row = self.view_games_row(&section.games);
            
            content = content.push(column![
                title,
                Space::new().height(12),
                scrollable(games_row).direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::default()
                )),
            ]);
        }
        
        content.into()
    }
    
    fn view_games_row<'a>(&'a self, games: &'a [GameInfo]) -> Element<'a, Message, Theme, Renderer> {
        let mut row_content = row![].spacing(20);
        
        for game in games.iter().take(12) {
            row_content = row_content.push(self.view_game_card(game));
        }
        
        row_content.into()
    }
    
    fn view_games_grid<'a>(&'a self, games: &'a [GameInfo]) -> Element<'a, Message, Theme, Renderer> {
        // Show loading message if no games yet
        if games.is_empty() {
            return container(
                text("Loading games...")
                    .size(16)
                    .color(Color::from_rgb(0.5, 0.5, 0.5))
            )
            .width(Length::Fill)
            .height(300)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
        }
        
        // Filter games based on search
        let filtered: Vec<&GameInfo> = if self.search_query.is_empty() {
            games.iter().collect()
        } else {
            let query = self.search_query.to_lowercase();
            games.iter()
                .filter(|g| g.title.to_lowercase().contains(&query))
                .collect()
        };
        
        // Show no results message if search found nothing
        if filtered.is_empty() {
            return container(
                text(format!("No games found for '{}'", self.search_query))
                    .size(16)
                    .color(Color::from_rgb(0.5, 0.5, 0.5))
            )
            .width(Length::Fill)
            .height(300)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
        }
        
        let mut rows: Vec<Element<'a, Message, Theme, Renderer>> = Vec::new();
        
        // Use 6 columns for 185px cards with 20px spacing
        // 6 * 185 + 5 * 20 = 1210px content width (fits most screens)
        for chunk in filtered.chunks(6) {
            let mut row_items: Vec<Element<'a, Message, Theme, Renderer>> = Vec::new();
            
            for game in chunk {
                row_items.push(self.view_game_card(game));
            }
            
            // Fill remaining slots with fixed-width spaces to maintain grid alignment
            while row_items.len() < 6 {
                row_items.push(Space::new().width(185).height(320).into());
            }
            
            rows.push(
                iced_widget::Row::with_children(row_items)
                    .spacing(20)
                    .into()
            );
        }
        
        iced_widget::Column::with_children(rows)
            .spacing(24)
            .padding(24)
            .into()
    }
    
    fn view_game_card<'a>(&'a self, game: &'a GameInfo) -> Element<'a, Message, Theme, Renderer> {
        let game_clone = game.clone();
        
        // Modern card dimensions - 2:3 aspect ratio like game covers
        let card_width = 185;
        let card_height = 260;
        
        // Colors
        let card_bg = Color::from_rgb(0.12, 0.12, 0.15);
        let card_bg_hover = Color::from_rgb(0.16, 0.16, 0.20);
        let gfn_green = Color::from_rgb(0.467, 0.784, 0.196);
        
        // Build image content
        let img_content: Element<'a, Message, Theme, Renderer> = if let Some(ref url) = game.image_url {
            if let Some(handle) = self.loaded_images.get(url) {
                // Image loaded - show it
                image(handle.clone())
                    .width(Length::Fixed(card_width as f32))
                    .height(Length::Fixed(card_height as f32))
                    .content_fit(iced_core::ContentFit::Cover)
                    .into()
            } else {
                // Loading placeholder - show abbreviated title
                let initials: String = game.title
                    .split_whitespace()
                    .take(2)
                    .filter_map(|w| w.chars().next())
                    .collect();
                
                container(
                    text(if initials.is_empty() { "...".to_string() } else { initials })
                        .size(36)
                        .color(Color::from_rgba(1.0, 1.0, 1.0, 0.15))
                )
                .width(Length::Fixed(card_width as f32))
                .height(Length::Fixed(card_height as f32))
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
            }
        } else {
            // No image URL - show game controller emoji placeholder
            container(
                text("\u{1F3AE}") // 🎮 game controller
                    .size(32)
                    .color(Color::from_rgba(1.0, 1.0, 1.0, 0.15))
            )
            .width(Length::Fixed(card_width as f32))
            .height(Length::Fixed(card_height as f32))
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
        };
        
        // Wrap image in a clipping container with rounded corners
        let image_container = container(img_content)
            .width(Length::Fixed(card_width as f32))
            .height(Length::Fixed(card_height as f32))
            .clip(true)
            .style(move |_| container::Style {
                background: Some(card_bg.into()),
                border: iced_core::Border::default().rounded(10),
                ..container::Style::default()
            });
        
        // Title with truncation - max ~2 lines worth
        let title_text = text(truncate_string(&game.title, 32))
            .size(13)
            .color(Color::WHITE);
        
        // Store badge - subtle pill style
        let store_badge = container(
            text(&game.store)
                .size(10)
                .color(Color::from_rgb(0.55, 0.55, 0.55))
        )
        .padding(Padding::from([2, 6]))
        .style(|_| container::Style {
            background: Some(Color::from_rgba(1.0, 1.0, 1.0, 0.06).into()),
            border: iced_core::Border::default().rounded(4),
            ..container::Style::default()
        });
        
        // Card content layout
        let card_content = column![
            image_container,
            Space::new().height(10),
            container(title_text)
                .width(Length::Fixed(card_width as f32))
                .height(36), // Fixed height for ~2 lines
            Space::new().height(4),
            store_badge,
        ]
        .width(Length::Fixed(card_width as f32));
        
        // Wrap in button for interactivity
        button(card_content)
            .padding(8)
            .on_press(Message::GameClicked(game_clone))
            .width(Length::Fixed(card_width as f32))
            .style(move |_, status| {
                let hovered = status == iced_widget::button::Status::Hovered;
                let pressed = status == iced_widget::button::Status::Pressed;
                
                button::Style {
                    background: Some(if pressed {
                        Color::from_rgb(0.1, 0.1, 0.12).into()
                    } else if hovered {
                        card_bg_hover.into()
                    } else {
                        Color::TRANSPARENT.into()
                    }),
                    text_color: Color::WHITE,
                    border: iced_core::Border {
                        color: if hovered || pressed { 
                            gfn_green 
                        } else { 
                            Color::TRANSPARENT 
                        },
                        width: if hovered || pressed { 2.0 } else { 0.0 },
                        radius: 12.0.into(),
                    },
                    shadow: if hovered {
                        iced_core::Shadow {
                            color: Color::from_rgba(0.467, 0.784, 0.196, 0.25), // Green glow
                            offset: iced_core::Vector::new(0.0, 4.0),
                            blur_radius: 16.0,
                        }
                    } else {
                        iced_core::Shadow::default()
                    },
                    snap: false,
                }
            })
            .into()
    }
    
    fn view_queue_times<'a>(&'a self) -> Element<'a, Message, Theme, Renderer> {
        // Placeholder for queue times view
        container(text("Queue Times - Coming Soon").color(Color::WHITE))
            .width(Length::Fill)
            .height(300)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }
    
    fn view_session<'a>(&'a self, status_message: &'a str) -> Element<'a, Message, Theme, Renderer> {
        let spinner = text(icons::SPINNER)
            .size(64)
            .color(Color::from_rgb(0.467, 0.784, 0.196));
        
        let status = text(status_message)
            .size(18)
            .color(Color::WHITE);
        
        let cancel_btn = button(
            container(text("Cancel").size(16))
                .padding(Padding::from([12, 32]))
        )
        .on_press(Message::CancelSession)
        .style(|_, status| button::Style {
            background: Some(if status == iced_widget::button::Status::Hovered {
                Color::from_rgb(0.4, 0.2, 0.2).into()
            } else {
                Color::from_rgb(0.3, 0.15, 0.15).into()
            }),
            text_color: Color::WHITE,
            border: iced_core::Border::default().rounded(8),
            ..button::Style::default()
        });
        
        let content = column![
            Space::new().height(Length::FillPortion(1)),
            spinner,
            Space::new().height(24),
            status,
            Space::new().height(32),
            cancel_btn,
            Space::new().height(Length::FillPortion(1)),
        ]
        .align_x(Alignment::Center);
        
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_| container::Style {
                background: Some(Color::from_rgb(0.078, 0.078, 0.118).into()),
                ..container::Style::default()
            })
            .into()
    }
    
    fn view_with_settings_modal<'a>(
        &'a self,
        background: Element<'a, Message, Theme, Renderer>,
        servers: &'a [ServerInfo],
        selected_server_index: usize,
        subscription: Option<&'a SubscriptionInfo>,
    ) -> Element<'a, Message, Theme, Renderer> {
        let modal = self.view_settings_modal(servers, selected_server_index, subscription);
        
        // Overlay modal on background
        iced_widget::Stack::with_children(vec![
            background,
            container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_| container::Style {
                    background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.7).into()),
                    ..container::Style::default()
                })
                .into(),
        ])
        .into()
    }
    
    fn view_settings_modal<'a>(
        &'a self,
        servers: &'a [ServerInfo],
        selected_server_index: usize,
        subscription: Option<&'a SubscriptionInfo>,
    ) -> Element<'a, Message, Theme, Renderer> {
        let _section_title_color = Color::from_rgb(0.467, 0.784, 0.196); // GFN Green
        let label_width = 140;
        
        // Helper to create a section card
        let section_card = |title: &str, content: Element<'a, Message, Theme, Renderer>| {
            container(
                column![
                    row![
                        // Accent line
                        container(Space::new().width(4).height(20))
                            .style(|_| container::Style {
                                background: Some(Color::from_rgb(0.467, 0.784, 0.196).into()),
                                border: iced_core::Border::default().rounded(2),
                                ..container::Style::default()
                            }),
                        Space::new().width(12),
                        text(title.to_string()).size(18).color(Color::WHITE),
                    ].align_y(Alignment::Center),
                    Space::new().height(16),
                    content,
                ]
                .padding(16)
            )
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(Color::from_rgb(0.12, 0.12, 0.16).into()),
                border: iced_core::Border::default().rounded(12),
                ..container::Style::default()
            })
        };

        let title = row![
            text("Settings").size(28).color(Color::WHITE),
            Space::new().width(Length::Fill),
            button(text(icons::CLOSE).size(20))
                .on_press(Message::CloseSettings)
                .padding(10)
                .style(|_, status| button::Style {
                    background: Some(if status == iced_widget::button::Status::Hovered {
                        Color::from_rgba(1.0, 1.0, 1.0, 0.1).into()
                    } else {
                        Color::TRANSPARENT.into()
                    }),
                    text_color: if status == iced_widget::button::Status::Hovered {
                        Color::WHITE
                    } else {
                        Color::from_rgb(0.7, 0.7, 0.7)
                    },
                    border: iced_core::Border::default().rounded(20),
                    ..button::Style::default()
                }),
        ].align_y(Alignment::Center);
        
        // === BUILD RESOLUTION OPTIONS FROM SUBSCRIPTION ===
        // Group entitled resolutions by aspect ratio
        let (resolution_options, fps_options) = if let Some(sub) = subscription {
            if !sub.entitled_resolutions.is_empty() {
                // Build unique resolutions grouped by aspect ratio
                let mut unique_resolutions: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
                let mut resolutions: Vec<(u32, u32, String)> = Vec::new(); // (width, height, display_name)
                
                // Sort by width then height descending
                let mut sorted_res = sub.entitled_resolutions.clone();
                sorted_res.sort_by(|a, b| b.width.cmp(&a.width).then(b.height.cmp(&a.height)));
                
                for res in sorted_res {
                    let key = (res.width, res.height);
                    if unique_resolutions.contains(&key) {
                        continue;
                    }
                    unique_resolutions.insert(key);
                    
                    // Calculate aspect ratio category
                    let ratio = res.width as f32 / res.height as f32;
                    let category = if (ratio - 16.0/9.0).abs() < 0.05 {
                        "16:9"
                    } else if (ratio - 16.0/10.0).abs() < 0.05 {
                        "16:10"
                    } else if (ratio - 21.0/9.0).abs() < 0.05 {
                        "21:9"
                    } else if (ratio - 32.0/9.0).abs() < 0.05 {
                        "32:9"
                    } else if (ratio - 4.0/3.0).abs() < 0.05 {
                        "4:3"
                    } else {
                        "Other"
                    };
                    
                    // Friendly name
                    let name = match (res.width, res.height) {
                        (1280, 720) => format!("1280x720 - 720p HD [{}]", category),
                        (1600, 900) => format!("1600x900 - 900p [{}]", category),
                        (1920, 1080) => format!("1920x1080 - 1080p FHD [{}]", category),
                        (2560, 1440) => format!("2560x1440 - 1440p QHD [{}]", category),
                        (3840, 2160) => format!("3840x2160 - 4K UHD [{}]", category),
                        (2560, 1080) => format!("2560x1080 - Ultrawide [{}]", category),
                        (3440, 1440) => format!("3440x1440 - Ultrawide QHD [{}]", category),
                        (5120, 1440) => format!("5120x1440 - Super Ultrawide [{}]", category),
                        (w, h) => format!("{}x{} [{}]", w, h, category),
                    };
                    
                    resolutions.push((res.width, res.height, name));
                }
                
                // Build FPS options for current resolution
                let (current_w, current_h) = crate::app::parse_resolution(&self.settings.resolution);
                let mut available_fps: Vec<u32> = sub.entitled_resolutions
                    .iter()
                    .filter(|r| r.width == current_w && r.height == current_h)
                    .map(|r| r.fps)
                    .collect();
                
                // If no FPS for this resolution, use all available FPS
                if available_fps.is_empty() {
                    available_fps = sub.entitled_resolutions.iter().map(|r| r.fps).collect();
                }
                available_fps.sort();
                available_fps.dedup();
                
                let res_strings: Vec<String> = resolutions.iter().map(|(_, _, name)| name.clone()).collect();
                (res_strings, available_fps)
            } else {
                // Fallback to static resolutions
                (self.settings.aspect_ratio.resolutions().iter().map(|(r, label)| format!("{} ({})", r, label)).collect(), FPS_OPTIONS.to_vec())
            }
        } else {
            // No subscription, use static fallback
            (self.settings.aspect_ratio.resolutions().iter().map(|(r, label)| format!("{} ({})", r, label)).collect(), FPS_OPTIONS.to_vec())
        };
        
        // Find current resolution in options
        let current_resolution = resolution_options.iter()
            .find(|r| r.starts_with(&self.settings.resolution))
            .cloned();
        
        // === VIDEO SETTINGS ===
        let codec_options: Vec<String> = VideoCodec::all().iter().map(|c| c.as_str().to_string()).collect();
        let current_codec = self.settings.codec.as_str().to_string();
        
        // Get platform-specific decoder options
        let decoder_options: Vec<String> = get_platform_decoder_options()
            .iter()
            .map(|d| d.as_str().to_string())
            .collect();
        let current_decoder = self.settings.decoder_backend.as_str().to_string();
        
        let color_options: Vec<String> = ColorQuality::all().iter().map(|c| c.display_name().to_string()).collect();
        let current_color = self.settings.color_quality.display_name().to_string();
        
        let video_content = column![
            // Bitrate
            row![
                text("Max Bitrate").size(14).color(Color::from_rgb(0.8, 0.8, 0.8)).width(label_width),
                column![
                    row![
                        slider(10.0..=200.0, self.bitrate_value, Message::BitrateChanged)
                            .step(5.0)
                            .width(Length::Fill),
                        Space::new().width(12),
                        text(format!("{} Mbps", self.settings.max_bitrate_mbps)).size(14).color(Color::WHITE),
                    ].align_y(Alignment::Center),
                ].width(Length::Fill),
            ].align_y(Alignment::Center),
            Space::new().height(16),
            
            // Resolution - populated from subscription entitled resolutions
            row![
                text("Resolution").size(14).color(Color::from_rgb(0.8, 0.8, 0.8)).width(label_width),
                pick_list(
                    resolution_options,
                    current_resolution,
                    |selected: String| {
                        // Extract the resolution part (e.g., "1920x1080" from "1920x1080 - 1080p FHD [16:9]")
                        let res = selected.split(" - ").next()
                            .or_else(|| selected.split(" (").next())
                            .unwrap_or(&selected)
                            .to_string();
                        Message::ResolutionChanged(res)
                    },
                ).width(Length::Fill).text_size(14),
            ].align_y(Alignment::Center),
            Space::new().height(12),
            
            // FPS - populated from subscription for current resolution
            row![
                text("Frame Rate").size(14).color(Color::from_rgb(0.8, 0.8, 0.8)).width(label_width),
                pick_list(fps_options, Some(self.settings.fps), Message::FpsChanged)
                    .width(Length::Fill).text_size(14),
            ].align_y(Alignment::Center),
            Space::new().height(12),
            
            // Codec
            row![
                text("Codec").size(14).color(Color::from_rgb(0.8, 0.8, 0.8)).width(label_width),
                pick_list(codec_options, Some(current_codec), Message::CodecChanged)
                    .width(Length::Fill).text_size(14),
            ].align_y(Alignment::Center),
            Space::new().height(12),
            
            // Decoder
            row![
                text("Decoder").size(14).color(Color::from_rgb(0.8, 0.8, 0.8)).width(label_width),
                pick_list(decoder_options, Some(current_decoder), Message::DecoderChanged)
                    .width(Length::Fill).text_size(14),
            ].align_y(Alignment::Center),
            Space::new().height(12),
            
            // Color Quality
            row![
                text("Color Quality").size(14).color(Color::from_rgb(0.8, 0.8, 0.8)).width(label_width),
                pick_list(color_options, Some(current_color), Message::ColorQualityChanged)
                    .width(Length::Fill).text_size(14),
            ].align_y(Alignment::Center),
            Space::new().height(16),
            
            // HDR
            checkbox(self.settings.hdr_enabled)
                .label("Enable HDR (High Dynamic Range)")
                .on_toggle(Message::HdrChanged)
                .text_size(14)
                .style(|theme, status| {
                    let mut style = checkbox::primary(theme, status);
                    style.text_color = Some(Color::WHITE);
                    style
                }),
        ];
        
        let video_section = section_card("Video Stream", video_content.into());
        
        // === DISPLAY SETTINGS ===
        let display_content = column![
            checkbox(self.settings.fullscreen)
                .label("Fullscreen Mode")
                .on_toggle(Message::FullscreenChanged)
                .text_size(14)
                .style(|theme, status| {
                    let mut style = checkbox::primary(theme, status);
                    style.text_color = Some(Color::WHITE);
                    style
                }),
            Space::new().height(12),
            
            checkbox(self.settings.borderless)
                .label("Borderless Window")
                .on_toggle(Message::BorderlessChanged)
                .text_size(14)
                .style(|theme, status| {
                    let mut style = checkbox::primary(theme, status);
                    style.text_color = Some(Color::WHITE);
                    style
                }),
            Space::new().height(12),
            
            checkbox(self.settings.vsync)
                .label("VSync (Vertical Sync)")
                .on_toggle(Message::VsyncChanged)
                .text_size(14)
                .style(|theme, status| {
                    let mut style = checkbox::primary(theme, status);
                    style.text_color = Some(Color::WHITE);
                    style
                }),
        ];
        
        let display_section = section_card("Display", display_content.into());
        
        // === PERFORMANCE SETTINGS ===
        let performance_content = column![
            checkbox(self.settings.low_latency_mode)
                .label("Low Latency Mode (Reduces buffering)")
                .on_toggle(Message::LowLatencyChanged)
                .text_size(14)
                .style(|theme, status| {
                    let mut style = checkbox::primary(theme, status);
                    style.text_color = Some(Color::WHITE);
                    style
                }),
        ];
        
        let performance_section = section_card("Performance", performance_content.into());
        
        // === AUDIO SETTINGS ===
        let audio_content = column![
            checkbox(self.settings.surround)
                .label("5.1 Surround Sound")
                .on_toggle(Message::SurroundChanged)
                .text_size(14)
                .style(|theme, status| {
                    let mut style = checkbox::primary(theme, status);
                    style.text_color = Some(Color::WHITE);
                    style
                }),
        ];
        
        let audio_section = section_card("Audio", audio_content.into());
        
        // === INPUT SETTINGS ===
        let input_content = column![
            checkbox(self.settings.clipboard_paste_enabled)
                .label("Enable Clipboard Paste (Ctrl+V)")
                .on_toggle(Message::ClipboardPasteChanged)
                .text_size(14)
                .style(|theme, status| {
                    let mut style = checkbox::primary(theme, status);
                    style.text_color = Some(Color::WHITE);
                    style
                }),
        ];
        
        let input_section = section_card("Input", input_content.into());
        
        // === GAME SETTINGS ===
        let language_options: Vec<String> = GameLanguage::all().iter().map(|l| l.display_name().to_string()).collect();
        let current_language = self.settings.game_language.display_name().to_string();
        
        let game_content = row![
            text("Language").size(14).color(Color::from_rgb(0.8, 0.8, 0.8)).width(label_width),
            pick_list(language_options, Some(current_language), Message::GameLanguageChanged)
                .width(Length::Fill).text_size(14),
        ].align_y(Alignment::Center);
        
        let game_section = section_card("Game", game_content.into());
        
        // === SERVER SETTINGS ===
        let server_names: Vec<String> = servers.iter()
            .map(|s| format!("{} ({})", s.name, s.ping_ms.map(|p| format!("{}ms", p)).unwrap_or("?".into())))
            .collect();
        let selected_server = server_names.get(selected_server_index).cloned();
        
        let server_content = column![
            row![
                text("Region").size(14).color(Color::from_rgb(0.8, 0.8, 0.8)).width(label_width),
                pick_list(server_names, selected_server, |_s| Message::ServerSelected(0))
                    .width(Length::Fill).text_size(14),
            ].align_y(Alignment::Center),
            Space::new().height(16),
            
            button(text("Test Server Ping").size(13))
                .on_press(Message::StartPingTest)
                .padding(Padding::from([8, 16]))
                .style(|_, status| {
                     let hovered = status == iced_widget::button::Status::Hovered;
                     button::Style {
                        background: Some(if hovered {
                             Color::from_rgb(0.25, 0.25, 0.3).into()
                        } else {
                             Color::from_rgb(0.2, 0.2, 0.25).into()
                        }),
                        text_color: Color::WHITE,
                        border: iced_core::Border::default().rounded(6),
                        ..button::Style::default()
                    }
                }),
        ];
        
        let server_section = section_card("Server", server_content.into());
        
        // Reset button
        let reset_section = column![
            Space::new().height(12),
            button(text("Reset All Settings to Defaults").size(14))
                .on_press(Message::ResetSettings)
                .padding(Padding::from([12, 24]))
                .width(Length::Fill)
                .style(|_, status| {
                     let hovered = status == iced_widget::button::Status::Hovered;
                     button::Style {
                        background: Some(if hovered {
                             Color::from_rgb(0.4, 0.2, 0.2).into()
                        } else {
                             Color::from_rgb(0.3, 0.15, 0.15).into()
                        }),
                        text_color: Color::WHITE,
                        border: iced_core::Border::default().rounded(8),
                        ..button::Style::default()
                    }
                }),
        ];
        
        let content = column![
            title,
            Space::new().height(20),
            scrollable(
                column![
                    video_section,
                    display_section,
                    performance_section,
                    audio_section,
                    input_section,
                    game_section,
                    server_section,
                    reset_section,
                    Space::new().height(20),
                ]
                .spacing(16)
                .padding(Padding::from([0.0, 12.0])) // Padding for scrollbar
            )
            .height(Length::Fill),
        ]
        .padding(32);
        
        container(content)
            .width(600)
            .height(750)
            .style(|_| container::Style {
                background: Some(Color::from_rgb(0.09, 0.09, 0.12).into()),
                border: iced_core::Border::default().rounded(16),
                shadow: iced_core::Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.5),
                    offset: iced_core::Vector::new(0.0, 10.0),
                    blur_radius: 30.0,
                },
                ..container::Style::default()
            })
            .into()
    }
    
    fn view_with_game_popup<'a>(
        &'a self,
        background: Element<'a, Message, Theme, Renderer>,
        game: &'a GameInfo,
    ) -> Element<'a, Message, Theme, Renderer> {
        let popup = self.view_game_popup(game);
        
        iced_widget::Stack::with_children(vec![
            background,
            
            // Overlay - Click to close
            button(container(Space::new().width(Length::Fill).height(Length::Fill)))
                .on_press(Message::GamePopupClose)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_, _| button::Style {
                    background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.8).into()),
                    ..button::Style::default()
                })
                .into(),
            
            // Popup Container - Centered
            // Note: We don't want clicks on the popup to close it.
            // By wrapping the popup in a container that sits on top, we ensure visuals are correct.
            // To prevent click-through, the popup content itself (view_game_popup) 
            // should be contained in a widget that consumes mouse events (like a button with no action? or just opaque container)
            // In Iced, a container with background doesn't necessarily block mouse events if it doesn't handle them.
            // A simple trick is to wrap the popup content in a generic element that stops propagation, 
            // but Iced doesn't have a simple "stop propagation" widget.
            // Instead, we can make the overlay button separate from the popup container.
            
            container(popup)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into(),
        ])
        .into()
    }
    
    fn view_game_popup<'a>(&'a self, game: &'a GameInfo) -> Element<'a, Message, Theme, Renderer> {
        let game_clone = game.clone();
        
        // --- Left Column: Game Art ---
        let img_width = 260;
        let img_height = 350;
        
        let img_section: Element<'a, Message, Theme, Renderer> = if let Some(ref url) = game.image_url {
            if let Some(handle) = self.loaded_images.get(url) {
                container(
                    image(handle.clone())
                        .width(img_width)
                        .height(img_height)
                        .content_fit(iced_core::ContentFit::Cover)
                )
                .width(img_width)
                .height(img_height)
                .clip(true)
                .style(|_| container::Style {
                    background: Some(Color::BLACK.into()),
                    border: iced_core::Border::default().rounded(8),
                    ..container::Style::default()
                })
                .into()
            } else {
                container(
                    text(&game.title).size(20).color(Color::from_rgb(0.5, 0.5, 0.5))
                )
                .width(img_width)
                .height(img_height)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_| container::Style {
                    background: Some(Color::from_rgb(0.15, 0.15, 0.2).into()),
                    border: iced_core::Border::default().rounded(8),
                    ..container::Style::default()
                })
                .into()
            }
        } else {
            container(Space::new())
                .width(img_width)
                .height(img_height)
                .style(|_| container::Style {
                    background: Some(Color::from_rgb(0.15, 0.15, 0.2).into()),
                    border: iced_core::Border::default().rounded(8),
                    ..container::Style::default()
                })
                .into()
        };

        // --- Right Column: Details ---
        
        // Header: Title + Close Button
        let header = row![
            column![
                text(&game.title).size(32).color(Color::WHITE),
                Space::new().height(4),
                row![
                    container(text(&game.store).size(12).color(Color::WHITE))
                        .padding(Padding::from([4, 8]))
                        .style(|_| container::Style {
                            background: Some(Color::from_rgb(0.25, 0.25, 0.3).into()),
                            border: iced_core::Border::default().rounded(4),
                            ..container::Style::default()
                        }),
                    Space::new().width(8),
                    if let Some(ref pub_name) = game.publisher {
                        text(pub_name).size(14).color(Color::from_rgb(0.7, 0.7, 0.7))
                    } else {
                        text("").size(14).color(Color::TRANSPARENT)
                    }
                ].align_y(Alignment::Center),
            ],
            Space::new().width(Length::Fill),
            button(text(icons::CLOSE).size(24))
                .on_press(Message::GamePopupClose)
                .padding(8)
                .style(|_, status| button::Style {
                    background: Some(if status == iced_widget::button::Status::Hovered {
                        Color::from_rgba(1.0, 1.0, 1.0, 0.1).into()
                    } else {
                        Color::TRANSPARENT.into()
                    }),
                    text_color: Color::WHITE,
                    border: iced_core::Border::default().rounded(20),
                    ..button::Style::default()
                }),
        ]
        .align_y(Alignment::Start);
        
        // Description
        let desc_text = if let Some(ref desc) = game.description {
            text(desc)
                .size(15)
                .color(Color::from_rgb(0.9, 0.9, 0.9))
                .line_height(1.4)
        } else {
            text("No description available for this game.")
                .size(15)
                .color(Color::from_rgb(0.5, 0.5, 0.5))
        };
        
        // Playability / Tier Info (if available in GameInfo, which it is)
        let mut meta_rows = column![].spacing(8);
        if let Some(ref tier) = game.membership_tier_label {
            meta_rows = meta_rows.push(
                row![
                    text("Required Tier:").size(14).color(Color::from_rgb(0.7, 0.7, 0.7)),
                    Space::new().width(8),
                    text(tier).size(14).color(Color::from_rgb(0.467, 0.784, 0.196)),
                ]
            );
        }
        
        // Variants selection
        let variants_section: Element<'a, Message, Theme, Renderer> = if game.variants.len() > 1 {
            let mut variant_btns: Vec<Element<'a, Message, Theme, Renderer>> = Vec::new();
            for (i, variant) in game.variants.iter().enumerate() {
                let is_selected = i == game.selected_variant_index;
                variant_btns.push(
                    button(text(&variant.store).size(13))
                        .padding(Padding::from([8, 16]))
                        .on_press(Message::VariantSelected(i))
                        .style(move |_, status| {
                             let hovered = status == iced_widget::button::Status::Hovered;
                            if is_selected {
                                button::Style {
                                    background: Some(Color::from_rgb(0.467, 0.784, 0.196).into()),
                                    text_color: Color::BLACK,
                                    border: iced_core::Border::default().rounded(6),
                                    ..button::Style::default()
                                }
                            } else {
                                button::Style {
                                    background: Some(if hovered { 
                                        Color::from_rgb(0.3, 0.3, 0.35).into() 
                                    } else { 
                                        Color::from_rgb(0.2, 0.2, 0.25).into() 
                                    }),
                                    text_color: Color::WHITE,
                                    border: iced_core::Border::default().rounded(6),
                                    ..button::Style::default()
                                }
                            }
                        })
                        .into()
                );
            }
            column![
                text("Select Platform").size(14).color(Color::from_rgb(0.7, 0.7, 0.7)),
                Space::new().height(8),
                iced_widget::Row::with_children(variant_btns).spacing(8).wrap(),
            ]
            .into()
        } else {
            Space::new().into()
        };
        
        // Play Button
        let play_btn = button(
            container(
                row![
                    text(icons::PLAY).size(18),
                    Space::new().width(12),
                    text("PLAY NOW").size(16).font(Font::with_name("Inter-Bold")),
                ]
                .align_y(Alignment::Center)
            )
            .width(Length::Fill)
            .center_x(Length::Fill)
        )
        .padding(Padding::from([16, 32]))
        .width(Length::Fill)
        .on_press(Message::GameLaunch(game_clone))
        .style(|_, status| {
             let hovered = status == iced_widget::button::Status::Hovered;
             button::Style {
                background: Some(if hovered {
                    Color::from_rgb(0.533, 0.847, 0.263).into()
                } else {
                    Color::from_rgb(0.467, 0.784, 0.196).into()
                }),
                text_color: Color::BLACK,
                border: iced_core::Border::default().rounded(8),
                shadow: iced_core::Shadow {
                    color: if hovered { Color::from_rgba(0.467, 0.784, 0.196, 0.4) } else { Color::TRANSPARENT },
                    offset: iced_core::Vector::new(0.0, 4.0),
                    blur_radius: 12.0,
                },
                ..button::Style::default()
            }
        });
        
        let right_column = column![
            header,
            Space::new().height(24),
            scrollable(
                column![
                    desc_text,
                    Space::new().height(24),
                    meta_rows,
                    Space::new().height(24),
                    variants_section,
                ]
            ).height(Length::Fill),
            Space::new().height(24),
            play_btn,
        ]
        .width(Length::Fill)
        .height(Length::Fill);
        
        let content = row![
            img_section,
            Space::new().width(32),
            right_column,
        ]
        .padding(32);
        
        // We wrap the content in a dummy button to capture clicks (prevent closing)
        // This is a workaround since container doesn't block clicks by default in some Iced versions
        let card = container(content)
            .width(800)
            .height(550)
            .style(|_| container::Style {
                background: Some(Color::from_rgb(0.1, 0.1, 0.14).into()),
                border: iced_core::Border::default().rounded(16),
                shadow: iced_core::Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.6),
                    offset: iced_core::Vector::new(0.0, 20.0),
                    blur_radius: 40.0,
                },
                ..container::Style::default()
            });
            
        // Wrap in button with NO message (disabled?) No, disabled passes clicks?
        // Actually, if we give it a message that does nothing...
        // But `button` changes cursor.
        // Let's rely on standard behavior first. If click-through happens, we can fix it.
        card.into()
    }
    
    /// Stats overlay view for streaming (F3 to toggle)
    pub fn view_stats_overlay<'a>(
        &'a self,
        stats: &'a crate::media::StreamStats,
        decoder_backend: &'a str,
    ) -> Element<'a, Message, Theme, Renderer> {
        // Stats panel in bottom-left corner
        let green = Color::from_rgb(0.463, 0.725, 0.0);
        let white = Color::WHITE;
        let gray = Color::from_rgb(0.7, 0.7, 0.7);
        
        // FPS row
        let fps_row = row![
            text("FPS:").size(12).color(gray),
            Space::new().width(8),
            text(format!("{:.0}", stats.render_fps)).size(12).color(green),
            text(format!(" / {}", stats.target_fps)).size(12).color(gray),
        ];
        
        // Resolution row
        let res_row = row![
            text("Res:").size(12).color(gray),
            Space::new().width(8),
            text(&stats.resolution).size(12).color(white),
        ];
        
        // Codec row
        let codec_row = row![
            text("Codec:").size(12).color(gray),
            Space::new().width(8),
            text(&stats.codec).size(12).color(white),
        ];
        
        // Decoder row
        let decoder_display = if decoder_backend.is_empty() { "Auto" } else { decoder_backend };
        let decoder_row = row![
            text("Decoder:").size(12).color(gray),
            Space::new().width(8),
            text(decoder_display).size(12).color(white),
        ];
        
        // Bitrate row
        let bitrate_row = row![
            text("Bitrate:").size(12).color(gray),
            Space::new().width(8),
            text(format!("{:.1} Mbps", stats.bitrate_mbps)).size(12).color(white),
        ];
        
        // Latency row
        let latency_color = if stats.latency_ms < 30.0 {
            green
        } else if stats.latency_ms < 60.0 {
            Color::from_rgb(1.0, 0.784, 0.196) // yellow
        } else {
            Color::from_rgb(1.0, 0.314, 0.314) // red
        };
        let latency_row = row![
            text("Latency:").size(12).color(gray),
            Space::new().width(8),
            text(format!("{:.0} ms", stats.latency_ms)).size(12).color(latency_color),
        ];
        
        // Decode time row
        let decode_row = row![
            text("Decode:").size(12).color(gray),
            Space::new().width(8),
            text(format!("{:.1} ms", stats.decode_time_ms)).size(12).color(white),
        ];
        
        let stats_content = column![
            fps_row,
            res_row,
            codec_row,
            decoder_row,
            bitrate_row,
            latency_row,
            decode_row,
        ]
        .spacing(4)
        .padding(10);
        
        let stats_panel = container(stats_content)
            .style(|_| container::Style {
                background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.7).into()),
                border: iced_core::Border::default().rounded(8),
                ..container::Style::default()
            });
        
        // Position in bottom-left with margin
        container(
            column![
                Space::new().height(Length::Fill),
                row![
                    Space::new().width(10),
                    stats_panel,
                ],
                Space::new().height(10),
            ]
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}
