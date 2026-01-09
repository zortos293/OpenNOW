//! Video Decoder
//!
//! Hardware-accelerated H.264/H.265 decoding.
//!
//! Platform-specific backends:
//! - Windows: Native DXVA (D3D11 Video API)
//! - macOS: FFmpeg with VideoToolbox
//! - Linux: Native Vulkan Video or GStreamer
//!
//! This module provides both blocking and non-blocking decode modes:
//! - Blocking: `decode()` - waits for result (legacy, causes latency)
//! - Non-blocking: `decode_async()` - fire-and-forget, writes to SharedFrame

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc as tokio_mpsc;

#[cfg(target_os = "windows")]
use std::path::Path;

use super::{ColorRange, ColorSpace, PixelFormat, TransferFunction, VideoFrame};
use crate::app::{config::VideoDecoderBackend, SharedFrame, VideoCodec};

// FFmpeg imports - only for macOS
#[cfg(target_os = "macos")]
extern crate ffmpeg_next as ffmpeg;

#[cfg(target_os = "macos")]
use ffmpeg::codec::{context::Context as CodecContext, decoder};
#[cfg(target_os = "macos")]
use ffmpeg::format::Pixel;
#[cfg(target_os = "macos")]
use ffmpeg::software::scaling::{context::Context as ScalerContext, flag::Flags as ScalerFlags};
#[cfg(target_os = "macos")]
use ffmpeg::util::frame::video::Video as FfmpegFrame;
#[cfg(target_os = "macos")]
use ffmpeg::Packet;

/// GPU Vendor for decoder optimization
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum GpuVendor {
    Nvidia,
    Intel,
    Amd,
    Apple,
    Broadcom, // Raspberry Pi VideoCore
    Other,
    Unknown,
}

/// Cached GPU vendor
static GPU_VENDOR: std::sync::OnceLock<GpuVendor> = std::sync::OnceLock::new();

/// Detect the primary GPU vendor using wgpu, prioritizing discrete GPUs
pub fn detect_gpu_vendor() -> GpuVendor {
    *GPU_VENDOR.get_or_init(|| {
        // blocked_on because we are in a sync context (VideoDecoder::new)
        // but wgpu adapter request is async
        pollster::block_on(async {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default()); // Needs borrow

            // Enumerate all available adapters (wgpu 28 returns a Future)
            let adapters = instance.enumerate_adapters(wgpu::Backends::all()).await;

            let mut best_score = -1;
            let mut best_vendor = GpuVendor::Unknown;

            info!("Available GPU adapters:");

            for adapter in adapters {
                let info = adapter.get_info();
                let name = info.name.to_lowercase();
                let mut score = 0;
                let mut vendor = GpuVendor::Other;

                // Identify vendor
                if name.contains("nvidia") || name.contains("geforce") || name.contains("quadro") {
                    vendor = GpuVendor::Nvidia;
                    score += 100;
                } else if name.contains("amd") || name.contains("adeon") || name.contains("ryzen") {
                    vendor = GpuVendor::Amd;
                    score += 80;
                } else if name.contains("intel")
                    || name.contains("uhd")
                    || name.contains("iris")
                    || name.contains("arc")
                {
                    vendor = GpuVendor::Intel;
                    score += 50;
                } else if name.contains("apple")
                    || name.contains("m1")
                    || name.contains("m2")
                    || name.contains("m3")
                {
                    vendor = GpuVendor::Apple;
                    score += 90; // Apple Silicon is high perf
                } else if name.contains("videocore")
                    || name.contains("broadcom")
                    || name.contains("v3d")
                    || name.contains("vc4")
                {
                    vendor = GpuVendor::Broadcom;
                    score += 30; // Raspberry Pi - low power device
                }

                // Prioritize discrete GPUs
                match info.device_type {
                    wgpu::DeviceType::DiscreteGpu => {
                        score += 50;
                    }
                    wgpu::DeviceType::IntegratedGpu => {
                        score += 10;
                    }
                    _ => {}
                }

                info!(
                    "  - {} ({:?}, Vendor: {:?}, Score: {})",
                    info.name, info.device_type, vendor, score
                );

                if score > best_score {
                    best_score = score;
                    best_vendor = vendor;
                }
            }

            if best_vendor != GpuVendor::Unknown {
                info!("Selected best GPU vendor: {:?}", best_vendor);
                best_vendor
            } else {
                // Fallback to default request if enumeration fails
                warn!("Adapter enumeration yielded no results, trying default request");

                let adapter_result = instance
                    .request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::HighPerformance,
                        compatible_surface: None,
                        force_fallback_adapter: false,
                    })
                    .await;

                // Handle Result
                if let Ok(adapter) = adapter_result {
                    let info = adapter.get_info();
                    let name = info.name.to_lowercase();

                    if name.contains("nvidia") {
                        GpuVendor::Nvidia
                    } else if name.contains("intel") {
                        GpuVendor::Intel
                    } else if name.contains("amd") {
                        GpuVendor::Amd
                    } else if name.contains("apple") {
                        GpuVendor::Apple
                    } else if name.contains("videocore")
                        || name.contains("broadcom")
                        || name.contains("v3d")
                    {
                        GpuVendor::Broadcom
                    } else {
                        GpuVendor::Other
                    }
                } else {
                    GpuVendor::Unknown
                }
            }
        })
    })
}

/// Check if Intel QSV runtime is available on the system
/// Returns true if the required DLLs are found
#[cfg(target_os = "windows")]
fn is_qsv_runtime_available() -> bool {
    use std::env;

    // Intel Media SDK / oneVPL runtime DLLs to look for
    let runtime_dlls = [
        "libmfx-gen.dll", // Intel oneVPL runtime (11th gen+, newer)
        "libmfxhw64.dll", // Intel Media SDK runtime (older)
        "mfxhw64.dll",    // Alternative naming
        "libmfx64.dll",   // Another variant
    ];

    // Check common paths where Intel runtimes are installed
    let search_paths: Vec<std::path::PathBuf> = vec![
        // System32 (most common for driver-installed runtimes)
        env::var("SystemRoot")
            .map(|s| Path::new(&s).join("System32"))
            .unwrap_or_default(),
        // SysWOW64 for 32-bit
        env::var("SystemRoot")
            .map(|s| Path::new(&s).join("SysWOW64"))
            .unwrap_or_default(),
        // Intel Media SDK default install
        Path::new(
            "C:\\Program Files\\Intel\\Media SDK 2023 R1\\Software Development Kit\\bin\\x64",
        )
        .to_path_buf(),
        Path::new("C:\\Program Files\\Intel\\Media SDK\\bin\\x64").to_path_buf(),
        // oneVPL default install
        Path::new("C:\\Program Files (x86)\\Intel\\oneAPI\\vpl\\latest\\bin").to_path_buf(),
        // Application directory (for bundled DLLs)
        env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_default(),
    ];

    for dll in &runtime_dlls {
        for path in &search_paths {
            let full_path = path.join(dll);
            if full_path.exists() {
                info!("Found Intel QSV runtime: {}", full_path.display());
                return true;
            }
        }
    }

    // Also try loading via Windows DLL search path
    // If Intel drivers are installed, the DLLs should be in PATH
    if let Ok(output) = std::process::Command::new("where")
        .arg("libmfx-gen.dll")
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout);
            info!("Found Intel QSV runtime via PATH: {}", path.trim());
            return true;
        }
    }

    debug!("Intel QSV runtime not found - QSV decoder will be skipped");
    false
}

#[cfg(not(target_os = "windows"))]
fn is_qsv_runtime_available() -> bool {
    // On Linux, check for libmfx.so or libvpl.so
    use std::process::Command;

    // QSV is only supported on Intel architectures
    if !cfg!(target_arch = "x86") && !cfg!(target_arch = "x86_64") {
        return false;
    }

    if let Ok(output) = Command::new("ldconfig").arg("-p").output() {
        let libs = String::from_utf8_lossy(&output.stdout);
        if libs.contains("libmfx") || libs.contains("libvpl") {
            info!("Found Intel QSV runtime on Linux");
            return true;
        }
    }

    debug!("Intel QSV runtime not found on Linux");
    false
}

/// Cached QSV availability check (only check once at startup)
static QSV_AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn check_qsv_available() -> bool {
    *QSV_AVAILABLE.get_or_init(|| {
        let available = is_qsv_runtime_available();
        if available {
            info!("Intel QuickSync Video (QSV) runtime detected - QSV decoding enabled");
        } else {
            info!("Intel QSV runtime not detected - QSV decoding disabled (install Intel GPU drivers for QSV support)");
        }
        available
    })
}

/// Cached Intel GPU name for QSV capability detection
static INTEL_GPU_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Get the Intel GPU name from wgpu adapter info
fn get_intel_gpu_name() -> String {
    INTEL_GPU_NAME
        .get_or_init(|| {
            pollster::block_on(async {
                let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
                let adapters = instance.enumerate_adapters(wgpu::Backends::all()).await;

                for adapter in adapters {
                    let info = adapter.get_info();
                    let name = info.name.to_lowercase();
                    if name.contains("intel") {
                        return info.name.clone();
                    }
                }
                String::new()
            })
        })
        .clone()
}

/// Check if the Intel GPU supports QSV decoding for the given codec (macOS only)
/// Older Intel GPUs have limited QSV codec support:
/// - Gen 7 (Ivy Bridge/HD 4000, 2012): Only H.264
/// - Gen 8 (Haswell, 2013): H.264 + limited HEVC
/// - Gen 9 (Skylake, 2015+): H.264 + HEVC
#[cfg(target_os = "macos")]
fn is_qsv_supported_for_codec(codec_id: ffmpeg::codec::Id) -> bool {
    // First check if QSV runtime is even available
    if !check_qsv_available() {
        return false;
    }

    let gpu_name = get_intel_gpu_name();
    let gpu_lower = gpu_name.to_lowercase();

    // Detect older Intel GPU generations that have limited QSV support
    let is_gen7_or_older = gpu_lower.contains("hd graphics 4000")
        || gpu_lower.contains("hd 4000")
        || gpu_lower.contains("hd graphics 2500")
        || gpu_lower.contains("hd 2500")
        || gpu_lower.contains("ivy bridge")
        || gpu_lower.contains("sandy bridge")
        || gpu_lower.contains("hd graphics 3000")
        || gpu_lower.contains("hd 3000");

    match codec_id {
        ffmpeg::codec::Id::H264 => {
            // H.264 supported on all Intel QSV generations
            true
        }
        ffmpeg::codec::Id::HEVC => {
            // HEVC not supported on Gen 7 (Ivy Bridge) and older
            if is_gen7_or_older {
                info!("Intel GPU '{}' (Gen 7 or older) does not support HEVC QSV - using software decoder", gpu_name);
                return false;
            }
            // Gen 8 has limited HEVC support (decode only, 8-bit only)
            true
        }
        _ => true, // Unknown codecs - try QSV
    }
}

/// Cached supported decoder backends
static SUPPORTED_BACKENDS: std::sync::OnceLock<Vec<VideoDecoderBackend>> =
    std::sync::OnceLock::new();

