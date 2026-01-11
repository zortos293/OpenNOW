//! Application Configuration
//!
//! Persistent settings for the OpenNow Streamer.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // === Video Settings ===
    /// Stream quality preset
    pub quality: StreamQuality,

    /// Custom resolution (e.g., "1920x1080")
    pub resolution: String,

    /// Target FPS (30, 60, 120, 240, 360)
    pub fps: u32,

    /// Preferred video codec
    pub codec: VideoCodec,

    /// Maximum bitrate in Mbps (200 = unlimited)
    pub max_bitrate_mbps: u32,

    /// Preferred video decoder backend
    pub decoder_backend: VideoDecoderBackend,

    /// Color quality setting (combines bit depth and chroma format)
    pub color_quality: ColorQuality,

    /// HDR mode enabled
    pub hdr_enabled: bool,

    // === Audio Settings ===
    /// Audio codec
    pub audio_codec: AudioCodec,

    /// Enable surround sound
    pub surround: bool,

    // === Performance ===
    /// Enable VSync
    pub vsync: bool,

    /// Low latency mode (reduces buffer)
    pub low_latency_mode: bool,

    /// NVIDIA Reflex (auto-enabled for 120+ FPS)
    pub nvidia_reflex: bool,

    // === Input ===
    /// Mouse sensitivity multiplier
    pub mouse_sensitivity: f32,

    /// Use raw input (Windows only)
    pub raw_input: bool,

    /// Enable clipboard paste (Ctrl+V sends clipboard text to remote session)
    /// Max 65536 bytes (64KB) per paste
    pub clipboard_paste_enabled: bool,

    // === Display ===
    /// Start in fullscreen
    pub fullscreen: bool,

    /// Borderless fullscreen
    pub borderless: bool,

    /// Window width (0 = use default)
    pub window_width: u32,

    /// Window height (0 = use default)
    pub window_height: u32,

    /// Show stats panel
    pub show_stats: bool,

    /// Stats panel position
    pub stats_position: StatsPosition,

    // === Network ===
    /// Preferred server region
    pub preferred_region: Option<String>,

    /// Selected server ID (zone ID)
    pub selected_server: Option<String>,

    /// Auto server selection (picks best ping)
    pub auto_server_selection: bool,

    /// Proxy URL
    pub proxy: Option<String>,

    /// Disable telemetry
    pub disable_telemetry: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Video
            quality: StreamQuality::Auto,
            resolution: "1920x1080".to_string(),
            fps: 60,
            codec: VideoCodec::H264,
            max_bitrate_mbps: 150,
            decoder_backend: VideoDecoderBackend::Auto, // Auto-select best decoder
            color_quality: ColorQuality::Bit10Yuv420,
            hdr_enabled: false,

            // Audio
            audio_codec: AudioCodec::Opus,
            surround: false,

            // Performance
            vsync: false,
            low_latency_mode: true,
            nvidia_reflex: true,

            // Input
            mouse_sensitivity: 1.0,
            raw_input: true,
            clipboard_paste_enabled: true, // Enable by default like official client

            // Display
            fullscreen: false,
            borderless: true,
            window_width: 0,  // 0 = use default
            window_height: 0, // 0 = use default
            show_stats: true,
            stats_position: StatsPosition::BottomLeft,

            // Network
            preferred_region: None,
            selected_server: None,
            auto_server_selection: true, // Default to auto
            proxy: None,
            disable_telemetry: true,
        }
    }
}

impl Settings {
    /// Get settings file path
    fn file_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("opennow-streamer").join("settings.json"))
    }

    /// Load settings from disk
    pub fn load() -> Result<Self> {
        let path = Self::file_path().ok_or_else(|| anyhow::anyhow!("No config directory"))?;

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&path)?;
        let settings: Settings = serde_json::from_str(&content)?;
        Ok(settings)
    }

    /// Save settings to disk
    pub fn save(&self) -> Result<()> {
        let path = Self::file_path().ok_or_else(|| anyhow::anyhow!("No config directory"))?;

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)?;

        Ok(())
    }

    /// Get resolution as (width, height)
    pub fn resolution_tuple(&self) -> (u32, u32) {
        let parts: Vec<&str> = self.resolution.split('x').collect();
        if parts.len() == 2 {
            let width = parts[0].parse().unwrap_or(1920);
            let height = parts[1].parse().unwrap_or(1080);
            (width, height)
        } else {
            (1920, 1080)
        }
    }

    /// Get max bitrate in kbps
    pub fn max_bitrate_kbps(&self) -> u32 {
        self.max_bitrate_mbps * 1000
    }
}

