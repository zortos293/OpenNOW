//! Media Pipeline
//!
//! Video decoding, audio decoding, and rendering.

use std::sync::atomic::{AtomicU64, Ordering};

mod audio;
mod rtp;
mod video;

/// Global frame ID counter for unique frame identification
/// Used to avoid redundant GPU texture uploads
static FRAME_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a new unique frame ID
pub fn next_frame_id() -> u64 {
    FRAME_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(target_os = "macos")]
pub mod videotoolbox;

#[cfg(target_os = "windows")]
pub mod d3d11;

#[cfg(target_os = "windows")]
pub mod dxva_decoder;

#[cfg(target_os = "windows")]
pub mod hevc_parser;

#[cfg(target_os = "windows")]
pub mod native_video;

#[cfg(target_os = "linux")]
pub mod vaapi;

#[cfg(target_os = "linux")]
pub mod v4l2;

// GStreamer decoder available on Linux and Windows
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod gstreamer_decoder;

pub use audio::*;
pub use rtp::{DepacketizerCodec, RtpDepacketizer};
pub use video::{get_supported_decoder_backends, DecodeStats, UnifiedVideoDecoder, VideoDecoder};

#[cfg(target_os = "macos")]
pub use videotoolbox::{
    CVMetalTexture, CVPixelBufferWrapper, LockedPlanes, MetalVideoRenderer, ZeroCopyFrame,
    ZeroCopyTextureManager,
};

#[cfg(target_os = "windows")]
pub use d3d11::{D3D11TextureWrapper, D3D11ZeroCopyManager, LockedPlanes as D3D11LockedPlanes};

#[cfg(target_os = "windows")]
pub use dxva_decoder::{DxvaCodec, DxvaDecoder, DxvaDecoderConfig};

#[cfg(target_os = "windows")]
pub use native_video::{NativeDecodeStats, NativeVideoDecoder};

#[cfg(target_os = "linux")]
pub use vaapi::{LockedPlanes as VaapiLockedPlanes, VAAPISurfaceWrapper, VaapiZeroCopyManager};

#[cfg(target_os = "linux")]
pub use v4l2::{
    get_pi_model, get_recommended_codec, is_raspberry_pi, is_v4l2_available,
    LockedPlanes as V4L2LockedPlanes, V4L2BufferWrapper, V4L2Codec, V4L2PixelFormat,
    V4L2ZeroCopyManager,
};

#[cfg(target_os = "linux")]
pub use gstreamer_decoder::{
    is_gstreamer_v4l2_available, GStreamerDecoder, GstCodec, GstDecoderConfig,
};

#[cfg(target_os = "windows")]
pub use gstreamer_decoder::{is_gstreamer_available, GStreamerDecoder, GstCodec, GstDecoderConfig};

/// Pixel format of decoded video frame
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum PixelFormat {
    /// YUV 4:2:0 planar (Y, U, V separate planes)
    #[default]
    YUV420P,
    /// NV12 semi-planar (Y plane + interleaved UV plane)
    /// More efficient on macOS VideoToolbox - skip CPU conversion
    NV12,
    /// P010 10-bit HDR (Y plane + interleaved UV, 10 bits per sample in 16-bit words)
    P010,
}