/// Get list of supported decoder backends for the current system
pub fn get_supported_decoder_backends() -> Vec<VideoDecoderBackend> {
    SUPPORTED_BACKENDS
        .get_or_init(|| {
            let mut backends = vec![VideoDecoderBackend::Auto];

            // Always check what's actually available
            #[cfg(target_os = "macos")]
            {
                backends.push(VideoDecoderBackend::VideoToolbox);
            }

            #[cfg(target_os = "windows")]
            {
                let gpu = detect_gpu_vendor();
                let qsv = check_qsv_available();

                // GStreamer D3D11 decoder - supports both H.264 and HEVC
                // This is the recommended decoder for Windows (stable, works on all GPUs)
                backends.push(VideoDecoderBackend::Dxva);

                // Native D3D11VA decoder (HEVC only) - EXPERIMENTAL
                // Uses direct D3D11 Video API for zero-copy decoding
                // WARNING: Only supports HEVC. H.264 streams will fail!
                backends.push(VideoDecoderBackend::NativeDxva);

                // GPU-specific accelerators
                if gpu == GpuVendor::Nvidia {
                    backends.push(VideoDecoderBackend::Cuvid);
                }

                if qsv || gpu == GpuVendor::Intel {
                    backends.push(VideoDecoderBackend::Qsv);
                }
            }

            #[cfg(target_os = "linux")]
            {
                let gpu = detect_gpu_vendor();
                let qsv = check_qsv_available();

                // GStreamer with hardware acceleration is the preferred decoder on Linux
                // It automatically selects the best available hardware decoder (VAAPI, NVDEC, etc.)
                if super::gstreamer_decoder::is_gstreamer_available() {
                    backends.push(VideoDecoderBackend::VulkanVideo); // GStreamer-based hardware decode
                }

                if gpu == GpuVendor::Nvidia {
                    backends.push(VideoDecoderBackend::Cuvid);
                }

                if qsv || gpu == GpuVendor::Intel {
                    backends.push(VideoDecoderBackend::Qsv);
                }

                // VAAPI is generally available on Linux (AMD/Intel)
                backends.push(VideoDecoderBackend::Vaapi);
            }

            backends.push(VideoDecoderBackend::Software);
            backends
        })
        .clone()
}

/// Commands sent to the decoder thread
enum DecoderCommand {
    /// Decode a packet and return result via channel (blocking mode)
    Decode(Vec<u8>),
    /// Decode a packet and write directly to SharedFrame (non-blocking mode)
    DecodeAsync {
        data: Vec<u8>,
        receive_time: std::time::Instant,
    },
    Stop,
}

/// Stats from the decoder thread
#[derive(Debug, Clone)]
pub struct DecodeStats {
    /// Time from packet receive to decode complete (ms)
    pub decode_time_ms: f32,
    /// Whether a frame was produced
    pub frame_produced: bool,
    /// Whether a keyframe is needed (too many consecutive decode failures)
    pub needs_keyframe: bool,
}

/// Video decoder using FFmpeg with hardware acceleration
/// Uses a dedicated thread for decoding since FFmpeg types are not Send
pub struct VideoDecoder {
    cmd_tx: mpsc::Sender<DecoderCommand>,
    frame_rx: mpsc::Receiver<Option<VideoFrame>>,
    /// Stats receiver for non-blocking mode
    stats_rx: Option<tokio_mpsc::Receiver<DecodeStats>>,
    hw_accel: bool,
    frames_decoded: u64,
    /// SharedFrame for non-blocking writes (set via set_shared_frame)
    shared_frame: Option<Arc<SharedFrame>>,
}

impl VideoDecoder {
    /// Create a new video decoder with hardware acceleration
    /// Note: On Linux, use new_async() instead - Linux uses native Vulkan Video decoder
    #[cfg(target_os = "macos")]
    pub fn new(codec: VideoCodec, backend: VideoDecoderBackend) -> Result<Self> {
        // Initialize FFmpeg
        ffmpeg::init().map_err(|e| anyhow!("Failed to initialize FFmpeg: {:?}", e))?;

        // Suppress FFmpeg's "no frame" info messages (EAGAIN is normal for H.264)
        unsafe {
            ffmpeg::ffi::av_log_set_level(ffmpeg::ffi::AV_LOG_ERROR as i32);
        }

        info!(
            "Creating FFmpeg video decoder for {:?} (backend: {:?})",
            codec, backend
        );

        // Find the decoder
        let decoder_id = match codec {
            VideoCodec::H264 => ffmpeg::codec::Id::H264,
            VideoCodec::H265 => ffmpeg::codec::Id::HEVC,
            VideoCodec::AV1 => ffmpeg::codec::Id::AV1,
        };

        // Create channels for communication with decoder thread
        let (cmd_tx, cmd_rx) = mpsc::channel::<DecoderCommand>();
        let (frame_tx, frame_rx) = mpsc::channel::<Option<VideoFrame>>();

        // Create decoder in a separate thread (FFmpeg types are not Send)
        let hw_accel =
            Self::spawn_decoder_thread(decoder_id, cmd_rx, frame_tx, None, None, backend)?;

        if hw_accel {
            info!("Using hardware-accelerated decoder");
        } else {
            info!("Using software decoder (hardware acceleration not available)");
        }

        Ok(Self {
            cmd_tx,
            frame_rx,
            stats_rx: None,
            hw_accel,
            frames_decoded: 0,
            shared_frame: None,
        })
    }

    /// Create a new video decoder configured for non-blocking async mode
    /// Decoded frames are written directly to the SharedFrame
    pub fn new_async(
        codec: VideoCodec,
        backend: VideoDecoderBackend,
        shared_frame: Arc<SharedFrame>,
    ) -> Result<(Self, tokio_mpsc::Receiver<DecodeStats>)> {
        // On Windows, use native DXVA decoder (no FFmpeg)
        // This uses D3D11 Video API directly for hardware acceleration
        #[cfg(target_os = "windows")]
        {
            return Err(anyhow!(
                "VideoDecoder::new_async not supported on Windows. Use UnifiedVideoDecoder::new_async instead."
            ));
        }

        // On Linux, use GStreamer for hardware-accelerated decoding
        // GStreamer automatically selects the best available backend (VAAPI, NVDEC, V4L2, etc.)
        #[cfg(target_os = "linux")]
        {
            // Use GStreamer decoder (auto-selects V4L2/VAAPI/NVDEC/software)
            // The GStreamer decoder automatically selects the best available backend
            if super::gstreamer_decoder::is_gstreamer_available() {
                info!(
                    "Using GStreamer decoder for {:?} (auto-selects V4L2/VA/VAAPI/software)",
                    codec
                );

                let gst_codec = match codec {
                    VideoCodec::H264 => super::gstreamer_decoder::GstCodec::H264,
                    VideoCodec::H265 => super::gstreamer_decoder::GstCodec::H265,
                    VideoCodec::AV1 => super::gstreamer_decoder::GstCodec::AV1,
                };

                let config = super::gstreamer_decoder::GstDecoderConfig {
                    codec: gst_codec,
                    width: 1920,
                    height: 1080,
                    low_latency: true, // Enable low latency for streaming
                };

                let gst_decoder = super::gstreamer_decoder::GStreamerDecoder::new(config)
                    .map_err(|e| anyhow!("Failed to create GStreamer decoder: {}", e))?;

                info!("GStreamer decoder created successfully!");

                let (cmd_tx, cmd_rx) = mpsc::channel::<DecoderCommand>();
                let (frame_tx, frame_rx) = mpsc::channel::<Option<VideoFrame>>();
                let (stats_tx, stats_rx) = tokio_mpsc::channel::<DecodeStats>(64);

                let shared_frame_clone = shared_frame.clone();

                thread::spawn(move || {
                    info!("GStreamer decoder thread started");
                    let mut decoder = gst_decoder;
                    let mut frames_decoded = 0u64;
                    let mut consecutive_failures = 0u32;
                    const KEYFRAME_REQUEST_THRESHOLD: u32 = 10;
                    const FRAMES_TO_SKIP: u64 = 5;

                    while let Ok(cmd) = cmd_rx.recv() {
                        match cmd {
                            DecoderCommand::Decode(data) => {
                                let result = decoder.decode(&data);
                                let _ = frame_tx.send(result.ok().flatten());
                            }
                            DecoderCommand::DecodeAsync { data, receive_time } => {
                                let result = decoder.decode(&data);
                                let decode_time_ms = receive_time.elapsed().as_secs_f32() * 1000.0;

                                let frame_produced = matches!(&result, Ok(Some(_)));

                                let needs_keyframe = if frame_produced {
                                    consecutive_failures = 0;
                                    false
                                } else {
                                    consecutive_failures += 1;
                                    consecutive_failures == KEYFRAME_REQUEST_THRESHOLD
                                };

                                if let Ok(Some(frame)) = result {
                                    frames_decoded += 1;
                                    if frames_decoded > FRAMES_TO_SKIP {
                                        shared_frame_clone.write(frame);
                                    }
                                }

                                let _ = stats_tx.try_send(DecodeStats {
                                    decode_time_ms,
                                    frame_produced,
                                    needs_keyframe,
                                });
                            }
                            DecoderCommand::Stop => break,
                        }
                    }
                    info!("GStreamer decoder thread stopped");
                });

                let decoder = Self {
                    cmd_tx,
                    frame_rx,
                    stats_rx: None,
                    hw_accel: true,
                    frames_decoded: 0,
                    shared_frame: Some(shared_frame),
                };

                return Ok((decoder, stats_rx));
            }

            // No decoder available
            return Err(anyhow!(
                "No video decoder available on Linux. Requires either:\n\
                 - Vulkan Video support (Intel Arc, NVIDIA RTX, AMD RDNA2+)\n\
                 - GStreamer with hardware decoding:\n\
                   * V4L2 (Raspberry Pi / embedded)\n\
                   * VA plugin (Intel/AMD desktop - vah264dec)\n\
                   * VAAPI plugin (legacy Intel/AMD - vaapih264dec)\n\
                   * Software fallback (avdec_h264)\n\
                 Run 'vulkaninfo | grep video' to check Vulkan Video support.\n\
                 Run 'gst-inspect-1.0 | grep -E \"v4l2|va|vaapi|avdec\"' to check GStreamer decoders."
            ));
        }

        // FFmpeg path (Windows/macOS only)
        #[cfg(target_os = "macos")]
        {
            // Initialize FFmpeg
            ffmpeg::init().map_err(|e| anyhow!("Failed to initialize FFmpeg: {:?}", e))?;

            // Suppress FFmpeg's "no frame" info messages (EAGAIN is normal for H.264)
            unsafe {
                ffmpeg::ffi::av_log_set_level(ffmpeg::ffi::AV_LOG_ERROR as i32);
            }

            info!(
                "Creating FFmpeg video decoder (async mode) for {:?} (backend: {:?})",
                codec, backend
            );

            // Find the decoder
            let decoder_id = match codec {
                VideoCodec::H264 => ffmpeg::codec::Id::H264,
                VideoCodec::H265 => ffmpeg::codec::Id::HEVC,
                VideoCodec::AV1 => ffmpeg::codec::Id::AV1,
            };

            // Create channels for communication with decoder thread
            let (cmd_tx, cmd_rx) = mpsc::channel::<DecoderCommand>();
            let (frame_tx, frame_rx) = mpsc::channel::<Option<VideoFrame>>();

            // Stats channel for async mode (non-blocking stats updates)
            let (stats_tx, stats_rx) = tokio_mpsc::channel::<DecodeStats>(64);

            // Create decoder in a separate thread with SharedFrame
            let hw_accel = Self::spawn_decoder_thread(
                decoder_id,
                cmd_rx,
                frame_tx,
                Some(shared_frame.clone()),
                Some(stats_tx),
                backend,
            )?;

            if hw_accel {
                info!("Using hardware-accelerated decoder (async mode)");
            } else {
                info!("Using software decoder (async mode)");
            }

            let decoder = Self {
                cmd_tx,
                frame_rx,
                stats_rx: None, // Stats come via the returned receiver
                hw_accel,
                frames_decoded: 0,
                shared_frame: Some(shared_frame),
            };

            Ok((decoder, stats_rx))
        } // end #[cfg(target_os = "macos")]
    }