/// Stream quality presets
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamQuality {
    /// Auto-detect based on connection
    #[default]
    Auto,
    /// 720p 30fps
    Low,
    /// 1080p 60fps
    Medium,
    /// 1440p 60fps
    High,
    /// 4K 60fps
    Ultra,
    /// 1080p 120fps
    High120,
    /// 1440p 120fps
    Ultra120,
    /// 1080p 240fps (competitive)
    Competitive,
    /// 1080p 360fps (extreme)
    Extreme,
    /// Custom settings
    Custom,
}

impl StreamQuality {
    /// Get resolution and FPS for this quality preset
    pub fn settings(&self) -> (&str, u32) {
        match self {
            StreamQuality::Auto => ("1920x1080", 60),
            StreamQuality::Low => ("1280x720", 30),
            StreamQuality::Medium => ("1920x1080", 60),
            StreamQuality::High => ("2560x1440", 60),
            StreamQuality::Ultra => ("3840x2160", 60),
            StreamQuality::High120 => ("1920x1080", 120),
            StreamQuality::Ultra120 => ("2560x1440", 120),
            StreamQuality::Competitive => ("1920x1080", 240),
            StreamQuality::Extreme => ("1920x1080", 360),
            StreamQuality::Custom => ("1920x1080", 60),
        }
    }

    /// Get display name for UI
    pub fn display_name(&self) -> &'static str {
        match self {
            StreamQuality::Auto => "Auto",
            StreamQuality::Low => "720p 30fps",
            StreamQuality::Medium => "1080p 60fps",
            StreamQuality::High => "1440p 60fps",
            StreamQuality::Ultra => "4K 60fps",
            StreamQuality::High120 => "1080p 120fps",
            StreamQuality::Ultra120 => "1440p 120fps",
            StreamQuality::Competitive => "1080p 240fps",
            StreamQuality::Extreme => "1080p 360fps",
            StreamQuality::Custom => "Custom",
        }
    }

    /// Get all available presets
    pub fn all() -> &'static [StreamQuality] {
        &[
            StreamQuality::Auto,
            StreamQuality::Low,
            StreamQuality::Medium,
            StreamQuality::High,
            StreamQuality::Ultra,
            StreamQuality::High120,
            StreamQuality::Ultra120,
            StreamQuality::Competitive,
            StreamQuality::Extreme,
            StreamQuality::Custom,
        ]
    }
}

/// Available resolutions
pub const RESOLUTIONS: &[(&str, &str)] = &[
    ("1280x720", "720p"),
    ("1920x1080", "1080p"),
    ("2560x1440", "1440p"),
    ("3840x2160", "4K"),
    ("2560x1080", "Ultrawide 1080p"),
    ("3440x1440", "Ultrawide 1440p"),
    ("5120x1440", "Super Ultrawide"),
];

/// Available FPS options
pub const FPS_OPTIONS: &[u32] = &[30, 60, 90, 120, 144, 165, 240, 360];

/// Video codec options
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    /// H.264/AVC - widest compatibility
    #[default]
    H264,
    /// H.265/HEVC - better compression
    H265,
    /// AV1 - best compression, modern GPUs only
    AV1,
}

/// Video decoder backend preference
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoDecoderBackend {
    /// Auto-detect best decoder
    #[default]
    Auto,
    /// NVIDIA CUDA/CUVID
    Cuvid,
    /// Intel QuickSync
    Qsv,
    /// AMD VA-API
    Vaapi,
    /// DirectX 11/12 (Windows) via FFmpeg
    Dxva,
    /// Native D3D11 Video decoder (Windows) - bypasses FFmpeg
    /// This is the NVIDIA-style approach with proper RTArray support
    NativeDxva,
    /// VideoToolbox (macOS)
    VideoToolbox,
    /// Vulkan Video (Linux) - cross-GPU hardware decode via Vulkan extensions
    /// Based on GeForce NOW's VkVideoDecoder implementation
    VulkanVideo,
    /// Software decoding (CPU)
    Software,
}

