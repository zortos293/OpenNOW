//! App Data Cache Management
//!
//! Handles caching of games, library, subscription, sessions, and tokens.

use log::{error, info, warn};
use std::path::PathBuf;

use super::{
    ActiveSessionInfo, GameInfo, GameSection, SessionInfo, SessionState, SubscriptionInfo,
};
use crate::app::session::MediaConnectionInfo;
use crate::auth::AuthTokens;

/// Get the application data directory
/// Creates directory if it doesn't exist
pub fn get_app_data_dir() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static APP_DATA_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

    APP_DATA_DIR
        .get_or_init(|| {
            let data_dir = dirs::data_dir()?;
            let app_dir = data_dir.join("opennow");

            // Ensure directory exists
            if let Err(e) = std::fs::create_dir_all(&app_dir) {
                error!("Failed to create app data directory: {}", e);
            }

            // Migration: copy auth.json from legacy locations if it doesn't exist in new location
            let new_auth = app_dir.join("auth.json");
            if !new_auth.exists() {
                // Try legacy opennow-streamer location (config_dir)
                if let Some(config_dir) = dirs::config_dir() {
                    let legacy_path = config_dir.join("opennow-streamer").join("auth.json");
                    if legacy_path.exists() {
                        if let Err(e) = std::fs::copy(&legacy_path, &new_auth) {
                            warn!("Failed to migrate auth.json from legacy location: {}", e);
                        } else {
                            info!(
                                "Migrated auth.json from {:?} to {:?}",
                                legacy_path, new_auth
                            );
                        }
                    }
                }

                // Try gfn-client location (config_dir)
                if !new_auth.exists() {
                    if let Some(config_dir) = dirs::config_dir() {
                        let legacy_path = config_dir.join("gfn-client").join("auth.json");
                        if legacy_path.exists() {
                            if let Err(e) = std::fs::copy(&legacy_path, &new_auth) {
                                warn!("Failed to migrate auth.json from gfn-client: {}", e);
                            } else {
                                info!(
                                    "Migrated auth.json from {:?} to {:?}",
                                    legacy_path, new_auth
                                );
                            }
                        }
                    }
                }
            }

            Some(app_dir)
        })
        .clone()
}

// ============================================================
// Auth Token Cache
// ============================================================

pub fn tokens_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("auth.json"))
}

pub fn load_tokens() -> Option<AuthTokens> {
    let path = tokens_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let tokens: AuthTokens = serde_json::from_str(&content).ok()?;

    // If token is expired, try to refresh it
    if tokens.is_expired() {
        if tokens.can_refresh() {
            info!("Token expired, attempting refresh...");
            // Try synchronous refresh using a blocking tokio runtime
            match try_refresh_tokens_sync(&tokens) {
                Some(new_tokens) => {
                    info!("Token refresh successful!");
                    return Some(new_tokens);
                }
                None => {
                    warn!("Token refresh failed, clearing auth file");
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
            }
        } else {
            info!("Token expired and no refresh token available, clearing auth file");
            let _ = std::fs::remove_file(&path);
            return None;
        }
    }

    Some(tokens)
}

/// Attempt to refresh tokens synchronously (blocking)
/// Used when loading tokens at startup
fn try_refresh_tokens_sync(tokens: &AuthTokens) -> Option<AuthTokens> {
    let refresh_token = tokens.refresh_token.as_ref()?;

    // Create a new tokio runtime for this blocking operation
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;

    let refresh_token_clone = refresh_token.clone();
    let result = rt.block_on(async { crate::auth::refresh_token(&refresh_token_clone).await });

    match result {
        Ok(new_tokens) => {
            // Save the new tokens
            save_tokens(&new_tokens);
            Some(new_tokens)
        }
        Err(e) => {
            warn!("Token refresh failed: {}", e);
            None
        }
    }
}

pub fn save_tokens(tokens: &AuthTokens) {
    if let Some(path) = tokens_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(tokens) {
            if let Err(e) = std::fs::write(&path, &json) {
                error!("Failed to save tokens: {}", e);
            } else {
                info!("Saved tokens to {:?}", path);
            }
        }
    }
}

pub fn clear_tokens() {
    if let Some(path) = tokens_path() {
        let _ = std::fs::remove_file(path);
        info!("Cleared auth tokens");
    }
}