    /// Spawn a dedicated decoder thread (FFmpeg-based, not used on Linux)
    #[cfg(target_os = "macos")]
    fn spawn_decoder_thread(
        codec_id: ffmpeg::codec::Id,
        cmd_rx: mpsc::Receiver<DecoderCommand>,
        frame_tx: mpsc::Sender<Option<VideoFrame>>,
        shared_frame: Option<Arc<SharedFrame>>,
        stats_tx: Option<tokio_mpsc::Sender<DecodeStats>>,
        backend: VideoDecoderBackend,
    ) -> Result<bool> {
        // Create decoder synchronously to report hw_accel status
        info!("Creating decoder for codec {:?}...", codec_id);
        let (decoder, hw_accel) = Self::create_decoder(codec_id, backend)?;
        info!("Decoder created, hw_accel={}", hw_accel);

        // Spawn thread to handle decoding
        thread::spawn(move || {
            info!("Decoder thread started for {:?}", codec_id);
            let mut decoder = decoder;
            let mut scaler: Option<ScalerContext> = None;
            let mut width = 0u32;
            let mut height = 0u32;
            let mut frames_decoded = 0u64;
            let mut consecutive_failures = 0u32;
            let mut packets_received = 0u64;
            const KEYFRAME_REQUEST_THRESHOLD: u32 = 10; // Request keyframe after 10 consecutive failures (was 30)
            const FRAMES_TO_SKIP: u64 = 5; // Skip first N frames to let decoder settle with reference frames

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    DecoderCommand::Decode(data) => {
                        // Blocking mode - send result back via channel
                        let result = Self::decode_frame(
                            &mut decoder,
                            &mut scaler,
                            &mut width,
                            &mut height,
                            &mut frames_decoded,
                            &data,
                            codec_id,
                            false, // No recovery tracking for blocking mode
                        );
                        let _ = frame_tx.send(result);
                    }
                    DecoderCommand::DecodeAsync { data, receive_time } => {
                        packets_received += 1;

                        // Check if we're in recovery mode (waiting for keyframe)
                        let in_recovery = consecutive_failures >= KEYFRAME_REQUEST_THRESHOLD;

                        // Non-blocking mode - write directly to SharedFrame
                        let result = Self::decode_frame(
                            &mut decoder,
                            &mut scaler,
                            &mut width,
                            &mut height,
                            &mut frames_decoded,
                            &data,
                            codec_id,
                            in_recovery,
                        );

                        let decode_time_ms = receive_time.elapsed().as_secs_f32() * 1000.0;
                        let frame_produced = result.is_some();

                        // Track consecutive decode failures for PLI request
                        // Note: EAGAIN (no frame) is normal for H.264 - decoder buffers B-frames
                        let needs_keyframe = if frame_produced {
                            // Only log recovery for significant failures (>5), not normal buffering
                            if consecutive_failures > 5 {
                                info!(
                                    "Decoder: recovered after {} packets without output",
                                    consecutive_failures
                                );
                            }
                            consecutive_failures = 0;
                            false
                        } else {
                            consecutive_failures += 1;

                            // Only log at higher thresholds - low counts are normal H.264 buffering
                            if consecutive_failures == 30 {
                                debug!(
                                    "Decoder: {} packets without frame (packets: {}, decoded: {})",
                                    consecutive_failures, packets_received, frames_decoded
                                );
                            }

                            if consecutive_failures == KEYFRAME_REQUEST_THRESHOLD {
                                warn!("Decoder: {} consecutive frames without output - requesting keyframe (packets: {}, decoded: {})",
                                    consecutive_failures, packets_received, frames_decoded);
                                true
                            } else if consecutive_failures > KEYFRAME_REQUEST_THRESHOLD
                                && consecutive_failures % 20 == 0
                            {
                                // Keep requesting every 20 frames if still failing (~166ms at 120fps)
                                warn!("Decoder: still failing after {} frames - requesting keyframe again", consecutive_failures);
                                true
                            } else {
                                false
                            }
                        };

                        // Write frame directly to SharedFrame (zero-copy handoff)
                        // Skip first few frames to let decoder settle with proper reference frames
                        // This prevents green/corrupted frames during stream startup
                        if let Some(frame) = result {
                            if frames_decoded > FRAMES_TO_SKIP {
                                if let Some(ref sf) = shared_frame {
                                    sf.write(frame);
                                }
                            } else {
                                debug!(
                                    "Skipping frame {} (waiting for decoder to settle)",
                                    frames_decoded
                                );
                            }
                        }

                        // Send stats update (non-blocking)
                        if let Some(ref tx) = stats_tx {
                            let _ = tx.try_send(DecodeStats {
                                decode_time_ms,
                                frame_produced,
                                needs_keyframe,
                            });
                        }
                    }
                    DecoderCommand::Stop => break,
                }
            }
        });

        Ok(hw_accel)
    }

    /// FFI Callback for format negotiation (VideoToolbox)
    #[cfg(target_os = "macos")]
    unsafe extern "C" fn get_videotoolbox_format(
        _ctx: *mut ffmpeg::ffi::AVCodecContext,
        mut fmt: *const ffmpeg::ffi::AVPixelFormat,
    ) -> ffmpeg::ffi::AVPixelFormat {
        use ffmpeg::ffi::*;

        // Log all available formats for debugging
        let mut available_formats = Vec::new();
        let mut check_fmt = fmt;
        while *check_fmt != AVPixelFormat::AV_PIX_FMT_NONE {
            available_formats.push(*check_fmt as i32);
            check_fmt = check_fmt.add(1);
        }
        info!(
            "get_format callback: available formats: {:?} (VIDEOTOOLBOX={}, NV12={}, YUV420P={})",
            available_formats,
            AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX as i32,
            AVPixelFormat::AV_PIX_FMT_NV12 as i32,
            AVPixelFormat::AV_PIX_FMT_YUV420P as i32
        );

        while *fmt != AVPixelFormat::AV_PIX_FMT_NONE {
            if *fmt == AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX {
                info!("get_format: selecting VIDEOTOOLBOX hardware format");
                return AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX;
            }
            fmt = fmt.add(1);
        }

        info!("get_format: VIDEOTOOLBOX not available, falling back to NV12");
        AVPixelFormat::AV_PIX_FMT_NV12
    }

    /// FFI Callback for D3D11VA format negotiation (works on all Windows GPUs including NVIDIA)
    /// This produces D3D11 textures that can be shared with wgpu via DXGI handles
    ///
    /// CRITICAL: This callback must set up hw_frames_ctx for D3D11VA to work!
    /// Based on NVIDIA GeForce NOW client's DXVADecoder implementation.
    ///
    /// Key insight: NVIDIA drivers are strict about texture dimensions.
    /// - `coded_width/height` includes encoder padding (e.g., 3840x2176)
    /// - Actual video dimensions are `width/height` (e.g., 3840x2160)
    /// - We must use dimensions that are multiples of the codec's macroblock size
    /// - For HEVC: 32x32 CTU alignment, for H.264: 16x16 MB alignment
    ///
    /// Note: This is only used on macOS where FFmpeg is used for video decoding.
    /// Windows uses native DXVA, Linux uses Vulkan Video.
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    unsafe extern "C" fn get_d3d11va_format(
        ctx: *mut ffmpeg::ffi::AVCodecContext,
        fmt: *const ffmpeg::ffi::AVPixelFormat,
    ) -> ffmpeg::ffi::AVPixelFormat {
        use ffmpeg::ffi::*;

        // Log all available formats for debugging
        let mut available_formats = Vec::new();
        let mut check_fmt = fmt;
        while *check_fmt != AVPixelFormat::AV_PIX_FMT_NONE {
            available_formats.push(*check_fmt as i32);
            check_fmt = check_fmt.add(1);
        }
        info!(
            "get_d3d11va_format: available formats: {:?} (D3D11={}, NV12={}, P010={})",
            available_formats,
            AVPixelFormat::AV_PIX_FMT_D3D11 as i32,
            AVPixelFormat::AV_PIX_FMT_NV12 as i32,
            AVPixelFormat::AV_PIX_FMT_P010LE as i32
        );

        // Check if D3D11 format is available
        let mut has_d3d11 = false;
        check_fmt = fmt;
        while *check_fmt != AVPixelFormat::AV_PIX_FMT_NONE {
            if *check_fmt == AVPixelFormat::AV_PIX_FMT_D3D11 {
                has_d3d11 = true;
                break;
            }
            check_fmt = check_fmt.add(1);
        }

        if !has_d3d11 {
            warn!("get_d3d11va_format: D3D11 not in available formats list");
            return *fmt;
        }

        // We need hw_device_ctx to create hw_frames_ctx
        if (*ctx).hw_device_ctx.is_null() {
            warn!("get_d3d11va_format: hw_device_ctx is null, cannot use D3D11VA");
            return *fmt;
        }

        // Check if hw_frames_ctx already exists (might be called multiple times)
        if !(*ctx).hw_frames_ctx.is_null() {
            info!("get_d3d11va_format: hw_frames_ctx already set, selecting D3D11");
            return AVPixelFormat::AV_PIX_FMT_D3D11;
        }

        // Determine sw_format based on codec and bit depth
        // HEVC Main10/AV1 10-bit needs P010, others use NV12
        // Check multiple indicators for 10-bit content
        let is_10bit = (*ctx).profile == 2  // HEVC Main10 profile
            || (*ctx).pix_fmt == AVPixelFormat::AV_PIX_FMT_YUV420P10LE
            || (*ctx).pix_fmt == AVPixelFormat::AV_PIX_FMT_YUV420P10BE
            || (*ctx).pix_fmt == AVPixelFormat::AV_PIX_FMT_P010LE
            || ((*ctx).codec_id == AVCodecID::AV_CODEC_ID_AV1 && (*ctx).profile >= 1); // AV1 High/Professional profile

        // Calculate proper dimensions for D3D11VA texture
        // NVIDIA drivers are strict: they don't like coded_width/height with encoder padding
        //
        // The issue: coded_width/height includes encoder alignment padding
        // - 4K video (3840x2160) may have coded dimensions of 3840x2176 (16 pixels padding for HEVC CTU)
        // - This padding causes D3D11VA texture creation to fail on NVIDIA with error 80070057
        //
        // Solution: Remove the padding by detecting common video resolutions
        // Standard resolutions: 2160p (4K), 1440p, 1080p, 720p, etc.
        let coded_w = (*ctx).coded_width;
        let coded_h = (*ctx).coded_height;

        // Calculate actual video height by removing encoder padding
        // HEVC uses 32-pixel CTU, H.264 uses 16-pixel MB
        // Common pattern: encoder adds 16-32 pixels of padding to height
        let actual_height = if coded_h > 2160 && coded_h <= 2176 {
            2160 // 4K UHD
        } else if coded_h > 1440 && coded_h <= 1472 {
            1440 // QHD
        } else if coded_h > 1080 && coded_h <= 1088 {
            1080 // Full HD
        } else if coded_h > 720 && coded_h <= 736 {
            720 // HD
        } else if coded_h > 480 && coded_h <= 496 {
            480 // SD
        } else {
            // No standard padding detected, use coded height
            coded_h
        };

        // Width is usually already correct (16:9 widths are typically aligned)
        let actual_width = coded_w;

        info!(
            "get_d3d11va_format: codec={:?}, profile={}, pix_fmt={:?}, coded={}x{}, actual={}x{}, is_10bit={}",
            (*ctx).codec_id,
            (*ctx).profile,
            (*ctx).pix_fmt as i32,
            coded_w,
            coded_h,
            actual_width,
            actual_height,
            is_10bit
        );

        // Try formats in order of preference
        // NVIDIA's DXVADecoder supports: NV12, P010, YUV444, YUV444_10
        let formats_to_try = if is_10bit {
            vec![
                AVPixelFormat::AV_PIX_FMT_P010LE,
                AVPixelFormat::AV_PIX_FMT_NV12,
            ]
        } else {
            vec![
                AVPixelFormat::AV_PIX_FMT_NV12,
                AVPixelFormat::AV_PIX_FMT_P010LE,
            ]
        };

        // Try with actual dimensions first (without padding), then fall back to coded dimensions
        let dimensions_to_try = [(actual_width, actual_height), (coded_w, coded_h)];

        for (width, height) in dimensions_to_try {
            for sw_format in &formats_to_try {
                // Allocate fresh hw_frames_ctx for each attempt
                let hw_frames_ref = av_hwframe_ctx_alloc((*ctx).hw_device_ctx);
                if hw_frames_ref.is_null() {
                    warn!("get_d3d11va_format: Failed to allocate hw_frames_ctx");
                    continue;
                }

                // Configure the frames context
                let frames_ctx = (*hw_frames_ref).data as *mut AVHWFramesContext;
                (*frames_ctx).format = AVPixelFormat::AV_PIX_FMT_D3D11;
                (*frames_ctx).sw_format = *sw_format;
                (*frames_ctx).width = width;
                (*frames_ctx).height = height;

                // NVIDIA-compatible pool size
                // NVIDIA's DXVADecoder uses texture arrays (RTArray) with ~16-20 surfaces
                // This matches their "allocating RTArrays" approach
                (*frames_ctx).initial_pool_size = 20;

                info!(
                    "get_d3d11va_format: Trying hw_frames_ctx: {}x{}, sw_format={:?}, pool_size=20",
                    width, height, *sw_format as i32
                );

                // Initialize the frames context
                let ret = av_hwframe_ctx_init(hw_frames_ref);
                if ret >= 0 {
                    // Success! Attach to codec context
                    (*ctx).hw_frames_ctx = av_buffer_ref(hw_frames_ref);
                    av_buffer_unref(&mut (hw_frames_ref as *mut _));

                    let format_name = if *sw_format == AVPixelFormat::AV_PIX_FMT_P010LE {
                        "P010 (10-bit HDR)"
                    } else {
                        "NV12 (8-bit SDR)"
                    };
                    info!(
                        "get_d3d11va_format: D3D11VA hw_frames_ctx initialized with {} at {}x{} - zero-copy enabled!",
                        format_name, width, height
                    );
                    return AVPixelFormat::AV_PIX_FMT_D3D11;
                }

                // Failed, clean up and try next format
                warn!(
                    "get_d3d11va_format: Failed to init hw_frames_ctx {}x{} with sw_format={:?} (error {})",
                    width, height, *sw_format as i32, ret
                );
                av_buffer_unref(&mut (hw_frames_ref as *mut _));
            }
        }

        // All formats failed
        warn!("get_d3d11va_format: All D3D11VA formats failed, falling back to software");
        *fmt
    }

    /// FFI Callback for CUDA format negotiation (NVIDIA CUVID)
    /// CRITICAL: This must set up hw_frames_ctx for proper frame buffer management
    ///
    /// Note: This is only used on macOS where FFmpeg is used for video decoding.
    /// Windows uses native DXVA, Linux uses Vulkan Video.
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    unsafe extern "C" fn get_cuda_format(
        ctx: *mut ffmpeg::ffi::AVCodecContext,
        fmt: *const ffmpeg::ffi::AVPixelFormat,
    ) -> ffmpeg::ffi::AVPixelFormat {
        use ffmpeg::ffi::*;

        // Check if CUDA format is available
        let mut has_cuda = false;
        let mut check_fmt = fmt;
        while *check_fmt != AVPixelFormat::AV_PIX_FMT_NONE {
            if *check_fmt == AVPixelFormat::AV_PIX_FMT_CUDA {
                has_cuda = true;
                break;
            }
            check_fmt = check_fmt.add(1);
        }

        if !has_cuda {
            info!("get_format: CUDA not in available formats, falling back to NV12");
            return AVPixelFormat::AV_PIX_FMT_NV12;
        }

        // We need hw_device_ctx to create hw_frames_ctx
        if (*ctx).hw_device_ctx.is_null() {
            warn!("get_format: hw_device_ctx is null, cannot use CUDA");
            return AVPixelFormat::AV_PIX_FMT_NV12;
        }

        // Check if hw_frames_ctx already exists
        if !(*ctx).hw_frames_ctx.is_null() {
            info!("get_format: hw_frames_ctx already set, selecting CUDA");
            return AVPixelFormat::AV_PIX_FMT_CUDA;
        }

        // Allocate hw_frames_ctx from hw_device_ctx
        let hw_frames_ref = av_hwframe_ctx_alloc((*ctx).hw_device_ctx);
        if hw_frames_ref.is_null() {
            warn!("get_format: Failed to allocate hw_frames_ctx for CUDA");
            return AVPixelFormat::AV_PIX_FMT_NV12;
        }

        // Configure the frames context
        let frames_ctx = (*hw_frames_ref).data as *mut AVHWFramesContext;
        (*frames_ctx).format = AVPixelFormat::AV_PIX_FMT_CUDA;
        (*frames_ctx).sw_format = AVPixelFormat::AV_PIX_FMT_NV12; // CUVID outputs NV12 as software format
        (*frames_ctx).width = (*ctx).coded_width;
        (*frames_ctx).height = (*ctx).coded_height;
        (*frames_ctx).initial_pool_size = 20; // Larger pool for B-frame reordering

        info!(
            "get_format: Configuring CUDA hw_frames_ctx: {}x{}, sw_format=NV12, pool_size=20",
            (*ctx).coded_width,
            (*ctx).coded_height
        );

        // Initialize the frames context
        let ret = av_hwframe_ctx_init(hw_frames_ref);
        if ret < 0 {
            warn!(
                "get_format: Failed to initialize CUDA hw_frames_ctx (error {})",
                ret
            );
            av_buffer_unref(&mut (hw_frames_ref as *mut _));
            return AVPixelFormat::AV_PIX_FMT_NV12;
        }

        // Attach to codec context
        (*ctx).hw_frames_ctx = av_buffer_ref(hw_frames_ref);
        av_buffer_unref(&mut (hw_frames_ref as *mut _));

        info!("get_format: CUDA hw_frames_ctx initialized successfully!");
        AVPixelFormat::AV_PIX_FMT_CUDA
    }

    // Note: VAAPI/Vulkan FFmpeg format callbacks removed - Linux now uses native Vulkan Video decoder

    /// Create decoder, trying hardware acceleration based on preference
    /// (FFmpeg-based, not used on Linux)
    #[cfg(target_os = "macos")]
    fn create_decoder(
        codec_id: ffmpeg::codec::Id,
        backend: VideoDecoderBackend,
    ) -> Result<(decoder::Video, bool)> {
        info!(
            "create_decoder: {:?} with backend preference {:?}",
            codec_id, backend
        );

        // On macOS, try VideoToolbox hardware acceleration
        #[cfg(target_os = "macos")]
        {
            if backend == VideoDecoderBackend::Auto || backend == VideoDecoderBackend::VideoToolbox
            {
                info!("macOS detected - attempting VideoToolbox hardware acceleration");

                // First try to find specific VideoToolbox decoders
                let vt_decoder_name = match codec_id {
                    ffmpeg::codec::Id::AV1 => Some("av1_videotoolbox"),
                    ffmpeg::codec::Id::HEVC => Some("hevc_videotoolbox"),
                    ffmpeg::codec::Id::H264 => Some("h264_videotoolbox"),
                    _ => None,
                };

                if let Some(name) = vt_decoder_name {
                    if let Some(codec) = ffmpeg::codec::decoder::find_by_name(name) {
                        info!("Found specific VideoToolbox decoder: {}", name);

                        // Try to use explicit decoder with hardware context attached
                        // This helps ensure we get VIDEOTOOLBOX frames even without set_get_format
                        let res = unsafe {
                            use ffmpeg::ffi::*;
                            use std::ptr;

                            let mut ctx = CodecContext::new_with_codec(codec);

                            // Create HW device context
                            let mut hw_device_ctx: *mut AVBufferRef = ptr::null_mut();
                            let ret = av_hwdevice_ctx_create(
                                &mut hw_device_ctx,
                                AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                                ptr::null(),
                                ptr::null_mut(),
                                0,
                            );

                            if ret >= 0 && !hw_device_ctx.is_null() {
                                let raw_ctx = ctx.as_mut_ptr();
                                (*raw_ctx).hw_device_ctx = av_buffer_ref(hw_device_ctx);
                                av_buffer_unref(&mut hw_device_ctx);

                                // FORCE VIDEOTOOLBOX FORMAT via callback and simple hint
                                (*raw_ctx).get_format = Some(Self::get_videotoolbox_format);
                                (*raw_ctx).pix_fmt = AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX;
                            }

                            ctx.set_threading(ffmpeg::codec::threading::Config::count(4));
                            ctx.decoder().video()
                        };

                        match res {
                            Ok(decoder) => {
                                info!(
                                    "Specific VideoToolbox decoder ({}) opened successfully",
                                    name
                                );
                                return Ok((decoder, true));
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to open specific VideoToolbox decoder {}: {:?}",
                                    name, e
                                );
                            }
                        }
                    }
                }

                // Fallback: Generic decoder with manual hw_device_ctx attachment
                // Try to set up VideoToolbox hwaccel using FFmpeg's device API
                unsafe {
                    use ffmpeg::ffi::*;
                    use std::ptr;

                    // Find the standard decoder
                    let codec = ffmpeg::codec::decoder::find(codec_id)
                        .ok_or_else(|| anyhow!("Decoder not found for {:?}", codec_id))?;

                    let mut ctx = CodecContext::new_with_codec(codec);

                    // Get raw pointer to AVCodecContext
                    let raw_ctx = ctx.as_mut_ptr();

                    // Create VideoToolbox hardware device context
                    let mut hw_device_ctx: *mut AVBufferRef = ptr::null_mut();
                    let ret = av_hwdevice_ctx_create(
                        &mut hw_device_ctx,
                        AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                        ptr::null(),
                        ptr::null_mut(),
                        0,
                    );

                    if ret >= 0 && !hw_device_ctx.is_null() {
                        // Attach hardware device context to codec context
                        (*raw_ctx).hw_device_ctx = av_buffer_ref(hw_device_ctx);
                        av_buffer_unref(&mut hw_device_ctx);

                        // CRITICAL: Set get_format callback to request VideoToolbox pixel format
                        // Without this, the decoder will output software frames (YUV420P)
                        (*raw_ctx).get_format = Some(Self::get_videotoolbox_format);

                        // Use single thread for lowest latency - multi-threading causes frame reordering delays
                        (*raw_ctx).thread_count = 1;

                        // Low latency flags for streaming (same as Windows D3D11VA)
                        (*raw_ctx).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
                        (*raw_ctx).flags2 |= AV_CODEC_FLAG2_FAST as i32;

                        match ctx.decoder().video() {
                            Ok(decoder) => {
                                info!("VideoToolbox hardware decoder created successfully (generic + hw_device + get_format)");
                                return Ok((decoder, true));
                            }
                            Err(e) => {
                                warn!("Failed to open VideoToolbox decoder: {:?}", e);
                            }
                        }
                    } else {
                        warn!(
                            "Failed to create VideoToolbox device context (error {})",
                            ret
                        );
                    }
                }
            } else {
                info!("VideoToolbox disabled by preference: {:?}", backend);
            }
        }

        // Platform-specific hardware decoders (Windows/Linux)
        #[cfg(not(target_os = "macos"))]
        {
            // Windows hardware decoder selection
            // Priority: D3D11VA (zero-copy, fastest) > CUVID (NVIDIA fallback) > QSV (Intel)
            // Based on NVIDIA GeForce NOW client analysis: they use DXVADecoder (D3D11VA) on all GPUs
            #[cfg(target_os = "windows")]
            if backend != VideoDecoderBackend::Software {
                let gpu_vendor = detect_gpu_vendor();

                // Try D3D11VA first for all GPUs including NVIDIA
                // NVIDIA's own GeForce NOW client uses DXVADecoder (D3D11 DXVA2), not CUVID
                // D3D11VA provides zero-copy GPU textures which are faster than CUVID's GPU→CPU transfer
                // If D3D11VA fails, we fall back to CUVID for NVIDIA or QSV for Intel
                let try_d3d11va =
                    backend == VideoDecoderBackend::Dxva || backend == VideoDecoderBackend::Auto;

                // Try D3D11VA for AMD and Intel GPUs - provides zero-copy texture path
                // SKIP D3D11VA on NVIDIA: FFmpeg's D3D11VA implementation has issues with NVIDIA drivers
                // NVIDIA GPUs should use CUVID instead (more reliable, better tested)
                // The issue is that D3D11VA "opens" successfully but then hw_frames_ctx creation fails
                // during actual decoding with error 80070057, and by then it's too late to fall back
                let skip_d3d11va_for_nvidia = matches!(gpu_vendor, GpuVendor::Nvidia);

                if try_d3d11va && !skip_d3d11va_for_nvidia {
                    info!(
                        "Attempting D3D11VA hardware acceleration (GPU: {:?}) - zero-copy mode",
                        gpu_vendor
                    );

                    let codec = ffmpeg::codec::decoder::find(codec_id)
                        .ok_or_else(|| anyhow!("Decoder not found for {:?}", codec_id));

                    if let Ok(codec) = codec {
                        // Try multiple D3D11VA initialization approaches
                        // Approach 1: Let FFmpeg create the D3D11VA device (most compatible)
                        // Approach 2: Create our own D3D11 device with VIDEO_SUPPORT (fallback)

                        // Approach 1: FFmpeg-managed D3D11VA device
                        // This is more compatible with NVIDIA drivers as FFmpeg handles
                        // the device creation with proper flags internally
                        let result = unsafe {
                            use ffmpeg::ffi::*;
                            use std::ptr;

                            let mut ctx = CodecContext::new_with_codec(codec);
                            let raw_ctx = ctx.as_mut_ptr();

                            // Let FFmpeg create the D3D11VA device context
                            // This is more compatible than creating our own device
                            let mut hw_device_ctx: *mut AVBufferRef = ptr::null_mut();
                            let ret = av_hwdevice_ctx_create(
                                &mut hw_device_ctx,
                                AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                                ptr::null(),     // Use default device
                                ptr::null_mut(), // No options
                                0,
                            );

                            if ret >= 0 && !hw_device_ctx.is_null() {
                                info!("D3D11VA hw_device_ctx created by FFmpeg (automatic device)");

                                (*raw_ctx).hw_device_ctx = av_buffer_ref(hw_device_ctx);
                                av_buffer_unref(&mut hw_device_ctx);

                                // Set format callback to select D3D11 pixel format and create hw_frames_ctx
                                (*raw_ctx).get_format = Some(Self::get_d3d11va_format);

                                // Low latency flags for streaming
                                (*raw_ctx).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
                                (*raw_ctx).flags2 |= AV_CODEC_FLAG2_FAST as i32;
                                (*raw_ctx).thread_count = 1; // Single thread for lowest latency

                                ctx.decoder().video()
                            } else {
                                warn!("FFmpeg failed to create D3D11VA device context (error {}), trying manual creation...", ret);
                                Err(ffmpeg::Error::Bug)
                            }
                        };

                        match result {
                            Ok(decoder) => {
                                info!("D3D11VA hardware decoder opened successfully (FFmpeg device) - zero-copy GPU decoding active!");
                                return Ok((decoder, true));
                            }
                            Err(_) => {
                                // Approach 2: Create our own D3D11 device with VIDEO_SUPPORT flag
                                // This may work better on some systems
                                info!("Trying D3D11VA with custom device creation...");

                                let result2 = unsafe {
                                    use ffmpeg::ffi::*;
                                    use windows::core::Interface;
                                    use windows::Win32::Foundation::HMODULE;
                                    use windows::Win32::Graphics::Direct3D::*;
                                    use windows::Win32::Graphics::Direct3D11::*;

                                    // Create D3D11 device with VIDEO_SUPPORT flag
                                    let mut device: Option<ID3D11Device> = None;
                                    let mut context: Option<ID3D11DeviceContext> = None;
                                    let mut feature_level = D3D_FEATURE_LEVEL_11_0;

                                    let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT
                                        | D3D11_CREATE_DEVICE_BGRA_SUPPORT;

                                    let hr = D3D11CreateDevice(
                                        None, // Default adapter
                                        D3D_DRIVER_TYPE_HARDWARE,
                                        HMODULE::default(),
                                        flags,
                                        Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                                        D3D11_SDK_VERSION,
                                        Some(&mut device),
                                        Some(&mut feature_level),
                                        Some(&mut context),
                                    );

                                    if hr.is_err() || device.is_none() {
                                        warn!("Failed to create D3D11 device with video support: {:?}", hr);
                                        return Err(anyhow!("Failed to create D3D11 device"));
                                    }

                                    let device = device.unwrap();
                                    info!("Created custom D3D11 device with VIDEO_SUPPORT (feature level: {:?})", feature_level);

                                    // Enable multithread protection
                                    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                                        mt.SetMultithreadProtected(true);
                                    }

                                    // Allocate hw_device_ctx and configure with our device
                                    let hw_device_ref = av_hwdevice_ctx_alloc(
                                        AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                                    );
                                    if hw_device_ref.is_null() {
                                        warn!("Failed to allocate D3D11VA device context");
                                        return Err(anyhow!(
                                            "Failed to allocate D3D11VA device context"
                                        ));
                                    }

                                    // Get the D3D11VA device context and set our device
                                    let hw_device_ctx =
                                        (*hw_device_ref).data as *mut AVHWDeviceContext;
                                    let d3d11_device_hwctx =
                                        (*hw_device_ctx).hwctx as *mut *mut std::ffi::c_void;

                                    // Set the device pointer (first field of AVD3D11VADeviceContext)
                                    *d3d11_device_hwctx = std::mem::transmute_copy(&device);
                                    std::mem::forget(device); // FFmpeg owns it now

                                    let ret = av_hwdevice_ctx_init(hw_device_ref);
                                    if ret < 0 {
                                        warn!("Failed to initialize D3D11VA device context (error {})", ret);
                                        av_buffer_unref(&mut (hw_device_ref as *mut _));
                                        return Err(anyhow!(
                                            "Failed to initialize D3D11VA device context"
                                        ));
                                    }

                                    info!("D3D11VA hw_device_ctx initialized with custom video device");

                                    let mut ctx = CodecContext::new_with_codec(codec);
                                    let raw_ctx = ctx.as_mut_ptr();

                                    (*raw_ctx).hw_device_ctx = av_buffer_ref(hw_device_ref);
                                    av_buffer_unref(&mut (hw_device_ref as *mut _));

                                    (*raw_ctx).get_format = Some(Self::get_d3d11va_format);
                                    (*raw_ctx).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
                                    (*raw_ctx).flags2 |= AV_CODEC_FLAG2_FAST as i32;
                                    (*raw_ctx).thread_count = 1;

                                    ctx.decoder().video()
                                };

                                match result2 {
                                    Ok(decoder) => {
                                        info!("D3D11VA hardware decoder opened successfully (custom device) - zero-copy GPU decoding active!");
                                        return Ok((decoder, true));
                                    }
                                    Err(e) => {
                                        warn!("D3D11VA decoder failed to open: {:?}, trying other backends...", e);
                                    }
                                }
                            }
                        }
                    }
                }

                // Try dedicated hardware decoders (CUVID/QSV)
                // CUVID for NVIDIA, QSV for Intel - these are the most reliable options
                // Always try these as fallback if D3D11VA failed above
                let qsv_available = check_qsv_available();

                // Don't try NVIDIA CUVID decoders on non-NVIDIA GPUs (causes libnvcuvid load errors)
                let is_nvidia = matches!(gpu_vendor, GpuVendor::Nvidia);
                let is_intel = matches!(gpu_vendor, GpuVendor::Intel);

                // If user selected DXVA but it failed, still try CUVID/QSV as fallback
                let try_hw_fallback = backend != VideoDecoderBackend::Software;

                // Build prioritized list of hardware decoders to try
                // Include CUVID/QSV as fallback even if user selected DXVA (since D3D11VA may fail)
                let hw_decoders: Vec<&str> = match codec_id {
                    ffmpeg::codec::Id::H264 => {
                        let mut list = Vec::new();
                        // NVIDIA CUVID (most reliable for NVIDIA, also fallback if DXVA failed)
                        if is_nvidia && try_hw_fallback {
                            list.push("h264_cuvid");
                        }
                        // Intel QSV (with codec-specific capability check for older GPUs)
                        if (is_intel && is_qsv_supported_for_codec(codec_id) && try_hw_fallback)
                            || backend == VideoDecoderBackend::Qsv
                        {
                            list.push("h264_qsv");
                        }
                        // AMD AMF (if available)
                        if gpu_vendor == GpuVendor::Amd && try_hw_fallback {
                            list.push("h264_amf");
                        }
                        list
                    }
                    ffmpeg::codec::Id::HEVC => {
                        let mut list = Vec::new();
                        // NVIDIA CUVID (most reliable for NVIDIA, also fallback if DXVA failed)
                        if is_nvidia && try_hw_fallback {
                            list.push("hevc_cuvid");
                        }
                        // Intel QSV (with codec-specific capability check - HD 4000 doesn't support HEVC)
                        if (is_intel && is_qsv_supported_for_codec(codec_id) && try_hw_fallback)
                            || backend == VideoDecoderBackend::Qsv
                        {
                            list.push("hevc_qsv");
                        }
                        // AMD AMF (if available)
                        if gpu_vendor == GpuVendor::Amd && try_hw_fallback {
                            list.push("hevc_amf");
                        }
                        list
                    }
                    _ => vec![],
                };

                info!(
                    "Hardware decoders to try: {:?} (GPU: {:?}, backend: {:?})",
                    hw_decoders, gpu_vendor, backend
                );

                // Try each hardware decoder in order
                for decoder_name in &hw_decoders {
                    if let Some(hw_codec) = ffmpeg::codec::decoder::find_by_name(decoder_name) {
                        info!(
                            "Found hardware decoder: {}, attempting to open...",
                            decoder_name
                        );

                        // For CUVID decoders, we need CUDA device context with proper hw_frames_ctx
                        if decoder_name.contains("cuvid") {
                            let result = unsafe {
                                use ffmpeg::ffi::*;
                                use std::ptr;

                                let mut ctx = CodecContext::new_with_codec(hw_codec);
                                let raw_ctx = ctx.as_mut_ptr();

                                // Create CUDA device context for CUVID
                                let mut hw_device_ctx: *mut AVBufferRef = ptr::null_mut();
                                let ret = av_hwdevice_ctx_create(
                                    &mut hw_device_ctx,
                                    AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                                    ptr::null(),
                                    ptr::null_mut(),
                                    0,
                                );

                                if ret >= 0 && !hw_device_ctx.is_null() {
                                    (*raw_ctx).hw_device_ctx = av_buffer_ref(hw_device_ctx);
                                    av_buffer_unref(&mut hw_device_ctx);
                                    (*raw_ctx).get_format = Some(Self::get_cuda_format);

                                    // CRITICAL: Set thread_count=1 for CUVID to prevent frame reordering issues
                                    // Multi-threaded decoding can cause frames to arrive out of order,
                                    // leading to visual corruption when reference frames are missing
                                    (*raw_ctx).thread_count = 1;
                                } else {
                                    warn!("Failed to create CUDA device context (error {}), CUVID may not work", ret);
                                }

                                // Set low latency flags for streaming
                                (*raw_ctx).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
                                (*raw_ctx).flags2 |= AV_CODEC_FLAG2_FAST as i32;

                                ctx.decoder().video()
                            };

                            match result {
                                Ok(decoder) => {
                                    info!("CUVID hardware decoder ({}) opened successfully - GPU decoding active!", decoder_name);
                                    return Ok((decoder, true));
                                }
                                Err(e) => {
                                    warn!("Failed to open CUVID decoder {}: {:?}", decoder_name, e);
                                }
                            }
                        } else {
                            // For QSV and other decoders, just open directly
                            let mut ctx = CodecContext::new_with_codec(hw_codec);

                            unsafe {
                                let raw_ctx = ctx.as_mut_ptr();
                                // Set low latency flags
                                (*raw_ctx).flags |= ffmpeg::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
                                (*raw_ctx).flags2 |= ffmpeg::ffi::AV_CODEC_FLAG2_FAST as i32;
                            }

                            match ctx.decoder().video() {
                                Ok(decoder) => {
                                    info!("Hardware decoder ({}) opened successfully - GPU decoding active!", decoder_name);
                                    return Ok((decoder, true));
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to open hardware decoder {}: {:?}",
                                        decoder_name, e
                                    );
                                }
                            }
                        }
                    } else {
                        debug!("Hardware decoder not found in FFmpeg: {}", decoder_name);
                    }
                }

                warn!("All hardware decoders failed, will use software decoder");
            }

            // Note: Linux hardware decoder handling uses GStreamer via new_async() instead of FFmpeg.
            // See gstreamer_decoder.rs for the implementation.
        }

        // Fall back to software decoder
        info!("Using software decoder for {:?}", codec_id);
        let codec = ffmpeg::codec::decoder::find(codec_id)
            .ok_or_else(|| anyhow!("Decoder not found for {:?}", codec_id))?;
        info!("Found software decoder: {:?}", codec.name());

        let mut ctx = CodecContext::new_with_codec(codec);

        // Use fewer threads on low-power devices to reduce memory usage
        let gpu_vendor = detect_gpu_vendor();
        let thread_count = if matches!(gpu_vendor, GpuVendor::Broadcom) {
            // Raspberry Pi: Use 2 threads to avoid memory overflow
            // Pi 5 has 4 cores but limited RAM bandwidth
            info!("Raspberry Pi detected: Using 2 decoder threads to conserve memory");
            2
        } else {
            // Desktop/laptop: Use 4 threads for better performance
            4
        };
        ctx.set_threading(ffmpeg::codec::threading::Config::count(thread_count));

        let decoder = ctx.decoder().video()?;
        info!(
            "Software decoder opened successfully with {} threads",
            thread_count
        );
        Ok((decoder, false))
    }

    /// Check if a pixel format is a hardware format (FFmpeg, not used on Linux)
    #[cfg(target_os = "macos")]
    fn is_hw_pixel_format(format: Pixel) -> bool {
        // Check common hardware formats
        // Note: Some formats may not be available depending on FFmpeg build configuration
        if matches!(
            format,
            Pixel::VIDEOTOOLBOX
                | Pixel::CUDA
                | Pixel::VDPAU
                | Pixel::QSV
                | Pixel::D3D11
                | Pixel::DXVA2_VLD
                | Pixel::D3D11VA_VLD
                | Pixel::VULKAN
        ) {
            return true;
        }

        // VAAPI/Vulkan check by name since these may not exist in all ffmpeg-next builds
        let format_name = format!("{:?}", format);
        format_name.contains("VAAPI") || format_name.contains("VULKAN")
    }

    /// Transfer hardware frame to system memory if needed (FFmpeg, not used on Linux)
    #[cfg(target_os = "macos")]
    fn transfer_hw_frame_if_needed(frame: &FfmpegFrame) -> Option<FfmpegFrame> {
        let format = frame.format();

        if !Self::is_hw_pixel_format(format) {
            // Not a hardware frame, no transfer needed
            return None;
        }

        unsafe {
            use ffmpeg::ffi::*;

            // Create a new frame for the software copy
            let sw_frame_ptr = av_frame_alloc();
            if sw_frame_ptr.is_null() {
                warn!("Failed to allocate software frame");
                return None;
            }

            // Transfer data from hardware frame to software frame
            // This is the main latency source - GPU to CPU copy
            let ret = av_hwframe_transfer_data(sw_frame_ptr, frame.as_ptr(), 0);
            if ret < 0 {
                warn!(
                    "Failed to transfer hardware frame to software (error {})",
                    ret
                );
                av_frame_free(&mut (sw_frame_ptr as *mut _));
                return None;
            }

            // Copy frame properties
            (*sw_frame_ptr).width = frame.width() as i32;
            (*sw_frame_ptr).height = frame.height() as i32;

            // Wrap in FFmpeg frame type
            Some(FfmpegFrame::wrap(sw_frame_ptr))
        }
    }

    /// Calculate 256-byte aligned stride for GPU compatibility (wgpu/DX12 requirement)
    #[cfg(target_os = "macos")]
    fn get_aligned_stride(width: u32) -> u32 {
        (width + 255) & !255
    }

    /// Decode a single frame (called in decoder thread) (FFmpeg, not used on Linux)
    /// `in_recovery` suppresses repeated warnings when waiting for keyframe
    #[cfg(target_os = "macos")]
    fn decode_frame(
        decoder: &mut decoder::Video,
        scaler: &mut Option<ScalerContext>,
        width: &mut u32,
        height: &mut u32,
        frames_decoded: &mut u64,
        data: &[u8],
        codec_id: ffmpeg::codec::Id,
        in_recovery: bool,
    ) -> Option<VideoFrame> {
        // AV1 uses OBUs directly, no start codes needed
        // H.264/H.265 need Annex B start codes (0x00 0x00 0x00 0x01)
        let data = if codec_id == ffmpeg::codec::Id::AV1 {
            // AV1 - use data as-is (OBU format)
            data.to_vec()
        } else if data.len() >= 4 && data[0..4] == [0, 0, 0, 1] {
            data.to_vec()
        } else if data.len() >= 3 && data[0..3] == [0, 0, 1] {
            data.to_vec()
        } else {
            // Add start code for H.264/H.265
            let mut with_start = vec![0, 0, 0, 1];
            with_start.extend_from_slice(data);
            with_start
        };

        // Create packet
        let mut packet = Packet::new(data.len());
        if let Some(pkt_data) = packet.data_mut() {
            pkt_data.copy_from_slice(&data);
        } else {
            warn!("Failed to allocate packet data");
            return None;
        }

        // Send packet to decoder
        if let Err(e) = decoder.send_packet(&packet) {
            // EAGAIN means we need to receive frames first
            match e {
                ffmpeg::Error::Other { errno } if errno == libc::EAGAIN => {}
                _ => {
                    // Suppress repeated warnings during keyframe recovery
                    if in_recovery {
                        debug!("Send packet error (waiting for keyframe): {:?}", e);
                    } else {
                        warn!("Send packet error: {:?}", e);
                    }
                }
            }
        }

        // Try to receive decoded frame
        let mut frame = FfmpegFrame::empty();
        match decoder.receive_frame(&mut frame) {
            Ok(_) => {
                *frames_decoded += 1;

                let w = frame.width();
                let h = frame.height();
                let format = frame.format();

                // Extract color metadata from original frame
                let color_range = match frame.color_range() {
                    ffmpeg::util::color::range::Range::JPEG => ColorRange::Full,
                    ffmpeg::util::color::range::Range::MPEG => ColorRange::Limited,
                    _ => ColorRange::Limited,
                };

                let color_space = match frame.color_space() {
                    ffmpeg::util::color::space::Space::BT709 => ColorSpace::BT709,
                    ffmpeg::util::color::space::Space::BT470BG => ColorSpace::BT601,
                    ffmpeg::util::color::space::Space::SMPTE170M => ColorSpace::BT601,
                    ffmpeg::util::color::space::Space::BT2020NCL => ColorSpace::BT2020,
                    _ => ColorSpace::BT709,
                };

                // Detect transfer function (SDR gamma vs HDR PQ/HLG)
                let transfer_function = match frame.color_transfer_characteristic() {
                    ffmpeg::util::color::TransferCharacteristic::SMPTE2084 => TransferFunction::PQ,
                    ffmpeg::util::color::TransferCharacteristic::ARIB_STD_B67 => {
                        TransferFunction::HLG
                    }
                    _ => TransferFunction::SDR,
                };

                // Log color metadata on first frame
                if *frames_decoded == 1 {
                    info!(
                        "First frame color info: space={:?}, range={:?}, transfer={:?} (raw: {:?})",
                        color_space,
                        color_range,
                        transfer_function,
                        frame.color_transfer_characteristic()
                    );
                }

                // ZERO-COPY PATH: For VideoToolbox, extract CVPixelBuffer directly
                // This skips the expensive GPU->CPU->GPU copy entirely
                #[cfg(target_os = "macos")]
                if format == Pixel::VIDEOTOOLBOX {
                    use crate::media::videotoolbox;
                    use std::sync::Arc;

                    // Extract CVPixelBuffer from frame.data[3] using raw FFmpeg pointer
                    // We use unsafe FFI because the safe wrapper does bounds checking
                    // that doesn't work for hardware frames
                    let cv_buffer = unsafe {
                        let raw_frame = frame.as_ptr();
                        let data_ptr = (*raw_frame).data[3] as *mut u8;
                        if !data_ptr.is_null() {
                            videotoolbox::extract_cv_pixel_buffer_from_data(data_ptr)
                        } else {
                            None
                        }
                    };

                    if let Some(buffer) = cv_buffer {
                        if *frames_decoded == 1 {
                            info!(
                                "ZERO-COPY: First frame {}x{} via CVPixelBuffer (no CPU transfer!)",
                                w, h
                            );
                        }

                        *width = w;
                        *height = h;

                        return Some(VideoFrame {
                            frame_id: super::next_frame_id(),
                            width: w,
                            height: h,
                            y_plane: Vec::new(),
                            u_plane: Vec::new(),
                            v_plane: Vec::new(),
                            y_stride: 0,
                            u_stride: 0,
                            v_stride: 0,
                            timestamp_us: 0,
                            format: PixelFormat::NV12,
                            color_range,
                            color_space,
                            transfer_function,
                            gpu_frame: Some(Arc::new(buffer)),
                        });
                    } else {
                        warn!("Failed to extract CVPixelBuffer, falling back to CPU transfer");
                    }
                }

                // ZERO-COPY PATH: For D3D11VA, extract D3D11 texture directly
                // This skips the expensive GPU->CPU->GPU copy entirely
                #[cfg(target_os = "windows")]
                if format == Pixel::D3D11 || format == Pixel::D3D11VA_VLD {
                    use crate::media::d3d11;
                    use std::sync::Arc;

                    // Extract D3D11 texture from frame data
                    // FFmpeg D3D11VA frame layout:
                    // - data[0] = ID3D11Texture2D*
                    // - data[1] = texture array index (as intptr_t)
                    let d3d11_texture = unsafe {
                        let raw_frame = frame.as_ptr();
                        let data0 = (*raw_frame).data[0] as *mut u8;
                        let data1 = (*raw_frame).data[1] as *mut u8;
                        d3d11::extract_d3d11_texture_from_frame(data0, data1)
                    };

                    if let Some(texture) = d3d11_texture {
                        if *frames_decoded == 1 {
                            info!(
                                "ZERO-COPY: First frame {}x{} via D3D11 texture (no CPU transfer!)",
                                w, h
                            );
                        }

                        *width = w;
                        *height = h;

                        return Some(VideoFrame {
                            frame_id: super::next_frame_id(),
                            width: w,
                            height: h,
                            y_plane: Vec::new(),
                            u_plane: Vec::new(),
                            v_plane: Vec::new(),
                            y_stride: 0,
                            u_stride: 0,
                            v_stride: 0,
                            timestamp_us: 0,
                            format: PixelFormat::NV12,
                            color_range,
                            color_space,
                            transfer_function,
                            gpu_frame: Some(Arc::new(texture)),
                        });
                    } else {
                        warn!("Failed to extract D3D11 texture, falling back to CPU transfer");
                    }
                }

                // ZERO-COPY PATH: For VAAPI, extract VA surface directly
                // This skips the expensive GPU->CPU->GPU copy entirely
                #[cfg(target_os = "linux")]
                if format!("{:?}", format).contains("VAAPI") {
                    use crate::media::vaapi;
                    use std::sync::Arc;

                    // Extract VAAPI surface from frame data
                    // FFmpeg VAAPI frame layout:
                    // - data[3] = VASurfaceID (as pointer-sized value)
                    // - hw_frames_ctx->device_ctx->hwctx = VADisplay
                    let vaapi_surface = unsafe {
                        let raw_frame = frame.as_ptr();
                        let data3 = (*raw_frame).data[3] as *mut u8;

                        // Get VADisplay from hw_frames_ctx
                        let hw_frames_ctx = (*raw_frame).hw_frames_ctx;
                        let va_display = if !hw_frames_ctx.is_null() {
                            let frames_ctx =
                                (*hw_frames_ctx).data as *mut ffmpeg::ffi::AVHWFramesContext;
                            if !frames_ctx.is_null() {
                                let device_ctx = (*frames_ctx).device_ctx;
                                if !device_ctx.is_null() {
                                    (*device_ctx).hwctx as *mut std::ffi::c_void
                                } else {
                                    std::ptr::null_mut()
                                }
                            } else {
                                std::ptr::null_mut()
                            }
                        } else {
                            std::ptr::null_mut()
                        };

                        vaapi::extract_vaapi_surface_from_frame(data3, va_display, w, h)
                    };

                    if let Some(surface) = vaapi_surface {
                        if *frames_decoded == 1 {
                            info!(
                                "ZERO-COPY: First frame {}x{} via VAAPI surface (no CPU transfer!)",
                                w, h
                            );
                        }

                        *width = w;
                        *height = h;

                        return Some(VideoFrame {
                            frame_id: super::next_frame_id(),
                            width: w,
                            height: h,
                            y_plane: Vec::new(),
                            u_plane: Vec::new(),
                            v_plane: Vec::new(),
                            y_stride: 0,
                            u_stride: 0,
                            v_stride: 0,
                            timestamp_us: 0,
                            format: PixelFormat::NV12,
                            color_range,
                            color_space,
                            transfer_function,
                            gpu_frame: Some(Arc::new(surface)),
                        });
                    } else {
                        warn!("Failed to extract VAAPI surface, falling back to CPU transfer");
                    }
                }

                // FALLBACK: Transfer hardware frame to CPU memory
                let sw_frame = Self::transfer_hw_frame_if_needed(&frame);
                let frame_to_use = sw_frame.as_ref().unwrap_or(&frame);
                let actual_format = frame_to_use.format();

                if *frames_decoded == 1 {
                    info!("First decoded frame: {}x{}, format: {:?} (hw: {:?}), range: {:?}, space: {:?}, transfer: {:?}",
                        w, h, actual_format, format, color_range, color_space, transfer_function);
                }

                // Check if frame is NV12 - skip CPU scaler and pass directly to GPU
                // NV12 has Y plane (full res) and UV plane (half res, interleaved)
                // GPU shader will handle color conversion - much faster than CPU scaler
                if actual_format == Pixel::NV12 {
                    *width = w;
                    *height = h;

                    let y_stride = frame_to_use.stride(0) as u32;
                    let uv_stride = frame_to_use.stride(1) as u32;
                    let uv_height = h / 2;

                    let y_data = frame_to_use.data(0);
                    let uv_data = frame_to_use.data(1);

                    // Check if we actually have data
                    if y_data.is_empty() || uv_data.is_empty() || y_stride == 0 {
                        warn!(
                            "NV12 frame has empty data: y_len={}, uv_len={}, y_stride={}",
                            y_data.len(),
                            uv_data.len(),
                            y_stride
                        );
                        // Fall through to scaler path
                    } else {
                        // GPU texture upload requires 256-byte aligned rows (wgpu restriction)
                        let aligned_y_stride = Self::get_aligned_stride(w);
                        let aligned_uv_stride = Self::get_aligned_stride(w);

                        if *frames_decoded == 1 {
                            info!("NV12 direct path: {}x{}, y_stride={}, uv_stride={}, y_len={}, uv_len={}",
                                w, h, y_stride, uv_stride, y_data.len(), uv_data.len());
                        }

                        // Optimized copy - fast path when strides match
                        let copy_plane_fast = |src: &[u8],
                                               src_stride: u32,
                                               dst_stride: u32,
                                               copy_width: u32,
                                               height: u32|
                         -> Vec<u8> {
                            let total_size = (dst_stride * height) as usize;
                            if src_stride == dst_stride && src.len() >= total_size {
                                // Fast path: single memcpy
                                src[..total_size].to_vec()
                            } else {
                                // Slow path: row-by-row
                                let mut dst = vec![0u8; total_size];
                                for row in 0..height as usize {
                                    let src_start = row * src_stride as usize;
                                    let src_end = src_start + copy_width as usize;
                                    let dst_start = row * dst_stride as usize;
                                    if src_end <= src.len() {
                                        dst[dst_start..dst_start + copy_width as usize]
                                            .copy_from_slice(&src[src_start..src_end]);
                                    }
                                }
                                dst
                            }
                        };

                        let y_plane = copy_plane_fast(y_data, y_stride, aligned_y_stride, w, h);
                        let uv_plane =
                            copy_plane_fast(uv_data, uv_stride, aligned_uv_stride, w, uv_height);

                        if *frames_decoded == 1 {
                            info!("NV12 direct GPU path: {}x{} - bypassing CPU scaler (y={} bytes, uv={} bytes)",
                                w, h, y_plane.len(), uv_plane.len());
                        }

                        return Some(VideoFrame {
                            frame_id: super::next_frame_id(),
                            width: w,
                            height: h,
                            y_plane,
                            u_plane: uv_plane,
                            v_plane: Vec::new(),
                            y_stride: aligned_y_stride,
                            u_stride: aligned_uv_stride,
                            v_stride: 0,
                            timestamp_us: 0,
                            format: PixelFormat::NV12,
                            color_range,
                            color_space,
                            transfer_function,
                            #[cfg(target_os = "macos")]
                            gpu_frame: None,
                            #[cfg(target_os = "windows")]
                            gpu_frame: None,
                            #[cfg(target_os = "linux")]
                            gpu_frame: None,
                        });
                    }
                }

                // For other formats, use scaler to convert to NV12
                // NV12 is more efficient for GPU upload and hardware decoders at high bitrates
                // Use POINT (nearest neighbor) since we're not resizing - just color format conversion
                // This is much faster than BILINEAR for same-size conversion
                if scaler.is_none() || *width != w || *height != h {
                    *width = w;
                    *height = h;

                    info!(
                        "Creating scaler: {:?} {}x{} -> NV12 {}x{} (POINT mode)",
                        actual_format, w, h, w, h
                    );

                    match ScalerContext::get(
                        actual_format,
                        w,
                        h,
                        Pixel::NV12,
                        w,
                        h,
                        ScalerFlags::POINT, // Fastest - no interpolation needed for same-size conversion
                    ) {
                        Ok(s) => *scaler = Some(s),
                        Err(e) => {
                            warn!("Failed to create scaler: {:?}", e);
                            return None;
                        }
                    }
                }

                // Convert to NV12
                // We must allocate the destination frame first!
                let mut nv12_frame = FfmpegFrame::new(Pixel::NV12, w, h);

                if let Some(ref mut s) = scaler {
                    if let Err(e) = s.run(frame_to_use, &mut nv12_frame) {
                        warn!("Scaler run failed: {:?}", e);
                        return None;
                    }
                } else {
                    return None;
                }

                // Extract NV12 planes with alignment
                // NV12: Y plane (full res) + UV plane (half height, interleaved)
                let y_stride = nv12_frame.stride(0) as u32;
                let uv_stride = nv12_frame.stride(1) as u32;

                let aligned_y_stride = Self::get_aligned_stride(w);
                let aligned_uv_stride = Self::get_aligned_stride(w);

                let uv_height = h / 2;

                // Optimized plane copy - use bulk copy when strides match, row-by-row otherwise
                let copy_plane_optimized = |src: &[u8],
                                            src_stride: u32,
                                            dst_stride: u32,
                                            width: u32,
                                            height: u32|
                 -> Vec<u8> {
                    let total_size = (dst_stride * height) as usize;

                    // Fast path: if source stride equals destination stride AND covers the data we need,
                    // we can do a single memcpy
                    if src_stride == dst_stride && src.len() >= total_size {
                        src[..total_size].to_vec()
                    } else {
                        // Slow path: row-by-row copy with stride conversion
                        let mut dst = vec![0u8; total_size];
                        let width = width as usize;
                        let src_stride = src_stride as usize;
                        let dst_stride = dst_stride as usize;

                        for row in 0..height as usize {
                            let src_start = row * src_stride;
                            let src_end = src_start + width;
                            let dst_start = row * dst_stride;
                            if src_end <= src.len() {
                                dst[dst_start..dst_start + width]
                                    .copy_from_slice(&src[src_start..src_end]);
                            }
                        }
                        dst
                    }
                };

                Some(VideoFrame {
                    frame_id: super::next_frame_id(),
                    width: w,
                    height: h,
                    y_plane: copy_plane_optimized(
                        nv12_frame.data(0),
                        y_stride,
                        aligned_y_stride,
                        w,
                        h,
                    ),
                    u_plane: copy_plane_optimized(
                        nv12_frame.data(1),
                        uv_stride,
                        aligned_uv_stride,
                        w,
                        uv_height,
                    ),
                    v_plane: Vec::new(), // NV12 has no separate V plane
                    y_stride: aligned_y_stride,
                    u_stride: aligned_uv_stride,
                    v_stride: 0,
                    timestamp_us: 0,
                    format: PixelFormat::NV12,
                    color_range,
                    color_space,
                    transfer_function,
                    #[cfg(target_os = "macos")]
                    gpu_frame: None,
                    #[cfg(target_os = "windows")]
                    gpu_frame: None,
                    #[cfg(target_os = "linux")]
                    gpu_frame: None,
                })
            }
            Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => None,
            Err(e) => {
                debug!("Receive frame error: {:?}", e);
                None
            }
        }
    }

    /// Decode a NAL unit - sends to decoder thread and receives result
    /// WARNING: This is BLOCKING and will stall the calling thread!
    /// For low-latency streaming, use `decode_async()` instead.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<VideoFrame>> {
        // Send decode command
        self.cmd_tx
            .send(DecoderCommand::Decode(data.to_vec()))
            .map_err(|_| anyhow!("Decoder thread closed"))?;

        // Receive result (blocking)
        match self.frame_rx.recv() {
            Ok(frame) => {
                if frame.is_some() {
                    self.frames_decoded += 1;
                }
                Ok(frame)
            }
            Err(_) => Err(anyhow!("Decoder thread closed")),
        }
    }

    /// Decode a NAL unit asynchronously - fire and forget
    /// The decoded frame will be written directly to the SharedFrame.
    /// Stats are sent via the stats channel returned from `new_async()`.
    ///
    /// This method NEVER blocks the calling thread, making it ideal for
    /// the main streaming loop where input responsiveness is critical.
    pub fn decode_async(&mut self, data: &[u8], receive_time: std::time::Instant) -> Result<()> {
        self.cmd_tx
            .send(DecoderCommand::DecodeAsync {
                data: data.to_vec(),
                receive_time,
            })
            .map_err(|_| anyhow!("Decoder thread closed"))?;

        self.frames_decoded += 1; // Optimistic count
        Ok(())
    }

    /// Check if using hardware acceleration
    pub fn is_hw_accelerated(&self) -> bool {
        self.hw_accel
    }

    /// Get number of frames decoded
    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        // Signal decoder thread to stop
        let _ = self.cmd_tx.send(DecoderCommand::Stop);
    }
}