/// Decoded video frame
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Unique frame ID for tracking (monotonically increasing)
    /// Used to avoid redundant GPU uploads of the same frame
    pub frame_id: u64,
    pub width: u32,
    pub height: u32,
    /// Y plane (luma) - full resolution
    pub y_plane: Vec<u8>,
    /// U plane (Cb chroma) - for YUV420P: half resolution
    /// For NV12: this contains interleaved UV data
    pub u_plane: Vec<u8>,
    /// V plane (Cr chroma) - for YUV420P: half resolution
    /// For NV12: this is empty (UV is interleaved in u_plane)
    pub v_plane: Vec<u8>,
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
    pub timestamp_us: u64,
    /// Pixel format (YUV420P or NV12)
    pub format: PixelFormat,
    /// Color range (Limited or Full)
    pub color_range: ColorRange,
    /// Color space (matrix coefficients)
    pub color_space: ColorSpace,
    /// Transfer function (SDR gamma vs HDR PQ/HLG)
    pub transfer_function: TransferFunction,
    /// Zero-copy GPU buffer (macOS VideoToolbox only)
    /// When present, y_plane/u_plane are empty and rendering uses this directly
    #[cfg(target_os = "macos")]
    pub gpu_frame: Option<std::sync::Arc<CVPixelBufferWrapper>>,
    /// Zero-copy GPU texture (Windows D3D11VA only)
    /// When present, y_plane/u_plane are empty and rendering imports this directly
    #[cfg(target_os = "windows")]
    pub gpu_frame: Option<std::sync::Arc<D3D11TextureWrapper>>,
    /// Zero-copy GPU surface (Linux VAAPI only)
    /// When present, y_plane/u_plane are empty and rendering imports this directly
    #[cfg(target_os = "linux")]
    pub gpu_frame: Option<std::sync::Arc<VAAPISurfaceWrapper>>,
}

/// Video color range
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ColorRange {
    /// Limited range (16-235 for Y, 16-240 for UV) - Standard for TV/Video
    #[default]
    Limited,
    /// Full range (0-255) - Standard for PC/JPEG
    Full,
}

/// Video color space (matrix coefficients)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ColorSpace {
    /// BT.709 (HDTV) - Default
    #[default]
    BT709,
    /// BT.601 (SDTV)
    BT601,
    /// BT.2020 (UHDTV)
    BT2020,
}

/// Video transfer function (EOTF - Electro-Optical Transfer Function)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum TransferFunction {
    /// SDR gamma (~2.4) - BT.709/BT.601/sRGB
    #[default]
    SDR,
    /// HDR PQ (Perceptual Quantizer) - SMPTE ST 2084 / HDR10
    PQ,
    /// HDR HLG (Hybrid Log-Gamma) - ARIB STD-B67
    HLG,
}

impl VideoFrame {
    /// Create empty frame (YUV420P format)
    pub fn empty(width: u32, height: u32) -> Self {
        let y_size = (width * height) as usize;
        let uv_size = y_size / 4;

        Self {
            frame_id: next_frame_id(),
            width,
            height,
            y_plane: vec![0; y_size],
            u_plane: vec![128; uv_size],
            v_plane: vec![128; uv_size],
            y_stride: width,
            u_stride: width / 2,
            v_stride: width / 2,
            timestamp_us: 0,
            format: PixelFormat::YUV420P,
            color_range: ColorRange::Limited,
            color_space: ColorSpace::BT709,
            transfer_function: TransferFunction::SDR,
            #[cfg(target_os = "macos")]
            gpu_frame: None,
            #[cfg(target_os = "windows")]
            gpu_frame: None,
            #[cfg(target_os = "linux")]
            gpu_frame: None,
        }
    }

    /// Convert YUV to RGB (for CPU rendering fallback)
    pub fn to_rgb(&self) -> Vec<u8> {
        let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);

        for row in 0..self.height {
            for col in 0..self.width {
                let yi = (row * self.y_stride + col) as usize;
                let ui = ((row / 2) * self.u_stride + col / 2) as usize;
                let vi = ((row / 2) * self.v_stride + col / 2) as usize;

                let y = self.y_plane.get(yi).copied().unwrap_or(0) as f32;
                let u = self.u_plane.get(ui).copied().unwrap_or(128) as f32 - 128.0;
                let v = self.v_plane.get(vi).copied().unwrap_or(128) as f32 - 128.0;

                // BT.601 YUV to RGB
                let r = (y + 1.402 * v).clamp(0.0, 255.0) as u8;
                let g = (y - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8;
                let b = (y + 1.772 * u).clamp(0.0, 255.0) as u8;

                rgb.push(r);
                rgb.push(g);
                rgb.push(b);
            }
        }

        rgb
    }