impl VideoDecoderBackend {
    /// Short display name for dropdown
    pub fn as_str(&self) -> &'static str {
        match self {
            VideoDecoderBackend::Auto => "Auto",
            VideoDecoderBackend::Cuvid => "NVDEC",
            VideoDecoderBackend::Qsv => "QuickSync",
            VideoDecoderBackend::Vaapi => "VA-API",
            VideoDecoderBackend::Dxva => "D3D11 (GStreamer)",
            VideoDecoderBackend::NativeDxva => "Native D3D11 (HEVC only)",
            VideoDecoderBackend::VideoToolbox => "VideoToolbox",
            VideoDecoderBackend::VulkanVideo => "GStreamer HW",
            VideoDecoderBackend::Software => "Software",
        }
    }

    /// Detailed description for tooltip
    pub fn description(&self) -> &'static str {
        match self {
            VideoDecoderBackend::Auto => {
                "Automatically selects the best available decoder for your system.\n\n\
                 Windows: GStreamer D3D11 (d3d11h264dec/d3d11h265dec)\n\
                 Linux: GStreamer with VA-API or V4L2\n\
                 macOS: FFmpeg with VideoToolbox\n\
                 Performance: Optimal for your hardware"
            }
            VideoDecoderBackend::Cuvid => {
                "NVIDIA hardware decoding using NVDEC.\n\n\
                 Backend: GStreamer + nvd3d11h264dec/nvd3d11h265dec\n\
                 Performance: Excellent on NVIDIA GPUs\n\
                 Compatibility: NVIDIA GPUs only (GTX 600+)"
            }
            VideoDecoderBackend::Qsv => {
                "Intel hardware decoding using Quick Sync Video.\n\n\
                 Backend: GStreamer + qsvh264dec/qsvh265dec\n\
                 Performance: Good on Intel CPUs with integrated graphics\n\
                 Compatibility: Intel 2nd gen Core+ with HD Graphics"
            }
            VideoDecoderBackend::Vaapi => {
                "Linux hardware decoding via Video Acceleration API.\n\n\
                 Backend: GStreamer + vah264dec/vah265dec (or legacy vaapih264dec)\n\
                 Performance: Good on AMD/Intel GPUs (Linux)\n\
                 Compatibility: AMD, Intel GPUs on Linux\n\
                 Note: Use this if GStreamer HW doesn't work"
            }
            VideoDecoderBackend::Dxva => {
                "Windows DirectX Video Acceleration via GStreamer.\n\n\
                 Backend: GStreamer + d3d11h264dec/d3d11h265dec\n\
                 Performance: Good hardware acceleration, stable\n\
                 Compatibility: Windows with any modern GPU\n\
                 Note: Recommended for Windows (supports H.264 and H.265)"
            }
            VideoDecoderBackend::NativeDxva => {
                "Direct D3D11 Video decoder (EXPERIMENTAL).\n\n\
                 Backend: Native Windows D3D11 Video API\n\
                 Performance: Zero-copy to GPU, lowest latency\n\
                 Compatibility: Windows 8+ with D3D11 GPU\n\
                 WARNING: Only supports H.265/HEVC! H.264 will fail.\n\
                 Use 'D3D11 (GStreamer)' for H.264 streams."
            }
            VideoDecoderBackend::VideoToolbox => {
                "macOS hardware decoding using Apple's VideoToolbox.\n\n\
                 Backend: FFmpeg + VideoToolbox\n\
                 Performance: Excellent on Apple Silicon/Intel Macs\n\
                 Compatibility: macOS only"
            }
            VideoDecoderBackend::VulkanVideo => {
                "GStreamer hardware decoding (Linux).\n\n\
                 Backend: GStreamer auto-selects best decoder:\n\
                 - V4L2 (Raspberry Pi / embedded)\n\
                 - VA plugin (Intel/AMD desktop)\n\
                 - VAAPI plugin (legacy fallback)\n\
                 Performance: Hardware accelerated\n\
                 Compatibility: Linux with GStreamer installed"
            }
            VideoDecoderBackend::Software => {
                "CPU-based software decoding.\n\n\
                 Backend: GStreamer + avdec_h264/avdec_h265\n\
                 Performance: Slow, high CPU usage\n\
                 Compatibility: Any system (fallback)\n\
                 Note: Use only if hardware decode fails"
            }
        }
    }

    /// Get the underlying technology/backend name
    pub fn backend_name(&self) -> &'static str {
        match self {
            VideoDecoderBackend::Auto => "Auto",
            VideoDecoderBackend::Cuvid => "GStreamer NVDEC",
            VideoDecoderBackend::Qsv => "GStreamer QSV",
            VideoDecoderBackend::Vaapi => "GStreamer VA-API",
            VideoDecoderBackend::Dxva => "GStreamer D3D11",
            VideoDecoderBackend::NativeDxva => "Native D3D11",
            VideoDecoderBackend::VideoToolbox => "FFmpeg VT",
            VideoDecoderBackend::VulkanVideo => "GStreamer HW",
            VideoDecoderBackend::Software => "GStreamer CPU",
        }
    }

    pub fn all() -> &'static [VideoDecoderBackend] {
        &[
            VideoDecoderBackend::Auto,
            VideoDecoderBackend::Cuvid,
            VideoDecoderBackend::Qsv,
            VideoDecoderBackend::Vaapi,
            VideoDecoderBackend::Dxva,
            VideoDecoderBackend::NativeDxva,
            VideoDecoderBackend::VideoToolbox,
            VideoDecoderBackend::VulkanVideo,
            VideoDecoderBackend::Software,
        ]
    }
}