// ============================================================================
// Unified Video Decoder - Wraps FFmpeg or Native DXVA decoder
// ============================================================================

/// Unified video decoder that can use either FFmpeg or native DXVA backend
///
/// This enum provides a common interface for decoder types, allowing
/// the streaming code to use the appropriate backend transparently.
/// - Windows: GStreamer D3D11 for H.264, Native DXVA for HEVC
/// - macOS: FFmpeg with VideoToolbox
/// - Linux: Handled separately via Vulkan Video or GStreamer
#[cfg(target_os = "windows")]
pub enum UnifiedVideoDecoder {
    /// Native D3D11 Video decoder (HEVC only, NVIDIA-style)
    Native(super::native_video::NativeVideoDecoder),
    /// GStreamer D3D11 decoder (H.264, with hardware acceleration)
    GStreamer(GStreamerDecoderWrapper),
}

/// Wrapper for GStreamer decoder with async interface
#[cfg(target_os = "windows")]
pub struct GStreamerDecoderWrapper {
    decoder: super::gstreamer_decoder::GStreamerDecoder,
    shared_frame: Arc<SharedFrame>,
    stats_tx: tokio_mpsc::Sender<DecodeStats>,
    frames_decoded: u64,
    /// Track consecutive failures for keyframe request
    consecutive_failures: u32,
}