// ============================================================
// Login Provider Cache (for Alliance persistence)
// ============================================================

use crate::auth::LoginProvider;

fn provider_cache_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("login_provider.json"))
}

pub fn save_login_provider(provider: &LoginProvider) {
    if let Some(path) = provider_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(provider) {
            if let Err(e) = std::fs::write(&path, &json) {
                error!("Failed to save login provider: {}", e);
            } else {
                info!(
                    "Saved login provider: {}",
                    provider.login_provider_display_name
                );
            }
        }
    }
}

pub fn load_login_provider() -> Option<LoginProvider> {
    let path = provider_cache_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let provider: LoginProvider = serde_json::from_str(&content).ok()?;
    info!(
        "Loaded cached login provider: {}",
        provider.login_provider_display_name
    );
    Some(provider)
}

pub fn clear_login_provider() {
    if let Some(path) = provider_cache_path() {
        let _ = std::fs::remove_file(path);
        info!("Cleared cached login provider");
    }
}

// ============================================================
// Games Cache
// ============================================================

fn games_cache_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("games_cache.json"))
}

pub fn save_games_cache(games: &[GameInfo]) {
    if let Some(path) = games_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(games) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_games_cache() -> Option<Vec<GameInfo>> {
    let path = games_cache_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn clear_games_cache() {
    if let Some(path) = games_cache_path() {
        let _ = std::fs::remove_file(path);
    }
}

// ============================================================
// Library Cache
// ============================================================

fn library_cache_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("library_cache.json"))
}

pub fn save_library_cache(games: &[GameInfo]) {
    if let Some(path) = library_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(games) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_library_cache() -> Option<Vec<GameInfo>> {
    let path = library_cache_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

// ============================================================
// Game Sections Cache (Home tab)
// ============================================================

/// Serializable section for cache
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedSection {
    id: Option<String>,
    title: String,
    games: Vec<GameInfo>,
}

fn sections_cache_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("sections_cache.json"))
}

pub fn save_sections_cache(sections: &[GameSection]) {
    if let Some(path) = sections_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cached: Vec<CachedSection> = sections
            .iter()
            .map(|s| CachedSection {
                id: s.id.clone(),
                title: s.title.clone(),
                games: s.games.clone(),
            })
            .collect();
        if let Ok(json) = serde_json::to_string(&cached) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_sections_cache() -> Option<Vec<GameSection>> {
    let path = sections_cache_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    let cached: Vec<CachedSection> = serde_json::from_str(&content).ok()?;
    Some(
        cached
            .into_iter()
            .map(|c| GameSection {
                id: c.id,
                title: c.title,
                games: c.games,
            })
            .collect(),
    )
}

// ============================================================
// Subscription Cache
// ============================================================

fn subscription_cache_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("subscription_cache.json"))
}

pub fn save_subscription_cache(sub: &SubscriptionInfo) {
    if let Some(path) = subscription_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cache = serde_json::json!({
            "membership_tier": sub.membership_tier,
            "remaining_hours": sub.remaining_hours,
            "total_hours": sub.total_hours,
            "has_persistent_storage": sub.has_persistent_storage,
            "storage_size_gb": sub.storage_size_gb,
            "is_unlimited": sub.is_unlimited,
            "entitled_resolutions": sub.entitled_resolutions,
        });
        if let Ok(json) = serde_json::to_string(&cache) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_subscription_cache() -> Option<SubscriptionInfo> {
    let path = subscription_cache_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    let cache: serde_json::Value = serde_json::from_str(&content).ok()?;

    Some(SubscriptionInfo {
        membership_tier: cache.get("membership_tier")?.as_str()?.to_string(),
        remaining_hours: cache.get("remaining_hours")?.as_f64()? as f32,
        total_hours: cache.get("total_hours")?.as_f64()? as f32,
        has_persistent_storage: cache.get("has_persistent_storage")?.as_bool()?,
        storage_size_gb: cache
            .get("storage_size_gb")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        is_unlimited: cache
            .get("is_unlimited")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        entitled_resolutions: cache
            .get("entitled_resolutions")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default(),
    })
}

// ============================================================
// Session Cache
// ============================================================

fn session_cache_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("session_cache.json"))
}