impl VideoCodec {
    pub fn as_str(&self) -> &'static str {
        match self {
            VideoCodec::H264 => "H264",
            VideoCodec::H265 => "H265",
            VideoCodec::AV1 => "AV1",
        }
    }

    /// Get display name with description
    pub fn display_name(&self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264 (Wide compatibility)",
            VideoCodec::H265 => "H.265/HEVC (Better quality)",
            VideoCodec::AV1 => "AV1 (Best compression, modern GPUs)",
        }
    }

    /// Get all available codecs
    pub fn all() -> &'static [VideoCodec] {
        &[VideoCodec::H264, VideoCodec::H265, VideoCodec::AV1]
    }
}

/// Audio codec options
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioCodec {
    /// Opus - low latency
    #[default]
    Opus,
    /// Opus Stereo
    OpusStereo,
}

/// Color quality options (bit depth + chroma subsampling)
/// Matches NVIDIA GFN client options
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColorQuality {
    /// 8-bit YUV 4:2:0 - Most compatible, lowest bandwidth
    Bit8Yuv420,
    /// 8-bit YUV 4:4:4 - Better color accuracy, higher bandwidth
    Bit8Yuv444,
    /// 10-bit YUV 4:2:0 - HDR capable, good balance (default)
    #[default]
    Bit10Yuv420,
    /// 10-bit YUV 4:4:4 - Best quality, highest bandwidth (requires HEVC)
    Bit10Yuv444,
}

impl ColorQuality {
    /// Get bit depth value (0 = 8-bit SDR, 10 = 10-bit HDR capable)
    pub fn bit_depth(&self) -> i32 {
        match self {
            ColorQuality::Bit8Yuv420 | ColorQuality::Bit8Yuv444 => 0, // 0 means 8-bit SDR
            ColorQuality::Bit10Yuv420 | ColorQuality::Bit10Yuv444 => 10,
        }
    }

    /// Get chroma format value (0 = 4:2:0, 2 = 4:4:4)
    /// Note: 4:2:2 is not commonly used in streaming
    pub fn chroma_format(&self) -> i32 {
        match self {
            ColorQuality::Bit8Yuv420 | ColorQuality::Bit10Yuv420 => 0, // YUV 4:2:0
            ColorQuality::Bit8Yuv444 | ColorQuality::Bit10Yuv444 => 2, // YUV 4:4:4
        }
    }

    /// Check if this mode requires HEVC codec
    pub fn requires_hevc(&self) -> bool {
        matches!(
            self,
            ColorQuality::Bit10Yuv420 | ColorQuality::Bit10Yuv444 | ColorQuality::Bit8Yuv444
        )
    }

    /// Check if this is a 10-bit mode
    pub fn is_10bit(&self) -> bool {
        matches!(self, ColorQuality::Bit10Yuv420 | ColorQuality::Bit10Yuv444)
    }

    /// Get display name for UI
    pub fn display_name(&self) -> &'static str {
        match self {
            ColorQuality::Bit8Yuv420 => "8-bit, YUV 4:2:0",
            ColorQuality::Bit8Yuv444 => "8-bit, YUV 4:4:4",
            ColorQuality::Bit10Yuv420 => "10-bit, YUV 4:2:0",
            ColorQuality::Bit10Yuv444 => "10-bit, YUV 4:4:4",
        }
    }

    /// Get description for UI
    pub fn description(&self) -> &'static str {
        match self {
            ColorQuality::Bit8Yuv420 => "Most compatible, lower bandwidth",
            ColorQuality::Bit8Yuv444 => "Better color, needs HEVC",
            ColorQuality::Bit10Yuv420 => "HDR ready, recommended",
            ColorQuality::Bit10Yuv444 => "Best quality, needs HEVC",
        }
    }

    /// Get all available options
    pub fn all() -> &'static [ColorQuality] {
        &[
            ColorQuality::Bit8Yuv420,
            ColorQuality::Bit8Yuv444,
            ColorQuality::Bit10Yuv420,
            ColorQuality::Bit10Yuv444,
        ]
    }
}

/// Stats panel position
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StatsPosition {
    TopLeft,
    TopRight,
    #[default]
    BottomLeft,
    BottomRight,
}