#[cfg(target_os = "macos")]
pub enum UnifiedVideoDecoder {
    /// FFmpeg-based decoder with VideoToolbox
    Ffmpeg(VideoDecoder),
}

#[cfg(target_os = "linux")]
pub enum UnifiedVideoDecoder {
    /// Linux uses Vulkan Video or GStreamer (placeholder for unified interface)
    Ffmpeg(VideoDecoder),
}

impl UnifiedVideoDecoder {
    /// Create a new unified decoder with the specified backend
    pub fn new_async(
        codec: VideoCodec,
        backend: VideoDecoderBackend,
        shared_frame: Arc<SharedFrame>,
    ) -> Result<(Self, tokio_mpsc::Receiver<DecodeStats>)> {
        // Windows: Use GStreamer D3D11 by default, Native DXVA only for HEVC when explicitly selected
        #[cfg(target_os = "windows")]
        {
            // Determine if we should use native DXVA decoder
            // Native DXVA only supports HEVC and must be explicitly selected
            let use_native =
                backend == VideoDecoderBackend::NativeDxva && codec == VideoCodec::H265;

            if use_native {
                // Native D3D11 Video decoder (HEVC only) - EXPERIMENTAL
                // Only used when explicitly selected by user
                info!("Creating native DXVA decoder for HEVC (experimental)");

                let (native_decoder, native_stats_rx) =
                    super::native_video::NativeVideoDecoder::new_async(
                        codec,
                        shared_frame.clone(),
                    )?;

                info!("Native DXVA HEVC decoder created successfully");

                // Convert NativeDecodeStats to DecodeStats via a bridge channel
                let (stats_tx, stats_rx) = tokio_mpsc::channel::<DecodeStats>(64);

                // Spawn a task to convert stats
                tokio::spawn(async move {
                    let mut native_rx = native_stats_rx;
                    while let Some(native_stats) = native_rx.recv().await {
                        let stats = DecodeStats {
                            decode_time_ms: native_stats.decode_time_ms,
                            frame_produced: native_stats.frame_produced,
                            needs_keyframe: native_stats.needs_keyframe,
                        };
                        if stats_tx.send(stats).await.is_err() {
                            break;
                        }
                    }
                });

                return Ok((UnifiedVideoDecoder::Native(native_decoder), stats_rx));
            }

            // Default: Use GStreamer D3D11 decoder for all codecs
            // This is stable and supports H.264, H.265, and AV1
            let gst_codec = match codec {
                VideoCodec::H264 => {
                    info!("Creating GStreamer D3D11 decoder for H.264");
                    super::gstreamer_decoder::GstCodec::H264
                }
                VideoCodec::H265 => {
                    info!("Creating GStreamer D3D11 decoder for H.265");
                    super::gstreamer_decoder::GstCodec::H265
                }
                VideoCodec::AV1 => {
                    info!("Creating GStreamer D3D11 decoder for AV1");
                    super::gstreamer_decoder::GstCodec::AV1
                }
            };

            let gst_config = super::gstreamer_decoder::GstDecoderConfig {
                codec: gst_codec,
                width: 1920,
                height: 1080,
                low_latency: true,
            };

            let gst_decoder = super::gstreamer_decoder::GStreamerDecoder::new(gst_config)
                .map_err(|e| anyhow!("Failed to create GStreamer {:?} decoder: {}", codec, e))?;

            info!("GStreamer D3D11 {:?} decoder created successfully", codec);

            let (stats_tx, stats_rx) = tokio_mpsc::channel::<DecodeStats>(64);

            let wrapper = GStreamerDecoderWrapper {
                decoder: gst_decoder,
                shared_frame: shared_frame.clone(),
                stats_tx,
                frames_decoded: 0,
                consecutive_failures: 0,
            };

            return Ok((UnifiedVideoDecoder::GStreamer(wrapper), stats_rx));
        }

        // macOS/Linux: Use FFmpeg decoder
        #[cfg(not(target_os = "windows"))]
        {
            let (ffmpeg_decoder, stats_rx) =
                VideoDecoder::new_async(codec, _backend, shared_frame)?;
            Ok((UnifiedVideoDecoder::Ffmpeg(ffmpeg_decoder), stats_rx))
        }
    }