    /// Convert YUV to RGBA - optimized with integer math
    pub fn to_rgba(&self) -> Vec<u8> {
        let pixel_count = (self.width * self.height) as usize;
        let mut rgba = vec![0u8; pixel_count * 4];

        // Pre-calculate constants for BT.601 YUV->RGB (scaled by 256 for integer math)
        // R = Y + 1.402*V  -> Y + (359*V)/256
        // G = Y - 0.344*U - 0.714*V -> Y - (88*U + 183*V)/256
        // B = Y + 1.772*U -> Y + (454*U)/256

        let width = self.width as usize;
        let height = self.height as usize;
        let y_stride = self.y_stride as usize;
        let u_stride = self.u_stride as usize;
        let _v_stride = self.v_stride as usize;

        for row in 0..height {
            let y_row_offset = row * y_stride;
            let uv_row_offset = (row / 2) * u_stride;
            let rgba_row_offset = row * width * 4;

            for col in 0..width {
                let yi = y_row_offset + col;
                let uvi = uv_row_offset + col / 2;
                let rgba_i = rgba_row_offset + col * 4;

                // Safe bounds check with defaults
                let y = *self.y_plane.get(yi).unwrap_or(&0) as i32;
                let u = *self.u_plane.get(uvi).unwrap_or(&128) as i32 - 128;
                let v = *self.v_plane.get(uvi).unwrap_or(&128) as i32 - 128;

                // Integer math conversion (faster than float)
                let r = (y + ((359 * v) >> 8)).clamp(0, 255) as u8;
                let g = (y - ((88 * u + 183 * v) >> 8)).clamp(0, 255) as u8;
                let b = (y + ((454 * u) >> 8)).clamp(0, 255) as u8;

                rgba[rgba_i] = r;
                rgba[rgba_i + 1] = g;
                rgba[rgba_i + 2] = b;
                rgba[rgba_i + 3] = 255;
            }
        }

        rgba
    }
}

/// Stream statistics
#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    /// Video resolution
    pub resolution: String,
    /// Current decoded FPS (frames decoded per second)
    pub fps: f32,
    /// Render FPS (frames actually rendered to screen per second)
    pub render_fps: f32,
    /// Target FPS
    pub target_fps: u32,
    /// Video bitrate in Mbps
    pub bitrate_mbps: f32,
    /// Network latency in ms
    pub latency_ms: f32,
    /// Frame decode time in ms
    pub decode_time_ms: f32,
    /// Frame render time in ms
    pub render_time_ms: f32,
    /// Input latency in ms (time from event creation to transmission)
    pub input_latency_ms: f32,
    /// Video codec name
    pub codec: String,
    /// GPU type
    pub gpu_type: String,
    /// Server region
    pub server_region: String,
    /// Packet loss percentage
    pub packet_loss: f32,
    /// Network jitter in ms
    pub jitter_ms: f32,
    /// Network RTT (round-trip time) in ms from ICE candidate pair
    pub rtt_ms: f32,
    /// Total frames received
    pub frames_received: u64,
    /// Total frames decoded
    pub frames_decoded: u64,
    /// Total frames dropped
    pub frames_dropped: u64,
    /// Total frames rendered
    pub frames_rendered: u64,
    /// Input events sent per second
    pub input_rate: f32,
    /// Frame delivery latency (RTP arrival to decode complete) in ms
    pub frame_delivery_ms: f32,
    /// Estimated end-to-end latency in ms (decode_time + estimated network)
    pub estimated_e2e_ms: f32,
    /// Audio buffer level in ms
    pub audio_buffer_ms: f32,
    /// HDR mode (true = HDR/PQ, false = SDR)
    pub is_hdr: bool,
    /// Color space (e.g., "BT.709", "BT.2020")
    pub color_space: String,
}

impl StreamStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Format resolution string
    pub fn format_resolution(&self) -> String {
        if self.resolution.is_empty() {
            "N/A".to_string()
        } else {
            self.resolution.clone()
        }
    }

    /// Format bitrate string
    pub fn format_bitrate(&self) -> String {
        if self.bitrate_mbps > 0.0 {
            format!("{:.1} Mbps", self.bitrate_mbps)
        } else {
            "N/A".to_string()
        }
    }
}
