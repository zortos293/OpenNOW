//! WebRTC Module
//!
//! WebRTC peer connection, signaling, and data channels for GFN streaming.

mod datachannel;
mod peer;
mod sdp;
mod signaling;

pub use datachannel::*;
pub use peer::{request_keyframe, NetworkStats, WebRtcEvent, WebRtcPeer};
pub use sdp::*;
pub use signaling::{GfnSignaling, IceCandidate, SignalingEvent};
// StreamingResult is defined in this module and exported automatically
use log::{debug, error, info, warn};
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::ice_transport::ice_server::RTCIceServer;

use crate::app::{SessionInfo, Settings, SharedFrame, VideoCodec};

/// Result of a streaming session - indicates why the stream ended
#[derive(Debug, Clone)]
pub enum StreamingResult {
    /// Stream ended normally (user stopped or server disconnected gracefully)
    Normal,
    /// Stream failed due to an error
    Error(String),
    /// Stream was interrupted by SSRC change (resolution change on server)
    /// Contains the stall duration in milliseconds before detection
    SsrcChangeDetected { stall_duration_ms: u64 },
}
use crate::input::{ControllerManager, FfbEffectType, G29FfbManager, InputHandler, WheelManager};
use crate::media::{
    AudioDecoder, AudioPlayer, DepacketizerCodec, RtpDepacketizer, StreamStats, UnifiedVideoDecoder,
};

/// Active streaming session
pub struct StreamingSession {
    pub signaling: Option<GfnSignaling>,
    pub peer: Option<WebRtcPeer>,
    pub connected: bool,
    pub stats: StreamStats,
    pub input_ready: bool,
}

impl StreamingSession {
    pub fn new() -> Self {
        Self {
            signaling: None,
            peer: None,
            connected: false,
            stats: StreamStats::default(),
            input_ready: false,
        }
    }
}