    /// Decode a frame asynchronously
    pub fn decode_async(&mut self, data: &[u8], receive_time: std::time::Instant) -> Result<()> {
        match self {
            #[cfg(not(target_os = "windows"))]
            UnifiedVideoDecoder::Ffmpeg(decoder) => decoder.decode_async(data, receive_time),
            #[cfg(target_os = "windows")]
            UnifiedVideoDecoder::Native(decoder) => {
                decoder.decode_async(data.to_vec(), receive_time);
                Ok(())
            }
            #[cfg(target_os = "windows")]
            UnifiedVideoDecoder::GStreamer(wrapper) => {
                wrapper.decode_async(data, receive_time);
                Ok(())
            }
        }
    }

    /// Check if using hardware acceleration
    pub fn is_hw_accelerated(&self) -> bool {
        match self {
            #[cfg(not(target_os = "windows"))]
            UnifiedVideoDecoder::Ffmpeg(decoder) => decoder.is_hw_accelerated(),
            #[cfg(target_os = "windows")]
            UnifiedVideoDecoder::Native(decoder) => decoder.is_hw_accel(),
            #[cfg(target_os = "windows")]
            UnifiedVideoDecoder::GStreamer(_) => true, // GStreamer uses D3D11 hardware acceleration
        }
    }