fn session_error_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("session_error.txt"))
}

pub fn save_session_cache(session: &SessionInfo) {
    if let Some(path) = session_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Serialize session info
        let cache = serde_json::json!({
            "session_id": session.session_id,
            "server_ip": session.server_ip,
            "zone": session.zone,
            "state": format!("{:?}", session.state),
            "gpu_type": session.gpu_type,
            "signaling_url": session.signaling_url,
            "is_ready": session.is_ready(),
            "is_queued": session.is_queued(),
            "queue_position": session.queue_position(),
            "media_connection_info": session.media_connection_info.as_ref().map(|mci| {
                serde_json::json!({
                    "ip": mci.ip,
                    "port": mci.port,
                })
            }),
            "ads_required": session.ads_required,
        });
        if let Ok(json) = serde_json::to_string(&cache) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_session_cache() -> Option<SessionInfo> {
    let path = session_cache_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    let cache: serde_json::Value = serde_json::from_str(&content).ok()?;

    let state_str = cache.get("state")?.as_str()?;
    let state = if state_str.contains("Ready") {
        SessionState::Ready
    } else if state_str.contains("Streaming") {
        SessionState::Streaming
    } else if state_str.contains("InQueue") {
        // Parse queue position and eta from state string
        let position = cache
            .get("queue_position")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        SessionState::InQueue {
            position,
            eta_secs: 0,
        }
    } else if state_str.contains("Error") {
        SessionState::Error(state_str.to_string())
    } else if state_str.contains("Launching") {
        SessionState::Launching
    } else {
        SessionState::Requesting
    };

    // Parse media_connection_info if present
    let media_connection_info = cache
        .get("media_connection_info")
        .and_then(|v| v.as_object())
        .and_then(|obj| {
            let ip = obj.get("ip")?.as_str()?.to_string();
            let port = obj.get("port")?.as_u64()? as u16;
            Some(MediaConnectionInfo { ip, port })
        });

    Some(SessionInfo {
        session_id: cache.get("session_id")?.as_str()?.to_string(),
        server_ip: cache.get("server_ip")?.as_str()?.to_string(),
        zone: cache.get("zone")?.as_str()?.to_string(),
        state,
        gpu_type: cache
            .get("gpu_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        signaling_url: cache
            .get("signaling_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        ice_servers: Vec::new(),
        media_connection_info,
        ads_required: cache
            .get("ads_required")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ads_info: None, // Ads info not persisted to cache
    })
}

pub fn clear_session_cache() {
    if let Some(path) = session_cache_path() {
        let _ = std::fs::remove_file(path);
    }
}

pub fn save_session_error(error: &str) {
    if let Some(path) = session_error_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, error);
    }
}

pub fn load_session_error() -> Option<String> {
    let path = session_error_path()?;
    std::fs::read_to_string(path).ok()
}

pub fn clear_session_error() {
    if let Some(path) = session_error_path() {
        let _ = std::fs::remove_file(path);
    }
}

// ============================================================
// Active Sessions Cache (for conflict detection)
// ============================================================

pub fn save_active_sessions_cache(sessions: &[ActiveSessionInfo]) {
    if let Some(path) = get_app_data_dir().map(|p| p.join("active_sessions.json")) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(sessions) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_active_sessions_cache() -> Option<Vec<ActiveSessionInfo>> {
    let path = get_app_data_dir()?.join("active_sessions.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn clear_active_sessions_cache() {
    if let Some(path) = get_app_data_dir().map(|p| p.join("active_sessions.json")) {
        let _ = std::fs::remove_file(path);
    }
}

// ============================================================
// Pending Game Cache
// ============================================================

pub fn save_pending_game_cache(game: &GameInfo) {
    if let Some(path) = get_app_data_dir().map(|p| p.join("pending_game.json")) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(game) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_pending_game_cache() -> Option<GameInfo> {
    let path = get_app_data_dir()?.join("pending_game.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn clear_pending_game_cache() {
    if let Some(path) = get_app_data_dir().map(|p| p.join("pending_game.json")) {
        let _ = std::fs::remove_file(path);
    }
}

// ============================================================
// Launch Proceed Flag
// ============================================================

pub fn save_launch_proceed_flag() {
    if let Some(path) = get_app_data_dir().map(|p| p.join("launch_proceed.flag")) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, "1");
    }
}

pub fn check_launch_proceed_flag() -> bool {
    if let Some(path) = get_app_data_dir().map(|p| p.join("launch_proceed.flag")) {
        if path.exists() {
            let _ = std::fs::remove_file(path);
            return true;
        }
    }
    false
}

// ============================================================
// Ping Results Cache
// ============================================================

use super::types::ServerStatus;

pub fn save_ping_results(results: &[(String, Option<u32>, ServerStatus)]) {
    if let Some(path) = get_app_data_dir().map(|p| p.join("ping_results.json")) {
        let cache: Vec<serde_json::Value> = results
            .iter()
            .map(|(id, ping, status)| {
                serde_json::json!({
                    "id": id,
                    "ping_ms": ping,
                    "status": format!("{:?}", status),
                })
            })
            .collect();

        if let Ok(json) = serde_json::to_string(&cache) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_ping_results() -> Option<Vec<serde_json::Value>> {
    let path = get_app_data_dir()?.join("ping_results.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let results: Vec<serde_json::Value> = serde_json::from_str(&content).ok()?;
    // Clear the ping file after loading
    let _ = std::fs::remove_file(&path);
    Some(results)
}

// ============================================================
// Queue Server Ping Results Cache
// ============================================================

pub fn save_queue_ping_results(results: &[(String, Option<u32>)]) {
    if let Some(path) = get_app_data_dir().map(|p| p.join("queue_ping_results.json")) {
        let cache: Vec<serde_json::Value> = results
            .iter()
            .map(|(id, ping)| {
                serde_json::json!({
                    "server_id": id,
                    "ping_ms": ping,
                })
            })
            .collect();

        if let Ok(json) = serde_json::to_string(&cache) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_queue_ping_results() -> Option<Vec<(String, Option<u32>)>> {
    let path = get_app_data_dir()?.join("queue_ping_results.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let results: Vec<serde_json::Value> = serde_json::from_str(&content).ok()?;
    // Clear the ping file after loading
    let _ = std::fs::remove_file(&path);

    let parsed: Vec<(String, Option<u32>)> = results
        .iter()
        .filter_map(|v| {
            let server_id = v.get("server_id")?.as_str()?.to_string();
            let ping_ms = v.get("ping_ms").and_then(|p| p.as_u64()).map(|p| p as u32);
            Some((server_id, ping_ms))
        })
        .collect();

    Some(parsed)
}

// ============================================================
// Popup Game Details Cache
// ============================================================

pub fn save_popup_game_details(game: &GameInfo) {
    if let Some(path) = get_app_data_dir().map(|p| p.join("popup_game.json")) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(game) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_popup_game_details() -> Option<GameInfo> {
    let path = get_app_data_dir()?.join("popup_game.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let game: GameInfo = serde_json::from_str(&content).ok()?;

    // Clear the file after loading to prevent stale data
    let _ = std::fs::remove_file(&path);

    Some(game)
}

pub fn clear_popup_game_details() {
    if let Some(path) = get_app_data_dir().map(|p| p.join("popup_game.json")) {
        let _ = std::fs::remove_file(path);
    }
}

// ============================================================
// Queue Times Cache (from PrintedWaste API)
// ============================================================

use crate::api::QueueServerInfo;

fn queue_cache_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("queue_cache.json"))
}

pub fn save_queue_cache(servers: &[QueueServerInfo]) {
    if let Some(path) = queue_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cache: Vec<serde_json::Value> = servers
            .iter()
            .map(|s| {
                serde_json::json!({
                    "server_id": s.server_id,
                    "display_name": s.display_name,
                    "region": s.region,
                    "ping_ms": s.ping_ms,
                    "queue_position": s.queue_position,
                    "eta_seconds": s.eta_seconds,
                    "is_4080_server": s.is_4080_server,
                    "is_5080_server": s.is_5080_server,
                    "last_updated": s.last_updated,
                })
            })
            .collect();

        if let Ok(json) = serde_json::to_string(&cache) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn load_queue_cache() -> Option<Vec<QueueServerInfo>> {
    let path = queue_cache_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let cache: Vec<serde_json::Value> = serde_json::from_str(&content).ok()?;

    Some(
        cache
            .into_iter()
            .filter_map(|v| {
                Some(QueueServerInfo {
                    server_id: v.get("server_id")?.as_str()?.to_string(),
                    display_name: v.get("display_name")?.as_str()?.to_string(),
                    region: v.get("region")?.as_str()?.to_string(),
                    ping_ms: v.get("ping_ms").and_then(|v| v.as_u64()).map(|v| v as u32),
                    queue_position: v.get("queue_position")?.as_i64()? as i32,
                    eta_seconds: v.get("eta_seconds").and_then(|v| v.as_i64()),
                    is_4080_server: v
                        .get("is_4080_server")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    is_5080_server: v
                        .get("is_5080_server")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    last_updated: v.get("last_updated").and_then(|v| v.as_i64()).unwrap_or(0),
                })
            })
            .collect(),
    )
}

pub fn clear_queue_cache() {
    if let Some(path) = queue_cache_path() {
        let _ = std::fs::remove_file(path);
    }
}

// ============================================================
// Welcome Shown Flag (first-time user experience)
// ============================================================

fn welcome_shown_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("welcome_shown"))
}

/// Check if the welcome popup has been shown before
pub fn has_shown_welcome() -> bool {
    welcome_shown_path().map(|p| p.exists()).unwrap_or(false)
}

/// Mark the welcome popup as shown
pub fn mark_welcome_shown() {
    if let Some(path) = welcome_shown_path() {
        // Just create an empty file as a marker
        if let Err(e) = std::fs::write(&path, "1") {
            warn!("Failed to save welcome shown flag: {}", e);
        }
    }
}

// ============================================================================
// ZNow Cache
// ============================================================================

use super::types::ZNowApp;

fn znow_apps_path() -> Option<PathBuf> {
    get_app_data_dir().map(|p| p.join("znow_apps_cache.json"))
}

/// Save ZNow apps to cache
pub fn save_znow_apps_cache(apps: &[ZNowApp]) {
    if let Some(path) = znow_apps_path() {
        match serde_json::to_string_pretty(apps) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!("Failed to save ZNow apps cache: {}", e);
                }
            }
            Err(e) => {
                warn!("Failed to serialize ZNow apps: {}", e);
            }
        }
    }
}

/// Load ZNow apps from cache
pub fn load_znow_apps_cache() -> Option<Vec<ZNowApp>> {
    let path = znow_apps_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Clear ZNow apps cache
pub fn clear_znow_apps_cache() {
    if let Some(path) = znow_apps_path() {
        let _ = std::fs::remove_file(path);
    }
}

// ============================================================
// ZNow Relay Sender Cache (in-memory, not file-based)
// ============================================================

use parking_lot::RwLock;
use tokio::sync::mpsc;
use crate::znow::relay::{OutgoingMessage, RelayEvent};

/// Global storage for the relay command sender
/// This is set by the async connect task and read by the main thread
static ZNOW_RELAY_TX: RwLock<Option<mpsc::Sender<OutgoingMessage>>> = RwLock::new(None);

/// Global storage for relay events (main thread polls these)
static ZNOW_RELAY_EVENTS: RwLock<Vec<RelayEvent>> = RwLock::new(Vec::new());

/// Store the relay sender for later use
pub fn set_znow_relay_sender(sender: mpsc::Sender<OutgoingMessage>) {
    *ZNOW_RELAY_TX.write() = Some(sender);
    info!("ZNow relay sender stored");
}

/// Take the relay sender (moves it out of the cache)
pub fn take_znow_relay_sender() -> Option<mpsc::Sender<OutgoingMessage>> {
    ZNOW_RELAY_TX.write().take()
}

/// Check if we have a relay sender cached
pub fn has_znow_relay_sender() -> bool {
    ZNOW_RELAY_TX.read().is_some()
}

/// Clear the relay sender
pub fn clear_znow_relay_sender() {
    *ZNOW_RELAY_TX.write() = None;
}

/// Push a relay event to be processed by the main thread
pub fn push_znow_relay_event(event: RelayEvent) {
    ZNOW_RELAY_EVENTS.write().push(event);
}

/// Take all pending relay events
pub fn take_znow_relay_events() -> Vec<RelayEvent> {
    std::mem::take(&mut *ZNOW_RELAY_EVENTS.write())
}