impl Default for StreamingSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Build nvstSdp string with streaming parameters
/// Based on official GFN browser client format
fn build_nvst_sdp(
    ice_ufrag: &str,
    ice_pwd: &str,
    fingerprint: &str,
    width: u32,
    height: u32,
    fps: u32,
    max_bitrate_kbps: u32,
) -> String {
    let min_bitrate_kbps = std::cmp::min(10000, max_bitrate_kbps / 10);
    let initial_bitrate_kbps = max_bitrate_kbps / 2;

    let is_high_fps = fps >= 120;
    let is_120_fps = fps == 120;
    let is_240_fps = fps >= 240;

    let mut lines = vec![
        "v=0".to_string(),
        "o=SdpTest test_id_13 14 IN IPv4 127.0.0.1".to_string(),
        "s=-".to_string(),
        "t=0 0".to_string(),
        format!("a=general.icePassword:{}", ice_pwd),
        format!("a=general.iceUserNameFragment:{}", ice_ufrag),
        format!("a=general.dtlsFingerprint:{}", fingerprint),
        "m=video 0 RTP/AVP".to_string(),
        "a=msid:fbc-video-0".to_string(),
        // FEC settings
        "a=vqos.fec.rateDropWindow:10".to_string(),
        "a=vqos.fec.minRequiredFecPackets:2".to_string(),
        "a=vqos.fec.repairMinPercent:5".to_string(),
        "a=vqos.fec.repairPercent:5".to_string(),
        "a=vqos.fec.repairMaxPercent:35".to_string(),
    ];

    // DRC/DFC settings based on FPS
    // Always disable DRC to allow full bitrate
    lines.push("a=vqos.drc.enable:0".to_string());
    if is_high_fps {
        lines.push("a=vqos.dfc.enable:1".to_string());
        lines.push("a=vqos.dfc.decodeFpsAdjPercent:85".to_string());
        lines.push("a=vqos.dfc.targetDownCooldownMs:250".to_string());
        lines.push("a=vqos.dfc.dfcAlgoVersion:2".to_string());
        lines.push(format!(
            "a=vqos.dfc.minTargetFps:{}",
            if is_120_fps { 100 } else { 60 }
        ));
    }

    // Video encoder settings
    lines.extend(vec![
        "a=video.dx9EnableNv12:1".to_string(),
        "a=video.dx9EnableHdr:1".to_string(),
        "a=vqos.qpg.enable:1".to_string(),
        "a=vqos.resControl.qp.qpg.featureSetting:7".to_string(),
        "a=bwe.useOwdCongestionControl:1".to_string(),
        "a=video.enableRtpNack:1".to_string(),
        "a=vqos.bw.txRxLag.minFeedbackTxDeltaMs:200".to_string(),
        "a=vqos.drc.bitrateIirFilterFactor:18".to_string(),
        "a=video.packetSize:1140".to_string(),
        "a=packetPacing.minNumPacketsPerGroup:15".to_string(),
    ]);

    // High FPS optimizations
    if is_high_fps {
        lines.extend(vec![
            "a=bwe.iirFilterFactor:8".to_string(),
            "a=video.encoderFeatureSetting:47".to_string(),
            "a=video.encoderPreset:6".to_string(),
            "a=vqos.resControl.cpmRtc.badNwSkipFramesCount:600".to_string(),
            "a=vqos.resControl.cpmRtc.decodeTimeThresholdMs:9".to_string(),
            format!(
                "a=video.fbcDynamicFpsGrabTimeoutMs:{}",
                if is_120_fps { 6 } else { 18 }
            ),
            format!(
                "a=vqos.resControl.cpmRtc.serverResolutionUpdateCoolDownCount:{}",
                if is_120_fps { 6000 } else { 12000 }
            ),
        ]);
    }

    // 240+ FPS optimizations
    if is_240_fps {
        lines.extend(vec![
            "a=video.enableNextCaptureMode:1".to_string(),
            "a=vqos.maxStreamFpsEstimate:240".to_string(),
            "a=video.videoSplitEncodeStripsPerFrame:3".to_string(),
            "a=video.updateSplitEncodeStateDynamically:1".to_string(),
        ]);
    }

    // Out of focus and additional settings
    lines.extend(vec![
        "a=vqos.adjustStreamingFpsDuringOutOfFocus:1".to_string(),
        "a=vqos.resControl.cpmRtc.ignoreOutOfFocusWindowState:1".to_string(),
        "a=vqos.resControl.perfHistory.rtcIgnoreOutOfFocusWindowState:1".to_string(),
        // Disable CPM-based resolution changes (prevents SSRC switches)
        // featureMask: 0 = disable all CPM features, 3 = enable some
        "a=vqos.resControl.cpmRtc.featureMask:0".to_string(),
        // Disable resolution scaling entirely
        "a=vqos.resControl.cpmRtc.enable:0".to_string(),
        // Never scale down resolution
        "a=vqos.resControl.cpmRtc.minResolutionPercent:100".to_string(),
        // Infinite cooldown to prevent resolution changes
        "a=vqos.resControl.cpmRtc.resolutionChangeHoldonMs:999999".to_string(),
        format!(
            "a=packetPacing.numGroups:{}",
            if is_120_fps { 3 } else { 5 }
        ),
        "a=packetPacing.maxDelayUs:1000".to_string(),
        "a=packetPacing.minNumPacketsFrame:10".to_string(),
        // NACK settings
        "a=video.rtpNackQueueLength:1024".to_string(),
        "a=video.rtpNackQueueMaxPackets:512".to_string(),
        "a=video.rtpNackMaxPacketCount:25".to_string(),
        // Resolution/quality
        "a=vqos.drc.qpMaxResThresholdAdj:4".to_string(),
        "a=vqos.grc.qpMaxResThresholdAdj:4".to_string(),
        "a=vqos.drc.iirFilterFactor:100".to_string(),
        // Viewport and FPS
        format!("a=video.clientViewportWd:{}", width),
        format!("a=video.clientViewportHt:{}", height),
        format!("a=video.maxFPS:{}", fps),
        // Bitrate - critical for achieving high bitrates
        // Initial bitrate should be high to avoid slow ramp-up
        format!("a=video.initialBitrateKbps:{}", max_bitrate_kbps * 3 / 4), // Start at 75% of max
        format!("a=video.initialPeakBitrateKbps:{}", max_bitrate_kbps),
        format!("a=vqos.bw.maximumBitrateKbps:{}", max_bitrate_kbps),
        format!("a=vqos.bw.minimumBitrateKbps:{}", min_bitrate_kbps),
        // Peak bitrate settings - these are critical for allowing bitrate above 100Mbps
        format!("a=vqos.bw.peakBitrateKbps:{}", max_bitrate_kbps),
        format!("a=vqos.bw.serverPeakBitrateKbps:{}", max_bitrate_kbps),
        // Bandwidth estimation settings - disable conservative limiting
        "a=vqos.bw.enableBandwidthEstimation:1".to_string(),
        "a=vqos.bw.disableBitrateLimit:1".to_string(),
        // GRC (Global Rate Control) settings - allow full bitrate
        format!("a=vqos.grc.maximumBitrateKbps:{}", max_bitrate_kbps),
        "a=vqos.grc.enable:0".to_string(), // Disable GRC limiting
        // Encoder settings
        "a=video.maxNumReferenceFrames:4".to_string(),
        "a=video.mapRtpTimestampsToFrames:1".to_string(),
        "a=video.encoderCscMode:3".to_string(),
        "a=video.scalingFeature1:0".to_string(),
        "a=video.prefilterParams.prefilterModel:0".to_string(),
        // Audio track
        "m=audio 0 RTP/AVP".to_string(),
        "a=msid:audio".to_string(),
        // Mic track
        "m=mic 0 RTP/AVP".to_string(),
        "a=msid:mic".to_string(),
        // Input/application track
        "m=application 0 RTP/AVP".to_string(),
        "a=msid:input_1".to_string(),
        "a=ri.partialReliableThresholdMs:300".to_string(),
        "".to_string(),
    ]);

    lines.join("\n")
}

/// Extract ICE credentials from SDP
fn extract_ice_credentials(sdp: &str) -> (String, String, String) {
    let ufrag = sdp
        .lines()
        .find(|l| l.starts_with("a=ice-ufrag:"))
        .map(|l| l.trim_start_matches("a=ice-ufrag:").to_string())
        .unwrap_or_default();

    let pwd = sdp
        .lines()
        .find(|l| l.starts_with("a=ice-pwd:"))
        .map(|l| l.trim_start_matches("a=ice-pwd:").to_string())
        .unwrap_or_default();

    let fingerprint = sdp
        .lines()
        .find(|l| l.starts_with("a=fingerprint:sha-256 "))
        .map(|l| l.trim_start_matches("a=fingerprint:sha-256 ").to_string())
        .unwrap_or_default();

    (ufrag, pwd, fingerprint)
}