    /// Get number of frames decoded
    pub fn frames_decoded(&self) -> u64 {
        match self {
            #[cfg(not(target_os = "windows"))]
            UnifiedVideoDecoder::Ffmpeg(decoder) => decoder.frames_decoded(),
            #[cfg(target_os = "windows")]
            UnifiedVideoDecoder::Native(decoder) => decoder.frames_decoded(),
            #[cfg(target_os = "windows")]
            UnifiedVideoDecoder::GStreamer(wrapper) => wrapper.frames_decoded,
        }
    }
}

#[cfg(target_os = "windows")]
impl GStreamerDecoderWrapper {
    /// Threshold for requesting a keyframe after consecutive failures
    const KEYFRAME_REQUEST_THRESHOLD: u32 = 10;

    /// Decode a frame asynchronously and write to SharedFrame
    pub fn decode_async(&mut self, data: &[u8], receive_time: std::time::Instant) {
        let decode_start = std::time::Instant::now();

        match self.decoder.decode(data) {
            Ok(Some(frame)) => {
                self.frames_decoded += 1;
                self.consecutive_failures = 0;
                self.shared_frame.write(frame);

                // Measure decode time from when we started pushing data
                let decode_time_ms = decode_start.elapsed().as_secs_f32() * 1000.0;

                // Log first frame
                if self.frames_decoded == 1 {
                    info!(
                        "GStreamer: First frame decoded in {:.1}ms (pipeline latency: {:.1}ms)",
                        decode_time_ms,
                        receive_time.elapsed().as_secs_f32() * 1000.0
                    );
                }

                let _ = self.stats_tx.try_send(DecodeStats {
                    decode_time_ms,
                    frame_produced: true,
                    needs_keyframe: false,
                });
            }
            Ok(None) => {
                // No frame produced yet (buffering or B-frame reordering)
                self.consecutive_failures += 1;

                let needs_keyframe =
                    if self.consecutive_failures == Self::KEYFRAME_REQUEST_THRESHOLD {
                        warn!(
                            "GStreamer: {} consecutive packets without frame - requesting keyframe",
                            self.consecutive_failures
                        );
                        true
                    } else if self.consecutive_failures > Self::KEYFRAME_REQUEST_THRESHOLD
                        && self.consecutive_failures % 20 == 0
                    {
                        warn!(
                            "GStreamer: Still failing after {} packets - requesting keyframe again",
                            self.consecutive_failures
                        );
                        true
                    } else {
                        false
                    };

                let decode_time_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
                let _ = self.stats_tx.try_send(DecodeStats {
                    decode_time_ms,
                    frame_produced: false,
                    needs_keyframe,
                });
            }
            Err(e) => {
                warn!("GStreamer decode error: {}", e);
                self.consecutive_failures += 1;

                let decode_time_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
                let _ = self.stats_tx.try_send(DecodeStats {
                    decode_time_ms,
                    frame_produced: false,
                    needs_keyframe: self.consecutive_failures >= Self::KEYFRAME_REQUEST_THRESHOLD,
                });
            }
        }
    }
}