/// Extract public IP from server hostname (e.g., "95-178-87-234.zai..." -> "95.178.87.234")
/// Also handles direct IP strings or IPs embedded in signaling URLs
fn extract_public_ip(input: &str) -> Option<String> {
    // Check for standard IP-like patterns with dashes (e.g. 80-250-97-38)
    let re_dash = regex::Regex::new(r"(\d{1,3})-(\d{1,3})-(\d{1,3})-(\d{1,3})").ok()?;
    if let Some(captures) = re_dash.captures(input) {
        let ip = format!(
            "{}.{}.{}.{}",
            &captures[1], &captures[2], &captures[3], &captures[4]
        );
        return Some(ip);
    }

    // Check for standard IP patterns (e.g. 80.250.97.38)
    let re_dot = regex::Regex::new(r"(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})").ok()?;
    if let Some(captures) = re_dot.captures(input) {
        return Some(captures[0].to_string());
    }

    None
}

/// Run the streaming session
/// Returns a `StreamingResult` indicating how/why the session ended
pub async fn run_streaming(
    session_info: SessionInfo,
    settings: Settings,
    shared_frame: Arc<SharedFrame>,
    stats_tx: mpsc::Sender<StreamStats>,
    input_handler: Arc<InputHandler>,
) -> StreamingResult {
    info!(
        "Starting streaming to {} with session {}",
        session_info.server_ip, session_info.session_id
    );

    let (width, height) = settings.resolution_tuple();
    let fps = settings.fps;
    let max_bitrate = settings.max_bitrate_kbps();
    let codec = settings.codec;
    let _codec_str = codec.as_str().to_string();

    // Create signaling client
    let (sig_event_tx, mut sig_event_rx) = mpsc::channel::<SignalingEvent>(64);
    let server_ip = session_info
        .signaling_url
        .as_ref()
        .and_then(|url| url.split("://").nth(1).and_then(|s| s.split('/').next()))
        .unwrap_or(&session_info.server_ip)
        .to_string();

    let mut signaling = GfnSignaling::new(
        server_ip.clone(),
        session_info.session_id.clone(),
        sig_event_tx,
    );

    // Connect to signaling
    if let Err(e) = signaling.connect().await {
        return StreamingResult::Error(format!("Failed to connect signaling: {}", e));
    }
    info!("Signaling connected");

    // Create WebRTC peer
    let (peer_event_tx, mut peer_event_rx) = mpsc::channel(64);
    let mut peer = WebRtcPeer::new(peer_event_tx);

    // Video decoder - use async mode for non-blocking decode
    // Decoded frames are written directly to SharedFrame by the decoder thread
    // Uses UnifiedVideoDecoder to support both FFmpeg and native DXVA backends
    let (mut video_decoder, mut decode_stats_rx) =
        match UnifiedVideoDecoder::new_async(codec, settings.decoder_backend, shared_frame.clone())
        {
            Ok(decoder) => decoder,
            Err(e) => {
                return StreamingResult::Error(format!("Failed to create video decoder: {}", e))
            }
        };

    // Create RTP depacketizer with correct codec
    let depacketizer_codec = match codec {
        VideoCodec::H264 => DepacketizerCodec::H264,
        VideoCodec::H265 => DepacketizerCodec::H265,
        VideoCodec::AV1 => DepacketizerCodec::AV1,
    };
    let mut rtp_depacketizer = RtpDepacketizer::with_codec(depacketizer_codec);
    info!("RTP depacketizer using {:?} mode", depacketizer_codec);

    let mut audio_decoder = match AudioDecoder::new(48000, 2) {
        Ok(decoder) => decoder,
        Err(e) => return StreamingResult::Error(format!("Failed to create audio decoder: {}", e)),
    };

    // Get the sample receiver from the decoder for async operation
    let audio_sample_rx = audio_decoder.take_sample_receiver();

    // Audio player thread - receives decoded samples and plays them
    // Uses larger jitter buffer (150ms) to handle network timing variations
    std::thread::spawn(move || {
        if let Ok(audio_player) = AudioPlayer::new(48000, 2) {
            info!("Audio player thread started (async mode with jitter buffer)");
            if let Some(mut rx) = audio_sample_rx {
                let mut total_samples: u64 = 0;
                let mut log_interval = std::time::Instant::now();
                while let Some(samples) = rx.blocking_recv() {
                    if total_samples == 0 {
                        info!(
                            "First audio samples received by player: {} samples",
                            samples.len()
                        );
                    }
                    total_samples += samples.len() as u64;
                    audio_player.push_samples(&samples);

                    // Log buffer status periodically
                    if log_interval.elapsed().as_secs() >= 5 {
                        info!(
                            "Audio: {} total samples played, buffer level: {} samples",
                            total_samples,
                            audio_player.buffer_available()
                        );
                        log_interval = std::time::Instant::now();
                    }
                }
            }
        } else {
            warn!("Failed to create audio player - audio disabled");
        }
    });

    // Stats tracking
    let mut stats = StreamStats::default();
    let mut last_stats_time = std::time::Instant::now();
    let mut frames_received: u64 = 0;
    let mut frames_decoded: u64 = 0;
    let frames_dropped: u64 = 0;
    let mut bytes_received: u64 = 0;
    let mut last_frames_decoded: u64 = 0; // For actual FPS calculation

    // Pipeline latency tracking (receive to decode complete)
    let mut pipeline_latency_sum: f64 = 0.0;
    let mut pipeline_latency_count: u64 = 0;

    // Input latency tracking (event creation to transmission)
    let mut input_latency_sum: f64 = 0.0;
    let mut input_latency_count: u64 = 0;

    // Input rate tracking (events per second)
    let mut input_events_this_period: u64 = 0;

    // Input state - use atomic for cross-task communication
    // input_ready_flag and input_protocol_version_shared are created later with the input task

    // Input channel - connect InputHandler to the streaming loop
    // Reduced buffer (32) to prevent latency buildup (buffer bloat)
    // If consumer is slow, we want to conflate events, not buffer them
    let (input_event_tx, input_event_rx) = mpsc::channel::<InputEvent>(32);
    input_handler.set_event_sender(input_event_tx.clone());

    // Also set raw input sender for direct mouse events (Windows/macOS)
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    crate::input::set_raw_input_sender(input_event_tx.clone());

    info!("Input handler connected to streaming loop");

    // Initialize and start ControllerManager (gamepads via gilrs)
    let controller_manager = Arc::new(ControllerManager::new());
    controller_manager.set_event_sender(input_event_tx.clone());
    controller_manager.start();
    info!("Controller manager started");

    // Initialize and start WheelManager (racing wheels via Windows.Gaming.Input)
    // WheelManager detects dedicated racing wheels and provides proper axis separation
    // (wheel rotation, throttle, brake, clutch, handbrake) mapped to gamepad format
    let wheel_manager = Arc::new(WheelManager::new());
    wheel_manager.set_event_sender(input_event_tx.clone());
    wheel_manager.start();
    if wheel_manager.has_wheels() {
        info!("Racing wheel manager started - wheel input active");
        // Initialize force feedback for detected wheels
        for i in 0..wheel_manager.wheel_count() {
            if wheel_manager.init_force_feedback(i) {
                info!("Force feedback initialized for wheel {}", i);
            }
        }
    } else {
        info!("No racing wheels detected - wheels will be handled as gamepads via gilrs");
    }

    // Initialize G29 force feedback manager (HID-based, works in PS3 mode)
    // This provides FFB support for Logitech G29 wheels that aren't detected by Windows.Gaming.Input
    let g29_ffb = Arc::new(G29FfbManager::new());
    let g29_connected = g29_ffb.init();
    if g29_connected {
        info!("G29 force feedback initialized via HID");
    }

    // Output decoder for force feedback / rumble messages from server
    let mut output_decoder = OutputDecoder::new();

    // Channel for input task to send encoded packets to the WebRTC peer
    // This decouples input processing from video decoding completely
    // Tuple: (encoded_data, is_mouse, is_controller, latency_us)
    let (input_packet_tx, mut input_packet_rx) = mpsc::channel::<(Vec<u8>, bool, bool, u64)>(1024);

    // Stats interval timer (must be created OUTSIDE the loop to persist across iterations)
    let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(1));

    // Spawn dedicated input processing task - completely decoupled from video/signaling
    // This ensures mouse/keyboard events are processed immediately without being blocked
    // by video decoding or network operations
    let input_packet_tx_clone = input_packet_tx.clone();
    let input_ready_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let input_ready_flag_clone = input_ready_flag.clone();
    let input_protocol_version_shared = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let input_protocol_version_clone = input_protocol_version_shared.clone();

    tokio::spawn(async move {
        let mut input_encoder = InputEncoder::new();
        let mut input_event_rx = input_event_rx;

        loop {
            match input_event_rx.recv().await {
                Some(event) => {
                    // Only process if input is ready (handshake complete)
                    if !input_ready_flag_clone.load(std::sync::atomic::Ordering::Acquire) {
                        continue;
                    }

                    // Update encoder protocol version if changed
                    let version =
                        input_protocol_version_clone.load(std::sync::atomic::Ordering::Relaxed);
                    input_encoder.set_protocol_version(version);

                    // Extract event timestamp for latency calculation
                    let event_timestamp_us = match &event {
                        InputEvent::KeyDown { timestamp_us, .. }
                        | InputEvent::KeyUp { timestamp_us, .. }
                        | InputEvent::MouseMove { timestamp_us, .. }
                        | InputEvent::MouseButtonDown { timestamp_us, .. }
                        | InputEvent::MouseButtonUp { timestamp_us, .. }
                        | InputEvent::MouseWheel { timestamp_us, .. }
                        | InputEvent::Gamepad { timestamp_us, .. } => *timestamp_us,
                        InputEvent::Heartbeat => 0,
                    };

                    // Calculate input latency (time from event creation to now)
                    let now_us = crate::input::get_timestamp_us();
                    let latency_us = now_us.saturating_sub(event_timestamp_us);

                    // Encode the event
                    let encoded = input_encoder.encode(&event);

                    // Determine if this is a mouse event
                    let is_mouse = matches!(
                        &event,
                        InputEvent::MouseMove { .. }
                            | InputEvent::MouseButtonDown { .. }
                            | InputEvent::MouseButtonUp { .. }
                            | InputEvent::MouseWheel { .. }
                    );

                    // Determine if this is a gamepad/controller event
                    let is_controller = matches!(&event, InputEvent::Gamepad { .. });

                    // Send to main loop for WebRTC transmission
                    // Use try_send to never block the input thread
                    if input_packet_tx_clone
                        .try_send((encoded, is_mouse, is_controller, latency_us))
                        .is_err()
                    {
                        // Channel full - this is fine, old packets can be dropped for mouse
                    }
                }
                None => {
                    // Channel closed, exit task
                    break;
                }
            }
        }
        debug!("Input processing task ended");
    });

    // Main event loop - no longer processes input directly
    loop {
        tokio::select! {
            // Process encoded input packets from the input task (high priority)
            biased;

            Some((encoded, is_mouse, is_controller, latency_us)) = input_packet_rx.recv() => {
                // Track input latency and count for stats
                input_events_this_period += 1;
                if latency_us > 0 {
                    input_latency_sum += latency_us as f64;
                    input_latency_count += 1;
                }

                // Log first few mouse events to verify flow
                static MOUSE_LOG_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if is_mouse {
                    let count = MOUSE_LOG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if count < 10 {
                        info!("Sending mouse #{}: {} bytes, latency {}us", count, encoded.len(), latency_us);
                    }
                }

                if is_controller {
                    // Controller input (Input Channel V1)
                    // "input_channel_v1 needs to be only controller"
                    let _ = peer.send_controller_input(&encoded).await;
                } else if is_mouse {
                    // Log if mouse channel is ready
                    static MOUSE_READY_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if !MOUSE_READY_LOGGED.load(std::sync::atomic::Ordering::Relaxed) {
                        info!("Mouse events will use PARTIALLY RELIABLE channel (lower latency)");
                        MOUSE_READY_LOGGED.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Send mouse on PARTIALLY RELIABLE channel (unordered, low latency)
                    let _ = peer.send_mouse_input(&encoded).await;
                } else {
                    // Keyboard events
                    // Currently uses send_input (V1)
                    // If user requires V1 to be *strictly* controller, we might need to route keyboard elsewhere?
                    // But usually keyboard shares reliable channel. For now, keep it here.
                    let _ = peer.send_input(&encoded).await;
                }
            }
            Some(event) = sig_event_rx.recv() => {
                match event {
                    SignalingEvent::SdpOffer(sdp) => {
                        info!("Received SDP offer, length: {}", sdp.len());

                        // Detect codec to use
                        let codec = match settings.codec {
                            VideoCodec::H264 => "H264",
                            VideoCodec::H265 => "H265",
                            VideoCodec::AV1 => "AV1",
                        };

                        info!("Preferred codec: {}", codec);

                        // Use media_connection_info IP first, then server_ip
                        let public_ip = session_info.media_connection_info.as_ref()
                            .and_then(|mci| extract_public_ip(&mci.ip))
                            .or_else(|| extract_public_ip(&session_info.server_ip));

                        // Modify SDP with extracted IP
                        let modified_sdp = if let Some(ref ip) = public_ip {
                            fix_server_ip(&sdp, ip)
                        } else {
                            sdp.clone()
                        };

                        // CRITICAL: Inject provisional SSRCs (2, 3, 4) for video
                        // GFN server uses sequential SSRCs when resolution changes, but
                        // webrtc-rs can't handle undeclared SSRCs without MID extensions.
                        // This is based on reverse-engineering of official GFN client (Bifrost2.dll).
                        let modified_sdp = inject_provisional_ssrcs(&modified_sdp);

                        // Prefer codec
                        let modified_sdp = prefer_codec(&modified_sdp, &settings.codec);

                        // CRITICAL: Create input channel BEFORE SDP negotiation (per GFN protocol)
                        info!("Creating input channel BEFORE SDP negotiation...");

                        // Align with official client: Use ICE servers from SessionInfo (TURN/STUN)
                        // This corresponds to `iceServerConfiguration` in the CloudMatch response.
                        let mut ice_servers = Vec::new();

                        // Convert SessionInfo ICE servers to RTCIceServer
                        for server in &session_info.ice_servers {
                            let mut s = RTCIceServer {
                                urls: server.urls.clone(),
                                ..Default::default()
                            };
                            if let Some(user) = &server.username {
                                s.username = user.clone();
                            }
                            if let Some(cred) = &server.credential {
                                s.credential = cred.clone();
                            }
                            ice_servers.push(s);
                        }

                        // Always add default STUN servers as fallback (Alliance robustness)
                        ice_servers.push(RTCIceServer {
                            urls: vec!["stun:s1.stun.gamestream.nvidia.com:19308".to_string()],
                            ..Default::default()
                        });
                        ice_servers.push(RTCIceServer {
                            urls: vec![
                                "stun:stun.l.google.com:19302".to_string(),
                                "stun:stun1.l.google.com:19302".to_string()
                            ],
                            ..Default::default()
                        });

                        if ice_servers.len() <= 2 {
                             info!("Using default/fallback ICE servers only");
                        } else {
                            info!("Using {} ICE servers (session + fallback)", ice_servers.len());
                        }

                        // Handle offer and create answer
                        match peer.handle_offer(&modified_sdp, ice_servers).await {
                            Ok(answer_sdp) => {
                                // Create input channel
                                if let Err(e) = peer.create_input_channel().await {
                                    warn!("Failed to create input channel: {}", e);
                                }

                                // Extract ICE credentials from our answer
                                let (ufrag, pwd, fingerprint) = extract_ice_credentials(&answer_sdp);

                                // Build rich GFN-specific SDP (nvstSdp)
                                let nvst_sdp_content = build_nvst_sdp(
                                    &ufrag, &pwd, &fingerprint,
                                    width, height, fps, max_bitrate
                                );
                                info!("Generated nvstSdp, length: {}", nvst_sdp_content.len());

                                // Use raw nvstSdp string (no wrapper object)
                                if let Err(e) = signaling.send_answer(&answer_sdp, Some(&nvst_sdp_content)).await {
                                    error!("Failed to send SDP answer: {}", e);
                                }

                                // For resume flow or Alliance partners (manual candidate needed)
                                if let Some(ref mci) = session_info.media_connection_info {
                                    info!("Using media port {} from session API", mci.port);

                                    // EXTRACT RAW IP from hostname (needed for valid ICE candidate)
                                    // Use extract_public_ip which handles "x-x-x-x" format or direct IP
                                    let raw_ip = extract_public_ip(&mci.ip)
                                        .or_else(|| {
                                            // Fallback: try to resolve hostname
                                            use std::net::ToSocketAddrs;
                                            format!("{}:{}", mci.ip, mci.port)
                                                .to_socket_addrs()
                                                .ok()?
                                                .next()
                                                .map(|addr| addr.ip().to_string())
                                        })
                                        .unwrap_or_else(|| mci.ip.clone());

                                    let candidate = format!(
                                        "candidate:1 1 udp 2130706431 {} {} typ host",
                                        raw_ip, mci.port
                                    );
                                    info!("Adding manual ICE candidate: {}", candidate);

                                    // Extract server ufrag from offer (needed for ice-lite)
                                    let (server_ufrag, _, _) = extract_ice_credentials(&sdp);

                                    if let Err(e) = peer.add_ice_candidate(&candidate, Some("0"), Some(0), Some(server_ufrag.clone())).await {
                                        warn!("Failed to add manual ICE candidate: {}", e);
                                        // Try other mids just in case
                                        for mid in ["1", "2", "3"] {
                                            if peer.add_ice_candidate(&candidate, Some(mid), Some(mid.parse().unwrap_or(0)), Some(server_ufrag.clone())).await.is_ok() {
                                                info!("Added ICE candidate with sdpMid={}", mid);
                                                break;
                                            }
                                        }
                                    }
                                } else {
                                    info!("No media_connection_info - waiting for ICE negotiation");
                                }

                                // Update stats with codec info
                                stats.codec = codec.to_string();
                                stats.resolution = format!("{}x{}", width, height);
                                stats.target_fps = fps;
                            }

                            Err(e) => {
                                error!("Failed to handle offer: {}", e);
                            }
                        }
                    }
                    SignalingEvent::IceCandidate(candidate) => {
                        info!("Received trickle ICE candidate");
                        if let Err(e) = peer.add_ice_candidate(
                            &candidate.candidate,
                            candidate.sdp_mid.as_deref(),
                            candidate.sdp_mline_index.map(|i| i as u16),
                            None
                        ).await {
                            warn!("Failed to add ICE candidate: {}", e);
                        }
                    }
                    SignalingEvent::Connected => {
                        info!("Signaling connected event");
                    }
                    SignalingEvent::Disconnected(reason) => {
                        info!("Signaling disconnected: {}", reason);
                        break;
                    }
                    SignalingEvent::Error(e) => {
                        error!("Signaling error: {}", e);
                        break;
                    }
                }
            }
            Some(event) = peer_event_rx.recv() => {
                match event {
                    WebRtcEvent::Connected => {
                        info!("=== WebRTC CONNECTED ===");
                        stats.gpu_type = session_info.gpu_type.clone().unwrap_or_default();
                    }
                    WebRtcEvent::Disconnected => {
                        warn!("WebRTC disconnected");
                        break;
                    }
                    WebRtcEvent::VideoFrame { payload, rtp_timestamp: _, marker } => {
                        frames_received += 1;
                        bytes_received += payload.len() as u64;
                        let packet_receive_time = std::time::Instant::now();

                        // Only log first packet
                        if frames_received == 1 {
                            info!("First video RTP packet received: {} bytes", payload.len());
                        }

                        // Handle codec-specific depacketization
                        match depacketizer_codec {
                            DepacketizerCodec::AV1 => {
                                // AV1: Use specialized OBU accumulation
                                rtp_depacketizer.process_av1_raw(&payload);

                                // On marker bit, flush pending OBU and get complete frame
                                if marker {
                                    rtp_depacketizer.flush_pending_obu();
                                    if let Some(frame_data) = rtp_depacketizer.take_accumulated_frame() {
                                        if let Err(e) = video_decoder.decode_async(&frame_data, packet_receive_time) {
                                            warn!("AV1 decode async failed: {}", e);
                                        }
                                    }
                                }
                            }
                            DepacketizerCodec::H264 | DepacketizerCodec::H265 => {
                                // H.264/H.265: depacketize RTP and accumulate NAL units
                                let nal_units = rtp_depacketizer.process(&payload);

                                // Accumulate NAL units until marker bit (end of frame)
                                // Each frame consists of multiple NAL units that must be sent together
                                for nal_unit in nal_units {
                                    rtp_depacketizer.accumulate_nal(nal_unit);
                                }

                                // On marker bit, we have a complete Access Unit - send to decoder
                                if marker {
                                    if let Some(frame_data) = rtp_depacketizer.take_nal_frame() {
                                        if let Err(e) = video_decoder.decode_async(&frame_data, packet_receive_time) {
                                            warn!("Decode async failed: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    WebRtcEvent::AudioFrame(rtp_data) => {
                        // Async decode - non-blocking, samples go directly to audio player
                        static AUDIO_PACKET_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        let count = AUDIO_PACKET_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if count == 0 {
                            info!("First audio packet received: {} bytes", rtp_data.len());
                        } else if count % 500 == 0 {
                            debug!("Audio packets received: {}", count);
                        }
                        audio_decoder.decode_async(&rtp_data);
                    }
                    WebRtcEvent::DataChannelOpen(label) => {
                        info!("Data channel opened: {}", label);
                        if label.contains("input") {
                            info!("Input channel ready, waiting for handshake...");
                        }
                    }
                    WebRtcEvent::DataChannelMessage(label, data) => {
                        debug!("Data channel '{}' message: {} bytes", label, data.len());

                        // Try to decode as output event (force feedback / rumble)
                        if let Some(output_event) = output_decoder.decode(&data) {
                            match output_event {
                                OutputEvent::Rumble {
                                    controller_id,
                                    left_motor,
                                    right_motor,
                                    duration_ms,
                                } => {
                                    debug!(
                                        "Rumble event: controller={}, left={}, right={}, duration={}ms",
                                        controller_id, left_motor, right_motor, duration_ms
                                    );
                                    // Queue rumble effect on the controller
                                    controller_manager.queue_rumble(
                                        controller_id,
                                        left_motor,
                                        right_motor,
                                        duration_ms,
                                    );
                                }
                                OutputEvent::ForceFeedback {
                                    wheel_id,
                                    effect_type,
                                    magnitude,
                                    duration_ms,
                                    ..
                                } => {
                                    debug!(
                                        "FFB event: wheel={}, type={}, magnitude={}, duration={}ms",
                                        wheel_id, effect_type, magnitude, duration_ms
                                    );
                                    // Convert magnitude from i16 (-32768 to 32767) to f64 (-1.0 to 1.0)
                                    let mag_normalized = magnitude as f64 / 32767.0;

                                    // Try Windows.Gaming.Input first (for wheels that support it)
                                    if wheel_manager.has_wheels() {
                                        wheel_manager.apply_force_feedback(
                                            wheel_id as usize,
                                            FfbEffectType::from(effect_type),
                                            mag_normalized,
                                            duration_ms,
                                        );
                                    } else if g29_ffb.is_connected() {
                                        // Fallback to G29 HID-based FFB
                                        g29_ffb.apply_constant_force(mag_normalized);
                                    }
                                }
                                OutputEvent::Unknown { .. } => {
                                    // Should not happen - decoder only returns Rumble/ForceFeedback
                                }
                            }
                        }

                        // Handle input handshake
                        if data.len() >= 2 {
                            let first_word = u16::from_le_bytes([data[0], data.get(1).copied().unwrap_or(0)]);
                            let mut protocol_version: u16 = 0;

                            if first_word == 526 {
                                // New format: 0x020E (526 LE)
                                protocol_version = data.get(2..4)
                                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                                    .unwrap_or(0);
                                info!("Input handshake (new format), version={}", protocol_version);
                            } else if data[0] == 0x0e {
                                // Old format
                                protocol_version = first_word;
                                info!("Input handshake (old format), version={}", protocol_version);
                            }

                            // Echo handshake response
                            let is_ready = input_ready_flag.load(std::sync::atomic::Ordering::Acquire);
                            if !is_ready && (first_word == 526 || data[0] == 0x0e) {
                                if let Err(e) = peer.send_input(&data).await {
                                    error!("Failed to send handshake response: {}", e);
                                } else {
                                    info!("Sent handshake response, input is ready! Protocol version: {}", protocol_version);

                                    // Update shared protocol version for input task
                                    input_protocol_version_shared.store(protocol_version as u8, std::sync::atomic::Ordering::Release);

                                    // Update output decoder protocol version too
                                    output_decoder.set_protocol_version(protocol_version as u8);

                                    // Signal input task that handshake is complete
                                    input_ready_flag.store(true, std::sync::atomic::Ordering::Release);

                                    info!("Input encoder protocol version set to {}", protocol_version);
                                }
                            }
                        }
                    }
                    WebRtcEvent::IceCandidate(candidate, sdp_mid, sdp_mline_index) => {
                        // Send our ICE candidate to server
                        if let Err(e) = signaling.send_ice_candidate(
                            &candidate,
                            sdp_mid.as_deref(),
                            sdp_mline_index.map(|i| i as u32),
                        ).await {
                            warn!("Failed to send ICE candidate: {}", e);
                        }
                    }
                    WebRtcEvent::Error(e) => {
                        error!("WebRTC error: {}", e);
                    }
                    WebRtcEvent::SsrcChangeDetected { stall_duration_ms } => {
                        // SSRC change detected - the server switched video streams
                        // This is a known limitation of webrtc-rs when handling mid-stream SSRC changes
                        // without MID header extensions (which GFN doesn't send).
                        error!(
                            "SSRC change detected after {}ms stall. Initiating auto-reconnect...",
                            stall_duration_ms
                        );

                        // Stop input managers before returning
                        controller_manager.stop();
                        wheel_manager.stop();
                        g29_ffb.stop();

                        // Clear raw input sender
                        #[cfg(any(target_os = "windows", target_os = "macos"))]
                        crate::input::clear_raw_input_sender();

                        // Return SSRC change result to trigger reconnection
                        return StreamingResult::SsrcChangeDetected { stall_duration_ms };
                    }
                }
            }
            // Receive decode stats from the decoder thread (non-blocking)
            Some(decode_stat) = decode_stats_rx.recv() => {
                if decode_stat.frame_produced {
                    frames_decoded += 1;

                    // Track decode latency
                    stats.decode_time_ms = decode_stat.decode_time_ms;
                    pipeline_latency_sum += decode_stat.decode_time_ms as f64;
                    pipeline_latency_count += 1;
                    stats.latency_ms = (pipeline_latency_sum / pipeline_latency_count as f64) as f32;

                    // Log first decoded frame
                    if frames_decoded == 1 {
                        info!("First frame decoded (async) in {:.1}ms", decode_stat.decode_time_ms);
                    }
                }

                // Request keyframe if decoder is failing
                if decode_stat.needs_keyframe {
                    // Reset depacketizer state to clear any corrupted fragment state
                    // This is critical for recovering from packet loss/corruption
                    rtp_depacketizer.reset_state();
                    request_keyframe().await;
                }
            }
            // Update stats periodically (interval persists across loop iterations)
            _ = stats_interval.tick() => {
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(last_stats_time).as_secs_f64();

                // Calculate actual FPS from decoded frames
                let frames_this_period = frames_decoded - last_frames_decoded;
                stats.fps = (frames_this_period as f64 / elapsed) as f32;
                last_frames_decoded = frames_decoded;

                // Calculate bitrate
                stats.bitrate_mbps = ((bytes_received as f64 * 8.0) / (elapsed * 1_000_000.0)) as f32;
                stats.frames_received = frames_received;
                stats.frames_decoded = frames_decoded;
                stats.frames_dropped = frames_dropped;

                // Calculate average input latency (microseconds to milliseconds)
                if input_latency_count > 0 {
                    stats.input_latency_ms = (input_latency_sum / input_latency_count as f64 / 1000.0) as f32;
                    // Reset for next period
                    input_latency_sum = 0.0;
                    input_latency_count = 0;
                }

                // Calculate input rate (events per second)
                stats.input_rate = (input_events_this_period as f64 / elapsed) as f32;
                input_events_this_period = 0;

                // Calculate frame delivery latency (pipeline latency)
                if pipeline_latency_count > 0 {
                    stats.frame_delivery_ms = (pipeline_latency_sum / pipeline_latency_count as f64) as f32;
                    pipeline_latency_sum = 0.0;
                    pipeline_latency_count = 0;
                }

                // Get network stats from WebRTC (RTT from ICE candidate pair)
                let net_stats = peer.get_network_stats().await;
                if net_stats.rtt_ms > 0.0 {
                    stats.rtt_ms = net_stats.rtt_ms;
                }

                // Estimate end-to-end latency:
                // E2E = network_rtt/2 (input to server) + server_processing (~16ms at 60fps)
                //     + network_rtt/2 (video back) + decode_time + render_time
                // If RTT is 0 (ice-lite), estimate based on typical values
                let (estimated_network_oneway, rtt_source) = if stats.rtt_ms > 0.0 {
                    (stats.rtt_ms / 2.0, "measured")
                } else {
                    // Estimate ~10ms one-way (20ms RTT) for fiber/good internet connection
                    // This prevents alarming ~80ms E2E reports on good connections where RTT is unmeasured
                    (10.0, "estimated")
                };
                let server_frame_time = 1000.0 / stats.target_fps.max(1) as f32; // ~16.7ms at 60fps
                let server_encode_time = 8.0; // Estimated server encode latency ~8ms
                stats.estimated_e2e_ms = estimated_network_oneway * 2.0 // network round trip
                    + server_frame_time // server processing (1 frame)
                    + server_encode_time // server encode
                    + stats.decode_time_ms
                    + stats.render_time_ms;

                // Log latency breakdown once
                static LOGGED_LATENCY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !LOGGED_LATENCY.swap(true, std::sync::atomic::Ordering::Relaxed) && stats.decode_time_ms > 0.0 {
                    info!("Latency breakdown ({}): network={:.0}ms x2, server_frame={:.0}ms, encode=8ms, decode={:.1}ms, render={:.1}ms = ~{:.0}ms E2E",
                        rtt_source, estimated_network_oneway, server_frame_time, stats.decode_time_ms, stats.render_time_ms, stats.estimated_e2e_ms);
                    info!("Note: If actual latency is higher, check server distance or try a closer region");
                }

                // Log if FPS is significantly below target (more than 20% drop)
                if stats.fps > 0.0 && stats.fps < (fps as f32 * 0.8) {
                    debug!("FPS below target: {:.1} / {} (dropped: {})", stats.fps, fps, frames_dropped);
                }

                // Update racing wheel count for UI notification
                stats.wheel_count = wheel_manager.wheel_count();

                // Reset counters
                bytes_received = 0;
                last_stats_time = now;

                // Send stats update
                let _ = stats_tx.try_send(stats.clone());
            }
        }
    }

    // Stop input managers
    controller_manager.stop();
    wheel_manager.stop();
    g29_ffb.stop();

    // Clean up raw input sender
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    crate::input::clear_raw_input_sender();

    info!("Streaming session ended");
    StreamingResult::Normal
}
