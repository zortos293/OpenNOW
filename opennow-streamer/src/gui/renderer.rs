//! GPU Renderer
//!
//! wgpu-based rendering for video frames and UI overlays.

// Local profiling macro for Tracy integration
// When tracy feature is enabled, creates tracing spans that Tracy visualizes
macro_rules! profile_scope {
    ($name:expr) => {
        #[cfg(feature = "tracy")]
        let _span = tracing::info_span!($name).entered();
        #[cfg(not(feature = "tracy"))]
        let _ = $name; // Suppress unused warning
    };
}

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use std::sync::Arc;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowAttributes};

#[cfg(target_os = "macos")]
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
// use wgpu::util::DeviceExt;

use super::image_cache;
use super::screens::{
    render_ads_required_screen, render_alliance_warning_dialog, render_av1_warning_dialog,
    render_login_screen, render_session_conflict_dialog, render_session_screen,
    render_settings_modal, render_welcome_popup,
};
use super::shaders::{EXTERNAL_TEXTURE_SHADER, NV12_HDR_TONEMAP_SHADER, NV12_SHADER, VIDEO_SHADER};
use super::StatsPanel;
use crate::app::session::ActiveSessionInfo;
use crate::app::{App, AppState, GameInfo, GamesTab, UiAction};
#[cfg(target_os = "windows")]
use crate::media::D3D11TextureWrapper;
#[cfg(target_os = "linux")]
use crate::media::VAAPISurfaceWrapper;
#[cfg(target_os = "macos")]
use crate::media::{CVMetalTexture, MetalVideoRenderer, ZeroCopyTextureManager};
use crate::media::{ColorSpace, PixelFormat, StreamStats, TransferFunction, VideoFrame};
use std::collections::HashMap;
use std::time::{Duration, Instant};
#[cfg(target_os = "windows")]
// unused: use windows::core::Interface;
#[cfg(target_os = "windows")]
#[cfg(target_os = "macos")]
use wgpu_hal::dx12;
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HANDLE;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Resource};

// Color conversion is now hardcoded in the shader using official GFN client BT.709 values
// This eliminates potential initialization bugs with uniform buffers

/// Resolution change notification for animated popup
struct ResolutionNotification {
    old_resolution: String,
    new_resolution: String,
    direction: ResolutionDirection,
    start_time: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum ResolutionDirection {
    Up,
    Down,
    Same,
}

impl ResolutionNotification {
    const DURATION_SECS: f32 = 5.0;
    const FADE_IN_SECS: f32 = 0.3;
    const FADE_OUT_SECS: f32 = 0.7;

    fn new(old_res: &str, new_res: &str) -> Self {
        // Parse resolutions to determine direction
        let old_pixels = Self::parse_resolution(old_res);
        let new_pixels = Self::parse_resolution(new_res);

        let direction = if new_pixels > old_pixels {
            ResolutionDirection::Up
        } else if new_pixels < old_pixels {
            ResolutionDirection::Down
        } else {
            ResolutionDirection::Same
        };

        Self {
            old_resolution: old_res.to_string(),
            new_resolution: new_res.to_string(),
            direction,
            start_time: Instant::now(),
        }
    }

    fn parse_resolution(res: &str) -> u64 {
        // Parse "1920x1080" or "1920x1080 @ 60fps" format
        let parts: Vec<&str> = res.split(['x', ' ', '@']).collect();
        if parts.len() >= 2 {
            let w: u64 = parts[0].trim().parse().unwrap_or(0);
            let h: u64 = parts[1].trim().parse().unwrap_or(0);
            w * h
        } else {
            0
        }
    }

    fn is_expired(&self) -> bool {
        self.start_time.elapsed().as_secs_f32() > Self::DURATION_SECS
    }

    fn alpha(&self) -> f32 {
        let elapsed = self.start_time.elapsed().as_secs_f32();

        if elapsed < Self::FADE_IN_SECS {
            // Fade in
            elapsed / Self::FADE_IN_SECS
        } else if elapsed > Self::DURATION_SECS - Self::FADE_OUT_SECS {
            // Fade out
            let fade_progress = (Self::DURATION_SECS - elapsed) / Self::FADE_OUT_SECS;
            fade_progress.max(0.0)
        } else {
            // Full opacity
            1.0
        }
    }
}

/// Racing wheel connection notification for animated popup
/// Shows when a racing wheel is detected during a streaming session
struct WheelNotification {
    wheel_count: usize,
    start_time: Instant,
}

impl WheelNotification {
    const DURATION_SECS: f32 = 6.0;
    const FADE_IN_SECS: f32 = 0.3;
    const FADE_OUT_SECS: f32 = 0.8;

    fn new(wheel_count: usize) -> Self {
        Self {
            wheel_count,
            start_time: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.start_time.elapsed().as_secs_f32() > Self::DURATION_SECS
    }

    fn alpha(&self) -> f32 {
        let elapsed = self.start_time.elapsed().as_secs_f32();

        if elapsed < Self::FADE_IN_SECS {
            // Fade in
            elapsed / Self::FADE_IN_SECS
        } else if elapsed > Self::DURATION_SECS - Self::FADE_OUT_SECS {
            // Fade out
            let fade_progress = (Self::DURATION_SECS - elapsed) / Self::FADE_OUT_SECS;
            fade_progress.max(0.0)
        } else {
            // Full opacity
            1.0
        }
    }
}

/// Main renderer
pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,

    // egui integration
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,

    // Video rendering pipeline (GPU YUV->RGB conversion)
    video_pipeline: wgpu::RenderPipeline,
    video_bind_group_layout: wgpu::BindGroupLayout,
    video_sampler: wgpu::Sampler,
    // YUV420P planar textures (Y = full res, U/V = half res for 4:2:0)
    y_texture: Option<wgpu::Texture>,
    u_texture: Option<wgpu::Texture>,
    v_texture: Option<wgpu::Texture>,
    video_bind_group: Option<wgpu::BindGroup>,
    video_size: (u32, u32),

    // NV12 pipeline (for VideoToolbox on macOS - faster than CPU scaler)
    nv12_pipeline: wgpu::RenderPipeline,
    nv12_bind_group_layout: wgpu::BindGroupLayout,
    // NV12 HDR tone mapping pipeline (for HDR content on SDR displays)
    nv12_hdr_pipeline: wgpu::RenderPipeline,
    // NV12 textures: Y (R8) and UV interleaved (Rg8)
    uv_texture: Option<wgpu::Texture>,
    nv12_bind_group: Option<wgpu::BindGroup>,
    // Current pixel format
    current_format: PixelFormat,
    // Current transfer function (for HDR detection)
    current_transfer_function: TransferFunction,

    // Direct access to decoder's frame buffer - pull frames here, not from App
    shared_frame: Option<Arc<crate::app::SharedFrame>>,

    // External Texture pipeline (true zero-copy hardware YUV->RGB)
    external_texture_pipeline: Option<wgpu::RenderPipeline>,
    external_texture_bind_group_layout: Option<wgpu::BindGroupLayout>,
    external_texture_bind_group: Option<wgpu::BindGroup>,
    external_texture: Option<wgpu::ExternalTexture>,
    external_texture_supported: bool,

    // Stats panel
    stats_panel: StatsPanel,

    // Fullscreen state
    fullscreen: bool,

    // Swapchain error recovery state
    // Tracks consecutive Outdated errors to avoid panic-fixing with wrong resolution
    consecutive_surface_errors: u32,

    // Supported present modes (for fallback when Immediate isn't available)
    supported_present_modes: Vec<wgpu::PresentMode>,

    // Game art texture cache (URL -> TextureHandle)
    game_textures: HashMap<String, egui::TextureHandle>,

    // === UI Optimization: Stats throttling ===
    // Cached stats for throttled rendering (updates every 200ms instead of every frame)
    cached_stats: Option<StreamStats>,
    stats_last_update: Instant,

    // === UI Optimization: Game grid caching ===
    // Cached game grid to avoid re-laying out every frame
    games_cache_hash: u64,

    // Track last uploaded frame to avoid redundant GPU uploads
    last_uploaded_frame_id: u64,

    // Resolution change notification
    resolution_notification: Option<ResolutionNotification>,
    last_resolution: String,

    // Racing wheel connection notification
    wheel_notification: Option<WheelNotification>,
    last_wheel_count: usize,

    // macOS zero-copy video rendering (Metal-based, no CPU copy)
    #[cfg(target_os = "macos")]
    zero_copy_manager: Option<ZeroCopyTextureManager>,
    #[cfg(target_os = "macos")]
    zero_copy_enabled: bool,
    // Store current CVMetalTextures to keep them alive during rendering
    #[cfg(target_os = "macos")]
    current_y_cv_texture: Option<CVMetalTexture>,
    #[cfg(target_os = "macos")]
    current_uv_cv_texture: Option<CVMetalTexture>,
    #[cfg(target_os = "windows")]
    current_imported_handle: Option<HANDLE>,
    #[cfg(target_os = "windows")]
    current_imported_texture: Option<wgpu::Texture>,
}

impl Renderer {
    /// Create a new renderer
    pub async fn new(event_loop: &ActiveEventLoop) -> Result<Self> {
        // Load settings to get saved window size
        let settings = crate::app::Settings::load().unwrap_or_default();

        // Create window attributes
        // Use saved window size if available, otherwise use defaults
        // ARM64 Linux: Start with smaller window to reduce initial GPU memory usage
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        let default_size = PhysicalSize::new(800u32, 600u32);
        #[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
        let default_size = PhysicalSize::new(1280u32, 720u32);

        // Use saved size if valid (non-zero and reasonable), otherwise use default
        let initial_size = if settings.window_width >= 640 && settings.window_height >= 480 {
            PhysicalSize::new(settings.window_width, settings.window_height)
        } else {
            default_size
        };

        let window_attrs = WindowAttributes::default()
            .with_title("OpenNow")
            .with_inner_size(initial_size)
            .with_min_inner_size(PhysicalSize::new(640, 480))
            .with_resizable(true);

        // Create window and wrap in Arc for surface creation
        let window = Arc::new(
            event_loop
                .create_window(window_attrs)
                .context("Failed to create window")?,
        );

        let size = window.inner_size();

        info!("Window created: {}x{}", size.width, size.height);

        // On macOS, enable high-performance mode and disable App Nap
        #[cfg(target_os = "macos")]
        Self::enable_macos_high_performance();

        // On macOS, set display to 120Hz immediately (before fullscreen)
        // This ensures Direct mode uses high refresh rate
        #[cfg(target_os = "macos")]
        Self::set_macos_display_mode_120hz();

        // Create wgpu instance
        // Force DX12 on Windows for better exclusive fullscreen support and lower latency
        // Vulkan on Windows has issues with exclusive fullscreen transitions causing DWM composition
        #[cfg(target_os = "windows")]
        let backends = wgpu::Backends::DX12;
        // ARM Linux (Raspberry Pi, etc): Check WGPU_BACKEND env var, default to Vulkan
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        let backends = {
            match std::env::var("WGPU_BACKEND").ok().as_deref() {
                Some("gl") | Some("GL") | Some("gles") | Some("GLES") => {
                    info!("ARM64 Linux: Using GL backend (from WGPU_BACKEND env var)");
                    wgpu::Backends::GL
                }
                Some("vulkan") | Some("VULKAN") => {
                    info!("ARM64 Linux: Using Vulkan backend (from WGPU_BACKEND env var)");
                    wgpu::Backends::VULKAN
                }
                _ => {
                    info!("ARM64 Linux: Using Vulkan backend (default - set WGPU_BACKEND=gl to try OpenGL)");
                    wgpu::Backends::VULKAN
                }
            }
        };
        #[cfg(all(
            not(target_os = "windows"),
            not(all(target_os = "linux", target_arch = "aarch64"))
        ))]
        let backends = wgpu::Backends::all();

        info!("Using wgpu backend: {:?}", backends);

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });

        // Create surface from Arc<Window>
        let surface = instance.create_surface(window.clone()).map_err(|e| {
            error!("Surface creation failed: {:?}", e);
            #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
            {
                error!("ARM64 Linux troubleshooting:");
                error!(
                    "  - Ensure Vulkan drivers are installed: sudo apt install mesa-vulkan-drivers"
                );
                error!("  - Try: WAYLAND_DISPLAY= ./run.sh  (force X11)");
            }
            anyhow::anyhow!("Failed to create surface: {:?}", e)
        })?;

        // Get adapter
        #[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("Failed to find GPU adapter")?;

        // ARM64 Linux: Try hardware GPU first, fall back to llvmpipe if it fails
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        let adapter = {
            let force_sw = std::env::var("OPENNOW_FORCE_SOFTWARE_GPU").is_ok();

            // Print V3D troubleshooting info
            info!("ARM64 Linux: GPU memory tips:");
            info!("  - Check GPU memory: vcgencmd get_mem gpu");
            info!("  - Increase GPU memory: Add 'gpu_mem=512' to /boot/firmware/config.txt");
            info!("  - V3D env vars: MESA_VK_ABORT_ON_DEVICE_LOSS=0 V3D_DEBUG=perf");

            if force_sw {
                info!("ARM64 Linux: Forcing software renderer (OPENNOW_FORCE_SOFTWARE_GPU set)");
                instance
                    .request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::LowPower,
                        compatible_surface: Some(&surface),
                        force_fallback_adapter: true,
                    })
                    .await
                    .context("Failed to find software GPU adapter")?
            } else {
                // Try hardware GPU first
                info!("ARM64 Linux: Trying hardware GPU...");
                match instance
                    .request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::LowPower,
                        compatible_surface: Some(&surface),
                        force_fallback_adapter: false,
                    })
                    .await
                {
                    Ok(hw_adapter) => {
                        let info = hw_adapter.get_info();
                        info!("  Hardware GPU found: {}", info.name);
                        // Check if this is V3D and warn about potential OOM
                        if info.name.to_lowercase().contains("v3d") {
                            warn!("  V3D GPU detected - may OOM during device creation");
                            warn!("  If OOM occurs, try: OPENNOW_FORCE_SOFTWARE_GPU=1 ./run.sh");
                            warn!("  Or increase GPU memory to 512MB in config.txt");
                        }
                        hw_adapter
                    }
                    Err(e) => {
                        warn!("  Hardware GPU failed: {:?}, using software renderer", e);
                        instance
                            .request_adapter(&wgpu::RequestAdapterOptions {
                                power_preference: wgpu::PowerPreference::LowPower,
                                compatible_surface: Some(&surface),
                                force_fallback_adapter: true,
                            })
                            .await
                            .context("Failed to find any GPU adapter")?
                    }
                }
            }
        };

        let adapter_info = adapter.get_info();
        info!(
            "GPU: {} (Backend: {:?}, Driver: {})",
            adapter_info.name, adapter_info.backend, adapter_info.driver_info
        );

        // Print to console directly for visibility (bypasses log filter)
        crate::utils::console_print(&format!(
            "[GPU] {} using {:?} backend",
            adapter_info.name, adapter_info.backend
        ));

        // Create device and queue
        // Request EXTERNAL_TEXTURE feature for true zero-copy video rendering
        let mut required_features = wgpu::Features::empty();
        let adapter_features = adapter.features();

        // Check if EXTERNAL_TEXTURE is supported (hardware YUV->RGB conversion)
        let external_texture_supported =
            adapter_features.contains(wgpu::Features::EXTERNAL_TEXTURE);
        if external_texture_supported {
            required_features |= wgpu::Features::EXTERNAL_TEXTURE;
            info!("EXTERNAL_TEXTURE feature supported - enabling true zero-copy video");
        } else {
            info!("EXTERNAL_TEXTURE not supported - using NV12 shader path");
        }

        // Detect Raspberry Pi V3D hardware GPU for ultra-minimal settings
        let is_v3d_hardware = adapter_info.name.to_lowercase().contains("v3d")
            || adapter_info.name.to_lowercase().contains("videocore");
        // Detect if we're on ARM64 Linux (includes llvmpipe on Pi)
        let is_arm64_linux = cfg!(all(target_os = "linux", target_arch = "aarch64"));

        // V3D: Don't request any optional features to minimize memory
        if is_v3d_hardware {
            required_features = wgpu::Features::empty();
            info!("V3D hardware: Disabling all optional features to save memory");
        }

        // Check if we're in legacy macOS mode (for 2015 and older Intel Macs)
        #[cfg(all(target_os = "macos", feature = "legacy-macos"))]
        let is_legacy_macos = true;
        #[cfg(not(all(target_os = "macos", feature = "legacy-macos")))]
        let is_legacy_macos = false;

        // Use appropriate limits based on GPU type
        let limits = if is_v3d_hardware {
            // V3D hardware: Use conservative limits (Pi 4/5 with 512MB+ GPU memory)
            info!("V3D hardware GPU detected - using conservative limits for 1080p");
            let mut lim = wgpu::Limits::downlevel_webgl2_defaults();
            // Support 1080p video (1920x1080) and some headroom
            lim.max_texture_dimension_1d = 2048;
            lim.max_texture_dimension_2d = 2048; // Enough for 1080p
            lim.max_texture_dimension_3d = 256;
            lim.max_buffer_size = 32 * 1024 * 1024; // 32MB
            lim.max_uniform_buffer_binding_size = 64 * 1024;
            lim.max_storage_buffer_binding_size = 32 * 1024 * 1024;
            lim.max_vertex_buffers = 8;
            lim.max_bind_groups = 4;
            lim.max_bindings_per_bind_group = 16;
            lim.max_samplers_per_shader_stage = 4;
            lim.max_sampled_textures_per_shader_stage = 8;
            info!("  Max texture: 2048, Max buffer: 32MB, Bind groups: 4");
            lim
        } else if is_legacy_macos {
            // Legacy macOS (2015 and older Intel Macs with Metal 1.0/1.1)
            // Use conservative limits that work on Intel Iris Graphics
            info!("Legacy macOS mode: Using conservative limits for Intel Iris Graphics");
            let mut lim = wgpu::Limits::downlevel_defaults();
            // Intel Iris Graphics (2015) supports up to 4096x4096 textures
            lim.max_texture_dimension_1d = 4096;
            lim.max_texture_dimension_2d = 4096;
            lim.max_texture_dimension_3d = 256;
            // Conservative buffer sizes for older GPUs
            lim.max_buffer_size = 256 * 1024 * 1024; // 256MB
            lim.max_uniform_buffer_binding_size = 64 * 1024;
            lim.max_storage_buffer_binding_size = 128 * 1024 * 1024;
            // Reduce bind groups to be safe on older Metal
            lim.max_bind_groups = 4;
            lim.max_bindings_per_bind_group = 16;
            info!("  Max texture: 4096, Max buffer: 256MB, Bind groups: 4");
            lim
        } else if is_arm64_linux {
            // llvmpipe or other ARM64: Use downlevel defaults
            info!("ARM64 Linux: Using downlevel defaults");
            wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits())
        } else {
            // Desktop: Use full adapter limits
            wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits())
        };

        info!(
            "Requesting device limits: Max Texture Dimension 2D: {}",
            limits.max_texture_dimension_2d
        );

        // ARM64 Linux: Try device creation, fallback to software if V3D OOMs
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        let (device, queue, adapter, is_v3d_hardware, required_features, limits): (
            wgpu::Device,
            wgpu::Queue,
            wgpu::Adapter,
            bool,
            wgpu::Features,
            wgpu::Limits,
        ) = {
            let device_result = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("OpenNow Device"),
                    required_features,
                    required_limits: limits.clone(),
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                    trace: wgpu::Trace::Off,
                })
                .await;

            match device_result {
                Ok((device, queue)) => {
                    info!("Device created successfully with {}", adapter_info.name);
                    (
                        device,
                        queue,
                        adapter,
                        is_v3d_hardware,
                        required_features,
                        limits,
                    )
                }
                Err(e) => {
                    // V3D device creation failed (likely OOM), fallback to software renderer
                    warn!("Hardware GPU device creation failed: {:?}", e);
                    warn!("Falling back to software renderer (llvmpipe)...");

                    crate::utils::console_print(
                        "[GPU] Hardware GPU failed, using software renderer",
                    );

                    // Get software (llvmpipe) adapter
                    let sw_adapter = instance
                        .request_adapter(&wgpu::RequestAdapterOptions {
                            power_preference: wgpu::PowerPreference::LowPower,
                            compatible_surface: Some(&surface),
                            force_fallback_adapter: true,
                        })
                        .await
                        .context("Failed to find software GPU adapter after hardware GPU failed")?;

                    let sw_info = sw_adapter.get_info();
                    info!(
                        "Fallback GPU: {} (Backend: {:?})",
                        sw_info.name, sw_info.backend
                    );

                    // Use downlevel defaults for llvmpipe
                    let sw_limits =
                        wgpu::Limits::downlevel_defaults().using_resolution(sw_adapter.limits());
                    let sw_features = wgpu::Features::empty();

                    let (device, queue) = sw_adapter
                        .request_device(&wgpu::DeviceDescriptor {
                            label: Some("OpenNow Device (Software Fallback)"),
                            required_features: sw_features,
                            required_limits: sw_limits.clone(),
                            memory_hints: wgpu::MemoryHints::MemoryUsage,
                            experimental_features: wgpu::ExperimentalFeatures::disabled(),
                            trace: wgpu::Trace::Off,
                        })
                        .await
                        .context("Failed to create software GPU device")?;

                    info!("Software renderer device created successfully");
                    (device, queue, sw_adapter, false, sw_features, sw_limits)
                }
            }
        };

        // Non-ARM64: Standard device creation
        #[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
        let (device, queue): (wgpu::Device, wgpu::Queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("OpenNow Device"),
                required_features,
                required_limits: limits,
                // Use MemoryUsage hint to avoid aggressive memory allocation which causes OOM on RPi5
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .context("Failed to create device")?;

        // Update adapter_info after potential ARM64 fallback to software renderer
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        let adapter_info = adapter.get_info();

        // Configure surface
        // Use non-sRGB (linear) format for video - H.264/HEVC output is already gamma-corrected
        // Using sRGB format would apply double gamma correction, causing washed-out colors
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| !f.is_srgb()) // Prefer linear format for video
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        // Start with Fifo (VSync) for low CPU usage in menus
        // Switches to Immediate when streaming for lowest latency
        let present_mode = wgpu::PresentMode::Fifo;
        info!("Using Fifo present mode (vsync) - low CPU usage for UI");

        // Frame latency: 2 for smoother pacing
        let frame_latency = 2;

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode,
            alpha_mode: if surface_caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PostMultiplied)
            {
                wgpu::CompositeAlphaMode::PostMultiplied
            } else if surface_caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
            {
                wgpu::CompositeAlphaMode::PreMultiplied
            } else {
                surface_caps.alpha_modes[0]
            },
            view_formats: vec![],
            desired_maximum_frame_latency: frame_latency,
        };

        surface.configure(&device, &config);

        // Create egui context
        let egui_ctx = egui::Context::default();

        // Create egui-winit state (egui 0.33 API)
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::default(),
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );

        // Create egui-wgpu renderer (egui 0.33 API)
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            surface_format,
            egui_wgpu::RendererOptions::default(),
        );

        // Create video rendering pipeline
        let video_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Video Shader"),
            source: wgpu::ShaderSource::Wgsl(VIDEO_SHADER.into()),
        });

        // Bind group layout for YUV planar textures (GPU color conversion)
        let video_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Video YUV Bind Group Layout"),
                entries: &[
                    // Y texture (full resolution)
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // U texture (half resolution)
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // V texture (half resolution)
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // Sampler
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let video_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Video Pipeline Layout"),
                bind_group_layouts: &[&video_bind_group_layout],
                immediate_size: 0,
            });

        let video_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Video Pipeline"),
            layout: Some(&video_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &video_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &video_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let video_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Video Sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        // Create NV12 pipeline (for VideoToolbox on macOS - GPU deinterleaving)
        let nv12_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("NV12 Shader"),
            source: wgpu::ShaderSource::Wgsl(NV12_SHADER.into()),
        });

        let nv12_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("NV12 Bind Group Layout"),
                entries: &[
                    // Y texture (full resolution, R8)
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // UV texture (half resolution, Rg8 interleaved)
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // Sampler
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let nv12_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("NV12 Pipeline Layout"),
            bind_group_layouts: &[&nv12_bind_group_layout],
            immediate_size: 0,
        });

        let nv12_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("NV12 Pipeline"),
            layout: Some(&nv12_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &nv12_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &nv12_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Create NV12 HDR tone mapping pipeline (for HDR content on SDR displays)
        let nv12_hdr_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("NV12 HDR Tonemap Shader"),
            source: wgpu::ShaderSource::Wgsl(NV12_HDR_TONEMAP_SHADER.into()),
        });

        // HDR pipeline uses the same bind group layout as NV12
        let nv12_hdr_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("NV12 HDR Tonemap Pipeline"),
            layout: Some(&nv12_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &nv12_hdr_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &nv12_hdr_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        info!("NV12 HDR tone mapping pipeline created");

        // Create External Texture pipeline (true zero-copy hardware YUV->RGB)
        let (external_texture_pipeline, external_texture_bind_group_layout) =
            if external_texture_supported {
                let external_texture_shader =
                    device.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("External Texture Shader"),
                        source: wgpu::ShaderSource::Wgsl(EXTERNAL_TEXTURE_SHADER.into()),
                    });

                let external_texture_bind_group_layout =
                    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some("External Texture Bind Group Layout"),
                        entries: &[
                            // External texture (hardware YUV->RGB conversion)
                            wgpu::BindGroupLayoutEntry {
                                binding: 0,
                                visibility: wgpu::ShaderStages::FRAGMENT,
                                ty: wgpu::BindingType::ExternalTexture,
                                count: None,
                            },
                            // Sampler for external texture
                            wgpu::BindGroupLayoutEntry {
                                binding: 1,
                                visibility: wgpu::ShaderStages::FRAGMENT,
                                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                                count: None,
                            },
                        ],
                    });

                let external_texture_pipeline_layout =
                    device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("External Texture Pipeline Layout"),
                        bind_group_layouts: &[&external_texture_bind_group_layout],
                        immediate_size: 0,
                    });

                let external_texture_pipeline =
                    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some("External Texture Pipeline"),
                        layout: Some(&external_texture_pipeline_layout),
                        vertex: wgpu::VertexState {
                            module: &external_texture_shader,
                            entry_point: Some("vs_main"),
                            buffers: &[],
                            compilation_options: Default::default(),
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &external_texture_shader,
                            entry_point: Some("fs_main"),
                            targets: &[Some(wgpu::ColorTargetState {
                                format: surface_format,
                                blend: Some(wgpu::BlendState::REPLACE),
                                write_mask: wgpu::ColorWrites::ALL,
                            })],
                            compilation_options: Default::default(),
                        }),
                        primitive: wgpu::PrimitiveState {
                            topology: wgpu::PrimitiveTopology::TriangleList,
                            strip_index_format: None,
                            front_face: wgpu::FrontFace::Ccw,
                            cull_mode: None,
                            polygon_mode: wgpu::PolygonMode::Fill,
                            unclipped_depth: false,
                            conservative: false,
                        },
                        depth_stencil: None,
                        multisample: wgpu::MultisampleState::default(),
                        multiview_mask: None,
                        cache: None,
                    });

                info!("External Texture pipeline created - true zero-copy video rendering enabled");
                (
                    Some(external_texture_pipeline),
                    Some(external_texture_bind_group_layout),
                )
            } else {
                (None, None)
            };

        // Create stats panel
        let stats_panel = StatsPanel::new();

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            size,
            egui_ctx,
            egui_state,
            egui_renderer,
            video_pipeline,
            video_bind_group_layout,
            video_sampler,
            y_texture: None,
            u_texture: None,
            v_texture: None,
            video_bind_group: None,
            video_size: (0, 0),
            nv12_pipeline,
            nv12_bind_group_layout,
            nv12_hdr_pipeline,
            uv_texture: None,
            nv12_bind_group: None,
            current_format: PixelFormat::YUV420P,
            current_transfer_function: TransferFunction::SDR,
            shared_frame: None,
            external_texture_pipeline,
            external_texture_bind_group_layout,
            external_texture_bind_group: None,
            external_texture: None,
            external_texture_supported,
            stats_panel,
            fullscreen: false,
            consecutive_surface_errors: 0,
            supported_present_modes: surface_caps.present_modes.clone(),
            game_textures: HashMap::new(),
            // UI optimization: stats throttling (200ms intervals)
            cached_stats: None,
            stats_last_update: Instant::now(),
            // UI optimization: game grid caching
            games_cache_hash: 0,
            // Track last uploaded frame to avoid redundant GPU uploads
            last_uploaded_frame_id: 0,
            // Resolution change notification
            resolution_notification: None,
            last_resolution: String::new(),
            // Racing wheel connection notification
            wheel_notification: None,
            last_wheel_count: 0,
            #[cfg(target_os = "macos")]
            zero_copy_manager: ZeroCopyTextureManager::new(),
            #[cfg(target_os = "macos")]
            zero_copy_enabled: true, // GPU blit via Metal for zero-copy CVPixelBuffer rendering
            #[cfg(target_os = "macos")]
            current_y_cv_texture: None,
            #[cfg(target_os = "macos")]
            current_uv_cv_texture: None,
            #[cfg(target_os = "windows")]
            current_imported_handle: None,
            #[cfg(target_os = "windows")]
            current_imported_texture: None,
        })
    }

    /// Get window reference
    pub fn window(&self) -> &Window {
        &self.window
    }

    /// Handle window event - returns (consumed, repaint)
    pub fn handle_event(&mut self, event: &WindowEvent) -> egui_winit::EventResponse {
        self.egui_state.on_window_event(&self.window, event)
    }

    /// Resize the renderer
    /// Filters out spurious resize events that occur during fullscreen transitions
    pub fn resize(&mut self, new_size: PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }

        // If we're in fullscreen mode, STRICTLY enforce that the resize matches the monitor
        // This prevents the race condition where the old windowed size (e.g., 1296x759)
        // is briefly reported during the fullscreen transition, causing DWM composition.
        if self.fullscreen {
            if let Some(monitor) = self.window.current_monitor() {
                let monitor_size = monitor.size();

                // Calculate deviation from monitor size (must be within 5%)
                let width_ratio = new_size.width as f32 / monitor_size.width as f32;
                let height_ratio = new_size.height as f32 / monitor_size.height as f32;

                // Reject if not within 95-105% of monitor resolution
                if width_ratio < 0.95
                    || width_ratio > 1.05
                    || height_ratio < 0.95
                    || height_ratio > 1.05
                {
                    debug!(
                        "Ignoring resize to {}x{} while in fullscreen (monitor: {}x{}, ratio: {:.2}x{:.2})",
                        new_size.width, new_size.height,
                        monitor_size.width, monitor_size.height,
                        width_ratio, height_ratio
                    );
                    return;
                }
            }
        }

        self.size = new_size;
        self.configure_surface();
    }

    /// Configure the surface with current size and optimal present mode
    /// Called on resize and to recover from swapchain errors
    fn configure_surface(&mut self) {
        self.config.width = self.size.width;
        self.config.height = self.size.height;
        self.surface.configure(&self.device, &self.config);
        info!(
            "Surface configured: {}x{} @ {:?} (frame latency: {})",
            self.config.width,
            self.config.height,
            self.config.present_mode,
            self.config.desired_maximum_frame_latency
        );

        // On macOS, set ProMotion frame rate and disable VSync on every configure
        // This ensures the Metal layer always requests 120fps from ProMotion
        #[cfg(target_os = "macos")]
        Self::disable_macos_vsync(&self.window);
    }

    /// Set VSync mode - use Fifo (vsync) for UI, Immediate/Mailbox for streaming
    /// This lets the GPU handle frame pacing, reducing CPU usage to near zero when idle
    pub fn set_vsync(&mut self, enabled: bool) {
        let new_mode = if enabled {
            wgpu::PresentMode::Fifo // VSync on - GPU waits for display refresh
        } else {
            // VSync off - prefer Immediate for lowest latency, fall back to Mailbox
            if self
                .supported_present_modes
                .contains(&wgpu::PresentMode::Immediate)
            {
                wgpu::PresentMode::Immediate
            } else if self
                .supported_present_modes
                .contains(&wgpu::PresentMode::Mailbox)
            {
                wgpu::PresentMode::Mailbox // Good low-latency alternative
            } else {
                wgpu::PresentMode::Fifo // Fallback to VSync if nothing else available
            }
        };

        if self.config.present_mode != new_mode {
            self.config.present_mode = new_mode;
            self.surface.configure(&self.device, &self.config);
            info!("Present mode changed to {:?}", new_mode);
        }
    }

    /// Recover from swapchain errors (Outdated/Lost)
    /// Returns true if recovery was successful
    fn recover_swapchain(&mut self) -> bool {
        // Get current window size - it may have changed (e.g., fullscreen toggle)
        let current_size = self.window.inner_size();
        if current_size.width == 0 || current_size.height == 0 {
            warn!("Cannot recover swapchain: window size is zero");
            return false;
        }

        // Update size and reconfigure
        self.size = current_size;
        self.configure_surface();
        info!(
            "Swapchain recovered: {}x{} @ {:?}",
            self.size.width, self.size.height, self.config.present_mode
        );
        true
    }

    /// Toggle fullscreen with high refresh rate support
    /// Uses exclusive fullscreen to bypass the desktop compositor (DWM) for lowest latency
    /// and selects the highest available refresh rate for the current resolution
    pub fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;

        if self.fullscreen {
            // On macOS, use Core Graphics to force 120Hz display mode
            #[cfg(target_os = "macos")]
            Self::set_macos_display_mode_120hz();

            // Use borderless fullscreen on macOS (exclusive doesn't work well)
            // The display mode is set separately via Core Graphics
            #[cfg(target_os = "macos")]
            {
                info!("Entering borderless fullscreen with 120Hz display mode");
                self.window
                    .set_fullscreen(Some(Fullscreen::Borderless(None)));
                Self::disable_macos_vsync(&self.window);
                return;
            }

            // On other platforms, try exclusive fullscreen
            #[cfg(not(target_os = "macos"))]
            {
                // Wayland doesn't support exclusive fullscreen - use borderless instead
                #[cfg(target_os = "linux")]
                let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
                #[cfg(not(target_os = "linux"))]
                let is_wayland = false;

                if is_wayland {
                    info!(
                        "Wayland detected - using borderless fullscreen (exclusive not supported)"
                    );
                    self.window
                        .set_fullscreen(Some(Fullscreen::Borderless(None)));
                    return;
                }

                let current_monitor = self.window.current_monitor();

                if let Some(monitor) = current_monitor {
                    let current_size = self.window.inner_size();
                    let mut best_mode: Option<winit::monitor::VideoModeHandle> = None;
                    let mut best_refresh_rate: u32 = 0;

                    info!("Searching for video modes on monitor: {:?}", monitor.name());
                    info!(
                        "Current window size: {}x{}",
                        current_size.width, current_size.height
                    );

                    let mut mode_count = 0;
                    let mut high_refresh_modes = Vec::new();
                    for mode in monitor.video_modes() {
                        let mode_size = mode.size();
                        let refresh_rate = mode.refresh_rate_millihertz() / 1000;

                        if refresh_rate >= 100 {
                            high_refresh_modes.push(format!(
                                "{}x{}@{}Hz",
                                mode_size.width, mode_size.height, refresh_rate
                            ));
                        }
                        mode_count += 1;

                        if mode_size.width >= current_size.width
                            && mode_size.height >= current_size.height
                        {
                            if refresh_rate > best_refresh_rate {
                                best_refresh_rate = refresh_rate;
                                best_mode = Some(mode);
                            }
                        }
                    }
                    info!(
                        "Total video modes: {} (high refresh >=100Hz: {:?})",
                        mode_count, high_refresh_modes
                    );

                    if let Some(mode) = best_mode {
                        let refresh_hz = mode.refresh_rate_millihertz() / 1000;
                        info!(
                            "SELECTED exclusive fullscreen: {}x{} @ {}Hz",
                            mode.size().width,
                            mode.size().height,
                            refresh_hz
                        );
                        self.window
                            .set_fullscreen(Some(Fullscreen::Exclusive(mode)));
                        return;
                    } else {
                        info!("No suitable exclusive fullscreen mode found");
                    }
                } else {
                    info!("No current monitor detected");
                }

                info!("Entering borderless fullscreen");
                self.window
                    .set_fullscreen(Some(Fullscreen::Borderless(None)));
            }
        } else {
            info!("Exiting fullscreen");
            self.window.set_fullscreen(None);
        }
    }

    /// Enter fullscreen with a specific target refresh rate
    /// Useful when the stream FPS is known (e.g., 120fps stream -> 120Hz mode)
    pub fn set_fullscreen_with_refresh(&mut self, target_fps: u32) {
        // Wayland doesn't support exclusive fullscreen - use borderless instead
        #[cfg(target_os = "linux")]
        let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
        #[cfg(not(target_os = "linux"))]
        let is_wayland = false;

        if is_wayland {
            info!(
                "Wayland detected - using borderless fullscreen for {}fps stream",
                target_fps
            );
            self.fullscreen = true;
            self.window
                .set_fullscreen(Some(Fullscreen::Borderless(None)));
            return;
        }

        let current_monitor = self.window.current_monitor();

        if let Some(monitor) = current_monitor {
            let current_size = self.window.inner_size();
            let mut best_mode: Option<winit::monitor::VideoModeHandle> = None;
            let mut best_refresh_diff: i32 = i32::MAX;

            // Find mode closest to target FPS
            for mode in monitor.video_modes() {
                let mode_size = mode.size();
                let refresh_rate = mode.refresh_rate_millihertz() / 1000;

                if mode_size.width >= current_size.width && mode_size.height >= current_size.height
                {
                    let diff = (refresh_rate as i32 - target_fps as i32).abs();
                    // Prefer modes >= target FPS
                    let adjusted_diff = if refresh_rate >= target_fps {
                        diff
                    } else {
                        diff + 1000
                    };

                    if adjusted_diff < best_refresh_diff {
                        best_refresh_diff = adjusted_diff;
                        best_mode = Some(mode);
                    }
                }
            }

            if let Some(mode) = best_mode {
                let refresh_hz = mode.refresh_rate_millihertz() / 1000;
                info!(
                    "Entering exclusive fullscreen for {}fps stream: {}x{} @ {}Hz",
                    target_fps,
                    mode.size().width,
                    mode.size().height,
                    refresh_hz
                );
                self.fullscreen = true;
                self.window
                    .set_fullscreen(Some(Fullscreen::Exclusive(mode)));

                #[cfg(target_os = "macos")]
                Self::disable_macos_vsync(&self.window);

                return;
            }
        }

        // Fallback
        self.fullscreen = true;
        self.window
            .set_fullscreen(Some(Fullscreen::Borderless(None)));

        #[cfg(target_os = "macos")]
        Self::disable_macos_vsync(&self.window);
    }

    /// Disable VSync on macOS Metal layer for unlimited FPS
    /// This prevents the compositor from limiting frame rate
    #[cfg(target_os = "macos")]
    fn disable_macos_vsync(window: &Window) {
        use cocoa::base::id;
        use objc::{msg_send, sel, sel_impl};

        // Get NSView from raw window handle
        let ns_view = match window.window_handle() {
            Ok(handle) => match handle.as_raw() {
                RawWindowHandle::AppKit(appkit) => appkit.ns_view.as_ptr() as id,
                _ => {
                    warn!("macOS: Unexpected window handle type");
                    return;
                }
            },
            Err(e) => {
                warn!("macOS: Could not get window handle: {:?}", e);
                return;
            }
        };

        unsafe {
            // Get the layer from NSView
            let layer: id = msg_send![ns_view, layer];
            if layer.is_null() {
                warn!("macOS: Could not get layer for VSync disable");
                return;
            }

            // Check if it's a CAMetalLayer by checking class name
            let class: id = msg_send![layer, class];
            let class_name: id = msg_send![class, description];
            let name_cstr: *const i8 = msg_send![class_name, UTF8String];

            if !name_cstr.is_null() {
                let name = std::ffi::CStr::from_ptr(name_cstr).to_string_lossy();
                if name.contains("CAMetalLayer") {
                    // Set preferredFrameRateRange for ProMotion displays FIRST
                    // This tells macOS we want 120fps, preventing dynamic drop to 60Hz
                    #[repr(C)]
                    struct CAFrameRateRange {
                        minimum: f32,
                        maximum: f32,
                        preferred: f32,
                    }

                    let frame_rate_range = CAFrameRateRange {
                        minimum: 60.0, // Allow 60fps minimum for flexibility
                        maximum: 120.0,
                        preferred: 120.0,
                    };

                    // Check if the layer responds to setPreferredFrameRateRange: (macOS 12+)
                    let responds: bool =
                        msg_send![layer, respondsToSelector: sel!(setPreferredFrameRateRange:)];
                    if responds {
                        let _: () = msg_send![layer, setPreferredFrameRateRange: frame_rate_range];
                        info!("macOS: Set preferredFrameRateRange to 60-120fps (ProMotion)");
                    }

                    // Enable displaySync for smooth presentation (no tearing)
                    // Latency is handled by decoder flags, not here
                    let _: () = msg_send![layer, setDisplaySyncEnabled: true];
                    info!("macOS: Enabled displaySync on CAMetalLayer for tear-free rendering");
                }
            }
        }
    }

    /// Set macOS display to 120Hz using Core Graphics
    /// This bypasses winit's video mode selection which doesn't work well on macOS
    #[cfg(target_os = "macos")]
    fn set_macos_display_mode_120hz() {
        use std::ffi::c_void;

        // Core Graphics FFI
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGMainDisplayID() -> u32;
            fn CGDisplayCopyAllDisplayModes(display: u32, options: *const c_void) -> *const c_void;
            fn CFArrayGetCount(array: *const c_void) -> isize;
            fn CFArrayGetValueAtIndex(array: *const c_void, idx: isize) -> *const c_void;
            fn CGDisplayModeGetWidth(mode: *const c_void) -> usize;
            fn CGDisplayModeGetHeight(mode: *const c_void) -> usize;
            fn CGDisplayModeGetRefreshRate(mode: *const c_void) -> f64;
            fn CGDisplaySetDisplayMode(
                display: u32,
                mode: *const c_void,
                options: *const c_void,
            ) -> i32;
            fn CGDisplayPixelsWide(display: u32) -> usize;
            fn CGDisplayPixelsHigh(display: u32) -> usize;
            fn CFRelease(cf: *const c_void);
        }

        unsafe {
            let display_id = CGMainDisplayID();
            let current_width = CGDisplayPixelsWide(display_id);
            let current_height = CGDisplayPixelsHigh(display_id);

            info!(
                "macOS: Searching for 120Hz mode on display {} (current: {}x{})",
                display_id, current_width, current_height
            );

            let modes = CGDisplayCopyAllDisplayModes(display_id, std::ptr::null());
            if modes.is_null() {
                warn!("macOS: Could not enumerate display modes");
                return;
            }

            let count = CFArrayGetCount(modes);
            let mut best_mode: *const c_void = std::ptr::null();
            let mut best_refresh: f64 = 0.0;

            for i in 0..count {
                let mode = CFArrayGetValueAtIndex(modes, i);
                let width = CGDisplayModeGetWidth(mode);
                let height = CGDisplayModeGetHeight(mode);
                let refresh = CGDisplayModeGetRefreshRate(mode);

                // Look for modes matching current resolution with high refresh rate
                if width == current_width && height == current_height {
                    if refresh > best_refresh {
                        best_refresh = refresh;
                        best_mode = mode;
                    }
                    if refresh >= 100.0 {
                        info!("  Found mode: {}x{} @ {:.1}Hz", width, height, refresh);
                    }
                }
            }

            if !best_mode.is_null() && best_refresh >= 119.0 {
                let width = CGDisplayModeGetWidth(best_mode);
                let height = CGDisplayModeGetHeight(best_mode);
                info!(
                    "macOS: Setting display mode to {}x{} @ {:.1}Hz",
                    width, height, best_refresh
                );

                let result = CGDisplaySetDisplayMode(display_id, best_mode, std::ptr::null());
                if result == 0 {
                    info!("macOS: Successfully set 120Hz display mode!");
                } else {
                    warn!("macOS: Failed to set display mode, error: {}", result);
                }
            } else if best_refresh > 0.0 {
                info!(
                    "macOS: No 120Hz mode found, best is {:.1}Hz - display may not support it",
                    best_refresh
                );
            } else {
                warn!("macOS: No matching display modes found");
            }

            CFRelease(modes);
        }
    }

    /// Enable high-performance mode on macOS
    /// This disables App Nap and other power throttling that can limit FPS
    #[cfg(target_os = "macos")]
    fn enable_macos_high_performance() {
        use cocoa::base::{id, nil};
        use objc::{class, msg_send, sel, sel_impl};

        unsafe {
            // Get NSProcessInfo
            let process_info: id = msg_send![class!(NSProcessInfo), processInfo];
            if process_info == nil {
                warn!("macOS: Could not get NSProcessInfo");
                return;
            }

            // Activity options for high performance:
            // NSActivityUserInitiated = 0x00FFFFFF (prevents App Nap, system sleep)
            // NSActivityLatencyCritical = 0xFF00000000 (requests low latency scheduling)
            let options: u64 = 0x00FFFFFF | 0xFF00000000;

            // Create reason string
            let reason: id = msg_send![class!(NSString), stringWithUTF8String: b"Streaming requires consistent frame timing\0".as_ptr()];

            // Begin activity - this returns an object we should retain
            let activity: id =
                msg_send![process_info, beginActivityWithOptions:options reason:reason];
            if activity != nil {
                // Retain the activity object to keep it alive for the app lifetime
                let _: id = msg_send![activity, retain];
                info!("macOS: High-performance mode enabled (App Nap disabled, latency-critical scheduling)");
            } else {
                warn!("macOS: Failed to enable high-performance mode");
            }

            // Also try to disable automatic termination
            let _: () = msg_send![process_info, disableAutomaticTermination: reason];

            // Disable sudden termination
            let _: () = msg_send![process_info, disableSuddenTermination];
        }
    }

    /// Lock cursor for streaming (captures mouse)
    pub fn lock_cursor(&self) {
        // Try confined first, then locked mode
        if let Err(e) = self.window.set_cursor_grab(CursorGrabMode::Confined) {
            info!("Confined cursor grab failed ({}), trying locked mode", e);
            if let Err(e) = self.window.set_cursor_grab(CursorGrabMode::Locked) {
                log::warn!("Failed to lock cursor: {}", e);
            }
        }
        self.window.set_cursor_visible(false);
        info!("Cursor locked for streaming");
    }

    /// Unlock cursor
    pub fn unlock_cursor(&self) {
        let _ = self.window.set_cursor_grab(CursorGrabMode::None);
        self.window.set_cursor_visible(true);
        info!("Cursor unlocked");
    }

    /// Check if fullscreen
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// Set the shared frame buffer for direct frame access
    /// This allows the renderer to pull frames directly from the decoder
    pub fn set_shared_frame(&mut self, shared_frame: Arc<crate::app::SharedFrame>) {
        self.shared_frame = Some(shared_frame);
    }

    /// Show racing wheel connection notification
    /// Called when racing wheels are detected during streaming session
    pub fn show_wheel_notification(&mut self, wheel_count: usize) {
        if wheel_count > 0 && wheel_count != self.last_wheel_count {
            info!(
                "Racing wheel notification: {} wheel(s) detected",
                wheel_count
            );
            self.wheel_notification = Some(WheelNotification::new(wheel_count));
            self.last_wheel_count = wheel_count;
        }
    }

    /// Reset wheel notification state (call when streaming stops)
    pub fn reset_wheel_notification(&mut self) {
        self.wheel_notification = None;
        self.last_wheel_count = 0;
    }

    /// Update video textures from frame (GPU YUV->RGB conversion)
    /// Supports both YUV420P (3 planes) and NV12 (2 planes) formats
    /// On macOS, uses zero-copy path via CVPixelBuffer + Metal blit
    /// On Windows, uses D3D11 shared textures
    pub fn update_video(&mut self, frame: &VideoFrame) {
        let uv_width = frame.width / 2;
        let uv_height = frame.height / 2;

        // ZERO-COPY PATH: CVPixelBuffer + Metal blit (macOS VideoToolbox)
        #[cfg(target_os = "macos")]
        if let Some(ref gpu_frame) = frame.gpu_frame {
            self.update_video_zero_copy(frame, gpu_frame, uv_width, uv_height);
            return;
        }

        // ZERO-COPY PATH: D3D11 texture sharing (Windows D3D11VA)
        // TODO: Implement true GPU sharing via D3D11/DX12 interop with wgpu
        // For now this still uses CPU staging - needs wgpu external memory support
        #[cfg(target_os = "windows")]
        if let Some(ref gpu_frame) = frame.gpu_frame {
            self.update_video_d3d11(frame, gpu_frame, uv_width, uv_height);
            self.last_uploaded_frame_id = frame.frame_id;
            return;
        }

        // ZERO-COPY PATH: VAAPI DMA-BUF import (Linux)
        // Imports the DMA-BUF from VAAPI decoder into Vulkan via VK_EXT_external_memory_dma_buf
        #[cfg(target_os = "linux")]
        if let Some(ref gpu_frame) = frame.gpu_frame {
            self.update_video_vaapi(frame, gpu_frame, uv_width, uv_height);
            self.last_uploaded_frame_id = frame.frame_id;
            return;
        }

        // EXTERNAL TEXTURE PATH: Disabled for now - using NV12 shader path instead
        // The external texture API on Windows DX12 may have issues with our frame lifecycle
        // TODO: Re-enable once external texture path is debugged
        // if self.external_texture_supported && frame.format == PixelFormat::NV12 && !frame.y_plane.is_empty() {
        //     self.update_video_external_texture(frame, uv_width, uv_height);
        //     return;
        // }

        // Check if we need to recreate textures (size or format change)
        let format_changed = self.current_format != frame.format;
        let size_changed = self.video_size != (frame.width, frame.height);

        // Update transfer function (HDR detection) - can change during stream
        if self.current_transfer_function != frame.transfer_function {
            info!(
                "Transfer function changed: {:?} -> {:?}",
                self.current_transfer_function, frame.transfer_function
            );
            self.current_transfer_function = frame.transfer_function;
        }

        if size_changed || format_changed {
            self.current_format = frame.format;
            self.video_size = (frame.width, frame.height);

            // Y texture is same for both formats (full resolution, R8)
            let y_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Y Texture"),
                size: wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            match frame.format {
                PixelFormat::NV12 => {
                    // NV12: UV plane is interleaved (Rg8, 2 bytes per pixel)
                    let uv_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("UV Texture (NV12)"),
                        size: wgpu::Extent3d {
                            width: uv_width,
                            height: uv_height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rg8Unorm, // 2-channel for interleaved UV
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });

                    let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

                    let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("NV12 Bind Group"),
                        layout: &self.nv12_bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&y_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(&uv_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&self.video_sampler),
                            },
                        ],
                    });

                    self.y_texture = Some(y_texture);
                    self.uv_texture = Some(uv_texture);
                    self.nv12_bind_group = Some(bind_group);
                    // Clear YUV420P textures
                    self.u_texture = None;
                    self.v_texture = None;
                    self.video_bind_group = None;

                    info!("NV12 textures created: {}x{} (UV: {}x{}) - GPU deinterleaving enabled (CPU scaler bypassed!)",
                        frame.width, frame.height, uv_width, uv_height);
                }
                PixelFormat::YUV420P => {
                    // YUV420P: Separate U and V planes (R8 each)
                    let u_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("U Texture"),
                        size: wgpu::Extent3d {
                            width: uv_width,
                            height: uv_height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::R8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });

                    let v_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("V Texture"),
                        size: wgpu::Extent3d {
                            width: uv_width,
                            height: uv_height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::R8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });

                    let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let u_view = u_texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let v_view = v_texture.create_view(&wgpu::TextureViewDescriptor::default());

                    let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("Video YUV Bind Group"),
                        layout: &self.video_bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&y_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(&u_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::TextureView(&v_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::Sampler(&self.video_sampler),
                            },
                        ],
                    });

                    self.y_texture = Some(y_texture);
                    self.u_texture = Some(u_texture);
                    self.v_texture = Some(v_texture);
                    self.video_bind_group = Some(bind_group);
                    // Clear NV12 textures
                    self.uv_texture = None;
                    self.nv12_bind_group = None;

                    info!("YUV420P textures created: {}x{} (UV: {}x{}) - GPU color conversion enabled",
                        frame.width, frame.height, uv_width, uv_height);
                }
                PixelFormat::P010 => {
                    // P010: 10-bit HDR with interleaved UV (similar to NV12 but 16-bit)
                    // For now, we treat it like NV12 - proper HDR support needs 16-bit textures
                    // TODO: Use Rg16Unorm for proper 10-bit support
                    let uv_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("UV Texture (P010/HDR)"),
                        size: wgpu::Extent3d {
                            width: uv_width,
                            height: uv_height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rg8Unorm, // TODO: Rg16Unorm for true 10-bit
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });

                    let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

                    let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("P010/HDR Bind Group"),
                        layout: &self.nv12_bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&y_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(&uv_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&self.video_sampler),
                            },
                        ],
                    });

                    self.y_texture = Some(y_texture);
                    self.uv_texture = Some(uv_texture);
                    self.nv12_bind_group = Some(bind_group);
                    self.u_texture = None;
                    self.v_texture = None;
                    self.video_bind_group = None;

                    info!(
                        "P010/HDR textures created: {}x{} (UV: {}x{}) - HDR mode (10-bit)",
                        frame.width, frame.height, uv_width, uv_height
                    );
                }
            }
        }

        // Upload Y plane (same for both formats)
        if let Some(ref texture) = self.y_texture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &frame.y_plane,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(frame.y_stride),
                    rows_per_image: Some(frame.height),
                },
                wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        match frame.format {
            PixelFormat::NV12 => {
                // Upload interleaved UV plane (Rg8)
                if let Some(ref texture) = self.uv_texture {
                    self.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &frame.u_plane, // NV12: u_plane contains interleaved UV data
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(frame.u_stride), // stride for interleaved UV
                            rows_per_image: Some(uv_height),
                        },
                        wgpu::Extent3d {
                            width: uv_width,
                            height: uv_height,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
            PixelFormat::YUV420P => {
                // Upload separate U and V planes
                if let Some(ref texture) = self.u_texture {
                    self.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &frame.u_plane,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(frame.u_stride),
                            rows_per_image: Some(uv_height),
                        },
                        wgpu::Extent3d {
                            width: uv_width,
                            height: uv_height,
                            depth_or_array_layers: 1,
                        },
                    );
                }

                if let Some(ref texture) = self.v_texture {
                    self.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &frame.v_plane,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(frame.v_stride),
                            rows_per_image: Some(uv_height),
                        },
                        wgpu::Extent3d {
                            width: uv_width,
                            height: uv_height,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
            PixelFormat::P010 => {
                // P010: Similar to NV12 but with 10-bit data in 16-bit words
                // For now, treat like NV12 (data truncated to 8-bit)
                if let Some(ref texture) = self.uv_texture {
                    self.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &frame.u_plane,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(frame.u_stride),
                            rows_per_image: Some(uv_height),
                        },
                        wgpu::Extent3d {
                            width: uv_width,
                            height: uv_height,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
        }

        // Mark this frame as uploaded to avoid redundant uploads
        self.last_uploaded_frame_id = frame.frame_id;
    }

    /// TRUE zero-copy video update using CVMetalTextureCache (macOS only)
    /// Creates Metal textures that share GPU memory with CVPixelBuffer - NO CPU COPY!
    /// Uses wgpu's hal layer to import Metal textures directly, avoiding all CPU involvement.
    #[cfg(target_os = "macos")]
    fn update_video_zero_copy(
        &mut self,
        frame: &VideoFrame,
        gpu_frame: &std::sync::Arc<crate::media::CVPixelBufferWrapper>,
        uv_width: u32,
        uv_height: u32,
    ) {
        use objc::runtime::Object;
        use objc::{msg_send, sel, sel_impl};

        // Use CVMetalTextureCache for true zero-copy (no CPU involvement)
        if self.zero_copy_enabled {
            if let Some(ref manager) = self.zero_copy_manager {
                // Create Metal textures directly from CVPixelBuffer - TRUE ZERO-COPY!
                // These textures share GPU memory with the decoded video frame
                if let Some((y_metal, uv_metal)) = manager.create_textures_from_cv_buffer(gpu_frame)
                {
                    // Check if we need to recreate wgpu textures (size changed)
                    let size_changed = self.video_size != (frame.width, frame.height);

                    if size_changed {
                        self.current_format = frame.format;
                        self.video_size = (frame.width, frame.height);

                        // Create wgpu textures that we'll blit into from Metal
                        let y_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                            label: Some("Y Texture (Zero-Copy Target)"),
                            size: wgpu::Extent3d {
                                width: frame.width,
                                height: frame.height,
                                depth_or_array_layers: 1,
                            },
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: wgpu::TextureFormat::R8Unorm,
                            usage: wgpu::TextureUsages::TEXTURE_BINDING
                                | wgpu::TextureUsages::COPY_DST
                                | wgpu::TextureUsages::RENDER_ATTACHMENT,
                            view_formats: &[],
                        });

                        let uv_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                            label: Some("UV Texture (Zero-Copy Target)"),
                            size: wgpu::Extent3d {
                                width: uv_width,
                                height: uv_height,
                                depth_or_array_layers: 1,
                            },
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: wgpu::TextureFormat::Rg8Unorm,
                            usage: wgpu::TextureUsages::TEXTURE_BINDING
                                | wgpu::TextureUsages::COPY_DST
                                | wgpu::TextureUsages::RENDER_ATTACHMENT,
                            view_formats: &[],
                        });

                        let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
                        let uv_view =
                            uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

                        let bind_group =
                            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                                label: Some("NV12 Bind Group (Zero-Copy)"),
                                layout: &self.nv12_bind_group_layout,
                                entries: &[
                                    wgpu::BindGroupEntry {
                                        binding: 0,
                                        resource: wgpu::BindingResource::TextureView(&y_view),
                                    },
                                    wgpu::BindGroupEntry {
                                        binding: 1,
                                        resource: wgpu::BindingResource::TextureView(&uv_view),
                                    },
                                    wgpu::BindGroupEntry {
                                        binding: 2,
                                        resource: wgpu::BindingResource::Sampler(
                                            &self.video_sampler,
                                        ),
                                    },
                                ],
                            });

                        self.y_texture = Some(y_texture);
                        self.uv_texture = Some(uv_texture);
                        self.nv12_bind_group = Some(bind_group);

                        log::info!(
                            "Zero-copy video textures created: {}x{} (UV: {}x{})",
                            frame.width,
                            frame.height,
                            uv_width,
                            uv_height
                        );
                    }

                    // GPU-to-GPU blit: Copy from CVMetalTexture to wgpu texture using Metal blit encoder
                    // This is entirely on GPU - no CPU involvement at all!
                    unsafe {
                        // Use the cached command queue from ZeroCopyTextureManager (created once, reused every frame)
                        let command_queue = manager.command_queue();

                        if !command_queue.is_null() {
                            let command_buffer: *mut Object =
                                msg_send![command_queue, commandBuffer];

                            if !command_buffer.is_null() {
                                let blit_encoder: *mut Object =
                                    msg_send![command_buffer, blitCommandEncoder];

                                if !blit_encoder.is_null() {
                                    // Get source Metal textures from CVMetalTexture
                                    let y_src = y_metal.metal_texture_ptr();
                                    let uv_src = uv_metal.metal_texture_ptr();

                                    // Get destination Metal textures from wgpu
                                    // wgpu on Metal stores the underlying MTLTexture
                                    if let (Some(ref y_dst_wgpu), Some(ref uv_dst_wgpu)) =
                                        (&self.y_texture, &self.uv_texture)
                                    {
                                        // Use wgpu's hal API to get underlying Metal textures
                                        let copied = self.blit_metal_textures(
                                            blit_encoder,
                                            y_src,
                                            uv_src,
                                            y_dst_wgpu,
                                            uv_dst_wgpu,
                                            frame.width,
                                            frame.height,
                                            uv_width,
                                            uv_height,
                                        );

                                        if copied {
                                            let _: () = msg_send![blit_encoder, endEncoding];
                                            let _: () = msg_send![command_buffer, commit];
                                            // NOTE: Not waiting for completion - GPU synchronization
                                            // is handled by the fact that we're rendering immediately after
                                            // and Metal will queue the operations correctly within the same frame

                                            // Store CVMetalTextures to keep them alive
                                            self.current_y_cv_texture = Some(y_metal);
                                            self.current_uv_cv_texture = Some(uv_metal);

                                            return; // Success! GPU-to-GPU copy complete
                                        }
                                    }

                                    let _: () = msg_send![blit_encoder, endEncoding];
                                }
                                // Don't commit if blit failed
                            }
                        }
                    }
                }
            }
        }

        // CPU fallback: Lock CVPixelBuffer and upload plane data to textures
        // This is slower than zero-copy but works on legacy Macs (2015 and earlier)
        // that don't support the Metal features required for zero-copy rendering
        if let Some(locked) = gpu_frame.lock_and_get_planes() {
            log::debug!("Using CPU fallback for video frame (legacy mode or zero-copy failed)");

            // Ensure textures exist
            let size_changed = self.video_size != (frame.width, frame.height);
            if size_changed || self.y_texture.is_none() {
                self.current_format = frame.format;
                self.video_size = (frame.width, frame.height);

                let y_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("Y Texture (CPU Fallback)"),
                    size: wgpu::Extent3d {
                        width: frame.width,
                        height: frame.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });

                let uv_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("UV Texture (CPU Fallback)"),
                    size: wgpu::Extent3d {
                        width: uv_width,
                        height: uv_height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rg8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });

                let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
                let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

                let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("NV12 Bind Group (CPU Fallback)"),
                    layout: &self.nv12_bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&y_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&uv_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&self.video_sampler),
                        },
                    ],
                });

                self.y_texture = Some(y_texture);
                self.uv_texture = Some(uv_texture);
                self.nv12_bind_group = Some(bind_group);

                log::info!(
                    "CPU fallback textures created: {}x{} (UV: {}x{})",
                    frame.width,
                    frame.height,
                    uv_width,
                    uv_height
                );
            }

            // Upload Y plane data
            if let Some(ref y_texture) = self.y_texture {
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: y_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    locked.y_data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(locked.y_stride),
                        rows_per_image: Some(locked.y_height),
                    },
                    wgpu::Extent3d {
                        width: frame.width,
                        height: frame.height,
                        depth_or_array_layers: 1,
                    },
                );
            }

            // Upload UV plane data
            if let Some(ref uv_texture) = self.uv_texture {
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: uv_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    locked.uv_data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(locked.uv_stride),
                        rows_per_image: Some(locked.uv_height),
                    },
                    wgpu::Extent3d {
                        width: uv_width,
                        height: uv_height,
                        depth_or_array_layers: 1,
                    },
                );
            }

            return; // CPU fallback succeeded
        }

        // If we get here, both zero-copy and CPU fallback failed
        log::warn!(
            "Both GPU blit and CPU fallback failed - frame dropped (zero_copy_enabled={}, manager={})",
            self.zero_copy_enabled,
            self.zero_copy_manager.is_some()
        );
    }

    /// Update video textures from D3D11 hardware-decoded frame (Windows)
    /// Copies from D3D11 staging texture to wgpu - faster than FFmpeg's av_hwframe_transfer_data
    /// because we skip FFmpeg's intermediate copies and work directly with decoder output
    #[cfg(target_os = "windows")]
    fn update_video_d3d11(
        &mut self,
        frame: &VideoFrame,
        gpu_frame: &std::sync::Arc<D3D11TextureWrapper>,
        uv_width: u32,
        uv_height: u32,
    ) {
        log::info!(
            "update_video_d3d11: {}x{}, array_index={}, is_texture_array={}",
            frame.width,
            frame.height,
            gpu_frame.array_index(),
            gpu_frame.is_texture_array()
        );

        // Skip zero-copy for texture arrays (array_index > 0 means it's part of an array)
        // The zero-copy path doesn't properly handle texture array slices yet
        // The CPU path correctly uses CopySubresourceRegion with the array_index
        let is_texture_array = gpu_frame.array_index() > 0 || gpu_frame.is_texture_array();

        if is_texture_array {
            log::info!(
                "D3D11: Using CPU path for texture array (array_index={})",
                gpu_frame.array_index()
            );
        }

        // Try zero-copy via Shared Handle first (only for non-array textures)
        // This eliminates the CPU copy by importing the D3D11 texture directly into DX12
        if !is_texture_array {
            if let Ok(handle) = gpu_frame.get_shared_handle() {
                let mut handle_changed = false;

                // Check if we need to re-import (handle changed or texture missing)
                let needs_import = match self.current_imported_handle {
                    Some(current) => current != handle,
                    None => true,
                };

                if needs_import || self.current_imported_texture.is_none() {
                    // Import the shared handle into DX12
                    // We must use unsafe to access the raw DX12 device via wgpu-hal
                    let imported_texture = unsafe {
                        match self.device.as_hal::<wgpu_hal::dx12::Api>() {
                            Some(hal_device) => {
                                let d3d12_device: &ID3D12Device = hal_device.raw_device();

                                // Open the shared handle as a D3D12 resource
                                let mut resource: Option<ID3D12Resource> = None;
                                if let Err(e) = d3d12_device.OpenSharedHandle(handle, &mut resource)
                                {
                                    warn!("Failed to OpenSharedHandle: {:?}", e);
                                    return; // Fallback to CPU copy
                                }
                                let resource = resource.unwrap();

                                // Wrap it in a wgpu::Texture
                                let size = wgpu::Extent3d {
                                    width: frame.width,
                                    height: frame.height,
                                    depth_or_array_layers: 1,
                                };

                                let format = wgpu::TextureFormat::NV12;
                                let usage = wgpu::TextureUsages::TEXTURE_BINDING
                                    | wgpu::TextureUsages::COPY_DST;

                                // Create wgpu-hal texture from raw resource
                                let hal_texture = wgpu_hal::dx12::Device::texture_from_raw(
                                    resource,
                                    format,
                                    wgpu::TextureDimension::D2,
                                    size,
                                    1, // mip_levels
                                    1, // sample_count
                                );

                                // Create wgpu Texture from HAL texture
                                let descriptor = wgpu::TextureDescriptor {
                                    label: Some("Imported D3D11 Texture"),
                                    size,
                                    mip_level_count: 1,
                                    sample_count: 1,
                                    dimension: wgpu::TextureDimension::D2,
                                    format,
                                    usage,
                                    view_formats: &[],
                                };

                                Some(self.device.create_texture_from_hal::<wgpu_hal::dx12::Api>(
                                    hal_texture,
                                    &descriptor,
                                ))
                            }
                            None => {
                                warn!("Failed to get DX12 HAL device");
                                None
                            }
                        }
                    };

                    if let Some(texture) = imported_texture {
                        self.current_imported_texture = Some(texture);
                        self.current_imported_handle = Some(handle);
                        handle_changed = true;
                        // Log success once per session or on change
                        debug!(
                            "Zero-copy: Imported D3D11 texture handle {:?} -> DX12",
                            handle
                        );
                    } else {
                        // Import failed - clear cache and fall through to CPU path
                        self.current_imported_handle = None;
                        self.current_imported_texture = None;
                    }
                }

                // If we have a valid imported texture, use it!
                if let Some(ref texture) = self.current_imported_texture {
                    // If the handle changed OR if we don't have an external texture bind group yet (e.g. resize)
                    // we need to recreate the bind group.
                    // Note: video_size check handles resolution changes
                    let size_changed = self.video_size != (frame.width, frame.height);

                    if handle_changed || size_changed || self.external_texture_bind_group.is_none()
                    {
                        self.video_size = (frame.width, frame.height);
                        self.current_format = PixelFormat::NV12;

                        // Create views for Y and UV planes
                        let y_view = texture.create_view(&wgpu::TextureViewDescriptor {
                            label: Some("Plane 0 View"),
                            aspect: wgpu::TextureAspect::Plane0,
                            ..Default::default()
                        });

                        let uv_view = texture.create_view(&wgpu::TextureViewDescriptor {
                            label: Some("Plane 1 View"),
                            aspect: wgpu::TextureAspect::Plane1,
                            ..Default::default()
                        });

                        // Create ExternalTexture with color space aware conversion
                        // Select YUV to RGB conversion matrix based on color space
                        let yuv_conversion_matrix: [f32; 16] = match frame.color_space {
                            ColorSpace::BT709 => [
                                1.0, 1.0, 1.0, 0.0, 0.0, -0.1873, 1.8556, 0.0, 1.5748, -0.4681,
                                0.0, 0.0, -0.7874, 0.3277, -0.9278, 1.0,
                            ],
                            ColorSpace::BT601 => [
                                1.0, 1.0, 1.0, 0.0, 0.0, -0.344, 1.772, 0.0, 1.402, -0.714, 0.0,
                                0.0, -0.701, 0.529, -0.886, 1.0,
                            ],
                            ColorSpace::BT2020 => [
                                1.0, 1.0, 1.0, 0.0, 0.0, -0.1646, 1.8814, 0.0, 1.4746, -0.5714,
                                0.0, 0.0, -0.7373, 0.3680, -0.9407, 1.0,
                            ],
                        };

                        let gamut_conversion_matrix: [f32; 9] = match frame.color_space {
                            ColorSpace::BT2020 => [
                                1.6605, -0.5876, -0.0728, -0.1246, 1.1329, -0.0083, -0.0182,
                                -0.1006, 1.1187,
                            ],
                            _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                        };

                        // HDR transfer function handling (same as CPU path)
                        // Based on NVIDIA GFN client TrueHDR analysis
                        let (src_transfer, dst_transfer) = match frame.transfer_function {
                            TransferFunction::PQ => {
                                // PQ (SMPTE ST 2084) HDR content
                                // Use moderate gamma to decode PQ and compress dynamic range
                                let pq_decode = wgpu::ExternalTextureTransferFunction {
                                    a: 1.0,
                                    b: 0.0,
                                    g: 1.8, // Moderate gamma for PQ decode
                                    k: 0.0,
                                };
                                let sdr_encode = wgpu::ExternalTextureTransferFunction {
                                    a: 1.0,
                                    b: 0.0,
                                    g: 0.55, // Re-encode to SDR gamma
                                    k: 0.0,
                                };
                                (pq_decode, sdr_encode)
                            }
                            TransferFunction::HLG => {
                                // HLG is backwards-compatible with SDR displays
                                let hlg_decode = wgpu::ExternalTextureTransferFunction {
                                    a: 1.0,
                                    b: 0.0,
                                    g: 1.2,
                                    k: 0.0,
                                };
                                let sdr_encode = wgpu::ExternalTextureTransferFunction {
                                    a: 1.0,
                                    b: 0.0,
                                    g: 0.85,
                                    k: 0.0,
                                };
                                (hlg_decode, sdr_encode)
                            }
                            TransferFunction::SDR => {
                                // SDR: identity transfer
                                let identity = wgpu::ExternalTextureTransferFunction {
                                    a: 1.0,
                                    b: 0.0,
                                    g: 1.0,
                                    k: 1.0,
                                };
                                (identity.clone(), identity)
                            }
                        };

                        let identity_transform: [f32; 6] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];

                        let external_texture = self.device.create_external_texture(
                            &wgpu::ExternalTextureDescriptor {
                                label: Some("Zero-Copy External Texture"),
                                width: frame.width,
                                height: frame.height,
                                format: wgpu::ExternalTextureFormat::Nv12,
                                yuv_conversion_matrix,
                                gamut_conversion_matrix,
                                src_transfer_function: src_transfer,
                                dst_transfer_function: dst_transfer,
                                sample_transform: identity_transform,
                                load_transform: identity_transform,
                            },
                            &[&y_view, &uv_view],
                        );

                        if let Some(ref layout) = self.external_texture_bind_group_layout {
                            let bind_group =
                                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                                    label: Some("Zero-Copy Bind Group"),
                                    layout,
                                    entries: &[
                                        wgpu::BindGroupEntry {
                                            binding: 0,
                                            resource: wgpu::BindingResource::ExternalTexture(
                                                &external_texture,
                                            ),
                                        },
                                        wgpu::BindGroupEntry {
                                            binding: 1,
                                            resource: wgpu::BindingResource::Sampler(
                                                &self.video_sampler,
                                            ),
                                        },
                                    ],
                                });

                            self.external_texture_bind_group = Some(bind_group);
                            self.external_texture = Some(external_texture);
                            log::info!(
                                "Zero-copy pipeline configured for {}x{}",
                                frame.width,
                                frame.height
                            );
                        }
                    }

                    // Success! We are set up for zero-copy rendering.
                    return;
                }
            }
        } // end if !is_texture_array

        // Fallback: Lock the D3D11 texture and get plane data (CPU Copy)
        log::info!("D3D11: Locking texture for CPU copy...");
        let planes = match gpu_frame.lock_and_get_planes() {
            Ok(p) => {
                log::info!(
                    "D3D11: Got planes - y_size={}, uv_size={}, stride={}",
                    p.y_plane.len(),
                    p.uv_plane.len(),
                    p.y_stride
                );

                // Debug: Check Y plane data content
                if !p.y_plane.is_empty() {
                    // Sample first few rows to check if data is valid or all zeros/gray
                    let sample_size = std::cmp::min(256, p.y_plane.len());
                    let sample = &p.y_plane[..sample_size];
                    let min_y = sample.iter().min().copied().unwrap_or(0);
                    let max_y = sample.iter().max().copied().unwrap_or(0);
                    let avg_y: u32 =
                        sample.iter().map(|&x| x as u32).sum::<u32>() / sample.len() as u32;
                    log::info!(
                        "D3D11: Y plane stats (first {} bytes): min={}, max={}, avg={}",
                        sample_size,
                        min_y,
                        max_y,
                        avg_y
                    );

                    // Also sample middle of the frame
                    let mid_offset = p.y_plane.len() / 2;
                    if mid_offset + 256 <= p.y_plane.len() {
                        let mid_sample = &p.y_plane[mid_offset..mid_offset + 256];
                        let mid_min = mid_sample.iter().min().copied().unwrap_or(0);
                        let mid_max = mid_sample.iter().max().copied().unwrap_or(0);
                        let mid_avg: u32 = mid_sample.iter().map(|&x| x as u32).sum::<u32>() / 256;
                        log::info!(
                            "D3D11: Y plane middle stats: min={}, max={}, avg={}",
                            mid_min,
                            mid_max,
                            mid_avg
                        );
                    }
                }

                p
            }
            Err(e) => {
                log::warn!("Failed to lock D3D11 texture: {:?}", e);
                return;
            }
        };

        // Check if we need to recreate textures (size change)
        let size_changed = self.video_size != (frame.width, frame.height);
        log::info!(
            "D3D11: size_changed={}, video_size={:?}, frame_size={}x{}",
            size_changed,
            self.video_size,
            frame.width,
            frame.height
        );

        if size_changed {
            self.video_size = (frame.width, frame.height);
            self.current_format = PixelFormat::NV12;

            // Create Y texture (full resolution, R8)
            let y_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Y Texture (D3D11)"),
                size: wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            // Create UV texture for NV12 (Rg8 interleaved)
            let uv_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("UV Texture (D3D11)"),
                size: wgpu::Extent3d {
                    width: uv_width,
                    height: uv_height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rg8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("NV12 Bind Group (D3D11)"),
                layout: &self.nv12_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&y_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&uv_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.video_sampler),
                    },
                ],
            });

            self.y_texture = Some(y_texture);
            self.uv_texture = Some(uv_texture);
            self.nv12_bind_group = Some(bind_group);
            // Clear YUV420P textures
            self.u_texture = None;
            self.v_texture = None;
            self.video_bind_group = None;

            log::info!(
                "D3D11 video textures created: {}x{} (UV: {}x{})",
                frame.width,
                frame.height,
                uv_width,
                uv_height
            );
        }

        // Upload Y plane from D3D11 staging texture
        if let Some(ref texture) = self.y_texture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &planes.y_plane,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(planes.y_stride),
                    rows_per_image: Some(planes.height),
                },
                wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        // Upload UV plane from D3D11 staging texture
        if let Some(ref texture) = self.uv_texture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &planes.uv_plane,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(planes.uv_stride),
                    rows_per_image: Some(uv_height),
                },
                wgpu::Extent3d {
                    width: uv_width,
                    height: uv_height,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    /// Update video from VAAPI surface via DMA-BUF (Linux)
    ///
    /// This provides zero-copy rendering by importing the DMA-BUF from VAAPI
    /// directly into Vulkan via VK_EXT_external_memory_dma_buf.
    ///
    /// Flow:
    /// 1. VAAPI decoder outputs VASurface in GPU VRAM
    /// 2. We export VASurface as DMA-BUF fd via vaExportSurfaceHandle
    /// 3. Import DMA-BUF into Vulkan via VK_EXT_external_memory_dma_buf
    /// 4. Bind to wgpu texture for rendering
    ///
    /// Fallback: If DMA-BUF import fails, we mmap and copy to CPU (still faster
    /// than FFmpeg's sw_transfer since we avoid the intermediate copy).
    #[cfg(target_os = "linux")]
    fn update_video_vaapi(
        &mut self,
        frame: &VideoFrame,
        gpu_frame: &std::sync::Arc<VAAPISurfaceWrapper>,
        uv_width: u32,
        uv_height: u32,
    ) {
        // TODO: Implement true zero-copy via VK_EXT_external_memory_dma_buf
        // This requires:
        // 1. Check for VK_EXT_external_memory_dma_buf extension
        // 2. Export DMA-BUF fd from VAAPI surface
        // 3. Create VkImage with VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT
        // 4. Import into wgpu via hal layer
        //
        // For now, use the fallback path: mmap the DMA-BUF and upload to GPU
        // This is still faster than FFmpeg's sw_transfer because:
        // - We skip FFmpeg's intermediate buffer allocation
        // - We read directly from the GPU-accessible DMA-BUF
        // - The DMA-BUF may be in CPU-cached memory for faster reads

        // Try to get plane data from the VAAPI surface
        let planes = match gpu_frame.lock_and_get_planes() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Failed to lock VAAPI surface: {:?}", e);
                return;
            }
        };

        // Check if we need to recreate textures (size change)
        let size_changed = self.video_size != (frame.width, frame.height);

        if size_changed {
            self.video_size = (frame.width, frame.height);
            self.current_format = PixelFormat::NV12;

            // Create Y texture (full resolution, R8)
            let y_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Y Texture (VAAPI)"),
                size: wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            // Create UV texture for NV12 (Rg8 interleaved)
            let uv_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("UV Texture (VAAPI)"),
                size: wgpu::Extent3d {
                    width: uv_width,
                    height: uv_height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rg8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("NV12 Bind Group (VAAPI)"),
                layout: &self.nv12_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&y_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&uv_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.video_sampler),
                    },
                ],
            });

            self.y_texture = Some(y_texture);
            self.uv_texture = Some(uv_texture);
            self.nv12_bind_group = Some(bind_group);

            log::info!(
                "VAAPI video textures created: {}x{} (UV: {}x{})",
                frame.width,
                frame.height,
                uv_width,
                uv_height
            );
        }

        // Upload Y plane from VAAPI DMA-BUF
        if let Some(ref texture) = self.y_texture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &planes.y_plane,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(planes.y_stride),
                    rows_per_image: Some(planes.height),
                },
                wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        // Upload UV plane from VAAPI DMA-BUF
        if let Some(ref texture) = self.uv_texture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &planes.uv_plane,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(planes.uv_stride),
                    rows_per_image: Some(uv_height),
                },
                wgpu::Extent3d {
                    width: uv_width,
                    height: uv_height,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    /// Update video using ExternalTexture for hardware YUV->RGB conversion
    /// This uses wgpu's ExternalTexture API which provides hardware-accelerated
    /// color space conversion on supported platforms (DX12, Metal, Vulkan)
    fn update_video_external_texture(&mut self, frame: &VideoFrame, uv_width: u32, uv_height: u32) {
        // Check if we need to recreate textures (size change)
        let size_changed = self.video_size != (frame.width, frame.height);

        if size_changed {
            self.video_size = (frame.width, frame.height);
            self.current_format = PixelFormat::NV12;

            // Create Y texture (full resolution, R8)
            let y_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Y Texture (External)"),
                size: wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            // Create UV texture for NV12 (Rg8 interleaved)
            let uv_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("UV Texture (External)"),
                size: wgpu::Extent3d {
                    width: uv_width,
                    height: uv_height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rg8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            self.y_texture = Some(y_texture);
            self.uv_texture = Some(uv_texture);

            log::info!(
                "External Texture video created: {}x{} (hardware YUV->RGB)",
                frame.width,
                frame.height
            );
        }

        // Upload Y plane
        if let Some(ref texture) = self.y_texture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &frame.y_plane,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(frame.y_stride),
                    rows_per_image: Some(frame.height),
                },
                wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        // Upload UV plane
        if let Some(ref texture) = self.uv_texture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &frame.u_plane, // u_plane contains interleaved UV for NV12
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(frame.u_stride),
                    rows_per_image: Some(uv_height),
                },
                wgpu::Extent3d {
                    width: uv_width,
                    height: uv_height,
                    depth_or_array_layers: 1,
                },
            );
        }

        // Create texture views for ExternalTexture
        let y_view = self
            .y_texture
            .as_ref()
            .unwrap()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = self
            .uv_texture
            .as_ref()
            .unwrap()
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Select YUV to RGB conversion matrix based on color space
        // All matrices are for Full Range (PC levels: Y 0-255, UV 0-255)
        // With UV offset of -0.5 baked into the matrix offsets
        let yuv_conversion_matrix: [f32; 16] = match frame.color_space {
            ColorSpace::BT709 => [
                // BT.709 Full Range: R = Y + 1.5748*V, G = Y - 0.1873*U - 0.4681*V, B = Y + 1.8556*U
                1.0, 1.0, 1.0, 0.0, // Column 0: Y coefficients
                0.0, -0.1873, 1.8556, 0.0, // Column 1: U coefficients
                1.5748, -0.4681, 0.0, 0.0, // Column 2: V coefficients
                -0.7874, 0.3277, -0.9278, 1.0, // Column 3: Offsets
            ],
            ColorSpace::BT601 => [
                // BT.601 Full Range: R = Y + 1.402*V, G = Y - 0.344*U - 0.714*V, B = Y + 1.772*U
                1.0, 1.0, 1.0, 0.0, // Column 0: Y coefficients
                0.0, -0.344, 1.772, 0.0, // Column 1: U coefficients
                1.402, -0.714, 0.0, 0.0, // Column 2: V coefficients
                -0.701, 0.529, -0.886, 1.0, // Column 3: Offsets
            ],
            ColorSpace::BT2020 => [
                // BT.2020 Full Range (NCL): R = Y + 1.4746*V, G = Y - 0.1646*U - 0.5714*V, B = Y + 1.8814*U
                1.0, 1.0, 1.0, 0.0, // Column 0: Y coefficients
                0.0, -0.1646, 1.8814, 0.0, // Column 1: U coefficients
                1.4746, -0.5714, 0.0, 0.0, // Column 2: V coefficients
                -0.7373, 0.3680, -0.9407, 1.0, // Column 3: Offsets
            ],
        };

        // For HDR (BT.2020), convert gamut from BT.2020 to sRGB/BT.709 primaries
        let gamut_conversion_matrix: [f32; 9] = match frame.color_space {
            ColorSpace::BT2020 => [
                // BT.2020 to BT.709 gamut conversion (row-major for wgpu)
                1.6605, -0.5876, -0.0728, -0.1246, 1.1329, -0.0083, -0.0182, -0.1006, 1.1187,
            ],
            _ => [
                // Identity (no gamut conversion for BT.709/BT.601)
                1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
            ],
        };

        // Select transfer function based on HDR mode
        // For SDR: identity (video is already gamma-corrected in BT.709)
        // For HDR PQ: apply tone mapping to convert to SDR display range
        //
        // Based on NVIDIA GFN client analysis, they use proper TrueHDR processing
        // with parameters like TrueHdrMiddleGrey, TrueHdrContrast, TrueHdrSaturation
        //
        // The wgpu ExternalTextureTransferFunction formula is:
        //   For E < k: L = a * E
        //   For E >= k: L = a * pow((E + b) / (1 + b), g)
        //
        // For PQ to SDR conversion, we need to:
        // 1. Decode PQ to linear light (linearize)
        // 2. Tone map from HDR range (0-10000 nits) to SDR range (0-100 nits)
        // 3. Re-encode to sRGB gamma
        let (src_transfer, dst_transfer) = match frame.transfer_function {
            TransferFunction::PQ => {
                // PQ (SMPTE ST 2084) HDR content
                // The PQ EOTF is complex, but we can approximate with gamma
                // PQ encoded values map roughly: 0.5 PQ ≈ 100 nits (SDR reference white)
                //
                // Simplified approach: Use a moderate gamma to decode PQ
                // and compress the dynamic range while preserving detail
                // A gamma of ~1.8 gives good results for tone mapping PQ to SDR
                let pq_decode = wgpu::ExternalTextureTransferFunction {
                    a: 1.0, // Scale factor
                    b: 0.0, // Offset
                    g: 1.8, // Moderate gamma for PQ decode (was 2.4, too aggressive)
                    k: 0.0, // Threshold
                };
                // Re-encode to SDR sRGB gamma
                // sRGB uses gamma 2.2, but 0.45 (1/2.2) for encoding
                let sdr_encode = wgpu::ExternalTextureTransferFunction {
                    a: 1.0,
                    b: 0.0,
                    g: 0.55, // Slightly stronger than 1/2.2 to brighten shadows
                    k: 0.0,
                };
                (pq_decode, sdr_encode)
            }
            TransferFunction::HLG => {
                // HLG (Hybrid Log-Gamma) is designed to be backwards-compatible
                // with SDR displays, so less aggressive tone mapping is needed
                let hlg_decode = wgpu::ExternalTextureTransferFunction {
                    a: 1.0,
                    b: 0.0,
                    g: 1.2, // Mild gamma adjustment for HLG
                    k: 0.0,
                };
                // HLG is mostly compatible with gamma 2.4 displays
                let sdr_encode = wgpu::ExternalTextureTransferFunction {
                    a: 1.0,
                    b: 0.0,
                    g: 0.85, // Slight adjustment for SDR display
                    k: 0.0,
                };
                (hlg_decode, sdr_encode)
            }
            TransferFunction::SDR => {
                // SDR: identity transfer (video is pre-gamma-corrected in BT.709)
                let identity = wgpu::ExternalTextureTransferFunction {
                    a: 1.0,
                    b: 0.0,
                    g: 1.0, // Linear passthrough
                    k: 1.0, // k=1 makes everything use the linear path (a*E)
                };
                (identity.clone(), identity)
            }
        };

        // Identity transforms for texture coordinates
        let identity_transform: [f32; 6] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];

        // Create ExternalTexture
        let external_texture = self.device.create_external_texture(
            &wgpu::ExternalTextureDescriptor {
                label: Some("Video External Texture"),
                width: frame.width,
                height: frame.height,
                format: wgpu::ExternalTextureFormat::Nv12,
                yuv_conversion_matrix,
                gamut_conversion_matrix,
                src_transfer_function: src_transfer,
                dst_transfer_function: dst_transfer,
                sample_transform: identity_transform,
                load_transform: identity_transform,
            },
            &[&y_view, &uv_view],
        );

        // Create bind group with external texture and sampler
        if let Some(ref layout) = self.external_texture_bind_group_layout {
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("External Texture Bind Group"),
                layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::ExternalTexture(&external_texture),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.video_sampler),
                    },
                ],
            });

            self.external_texture_bind_group = Some(bind_group);
            self.external_texture = Some(external_texture);
        }
    }

    /// Helper function to blit Metal textures using wgpu's hal layer
    /// Returns true if the blit was successful
    #[cfg(target_os = "macos")]
    unsafe fn blit_metal_textures(
        &self,
        blit_encoder: *mut objc::runtime::Object,
        y_src: *mut objc::runtime::Object,
        uv_src: *mut objc::runtime::Object,
        y_dst_wgpu: &wgpu::Texture,
        uv_dst_wgpu: &wgpu::Texture,
        y_width: u32,
        y_height: u32,
        uv_width: u32,
        uv_height: u32,
    ) -> bool {
        use objc::{msg_send, sel, sel_impl};

        // Define MTLOrigin and MTLSize structs for Metal API
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct MTLOrigin {
            x: u64,
            y: u64,
            z: u64,
        }
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct MTLSize {
            width: u64,
            height: u64,
            depth: u64,
        }

        let origin = MTLOrigin { x: 0, y: 0, z: 0 };

        // wgpu 27 as_hal API: returns Option<impl Deref<Target = A::Texture>>
        // IMPORTANT: as_hal holds a read lock - we must get one pointer and drop the result
        // before getting the next, otherwise we get a recursive lock panic.

        // Get Y texture pointer and drop hal reference immediately
        let y_dst: Option<*mut objc::runtime::Object> = {
            let y_hal = y_dst_wgpu.as_hal::<wgpu_hal::metal::Api>();
            y_hal.map(|y_hal_tex| {
                let y_metal_tex = (*y_hal_tex).raw_handle();
                *(y_metal_tex as *const _ as *const *mut objc::runtime::Object)
            })
        }; // y_hal dropped here, lock released

        // Get UV texture pointer (now safe - Y's lock is released)
        let uv_dst: Option<*mut objc::runtime::Object> = {
            let uv_hal = uv_dst_wgpu.as_hal::<wgpu_hal::metal::Api>();
            uv_hal.map(|uv_hal_tex| {
                let uv_metal_tex = (*uv_hal_tex).raw_handle();
                *(uv_metal_tex as *const _ as *const *mut objc::runtime::Object)
            })
        }; // uv_hal dropped here

        if let (Some(y_dst), Some(uv_dst)) = (y_dst, uv_dst) {
            // Blit Y texture (GPU-to-GPU copy)
            let y_size = MTLSize {
                width: y_width as u64,
                height: y_height as u64,
                depth: 1,
            };
            let _: () = msg_send![blit_encoder,
                copyFromTexture: y_src
                sourceSlice: 0u64
                sourceLevel: 0u64
                sourceOrigin: origin
                sourceSize: y_size
                toTexture: y_dst as *mut objc::runtime::Object
                destinationSlice: 0u64
                destinationLevel: 0u64
                destinationOrigin: origin
            ];

            // Blit UV texture (GPU-to-GPU copy)
            let uv_size = MTLSize {
                width: uv_width as u64,
                height: uv_height as u64,
                depth: 1,
            };
            let uv_origin = MTLOrigin { x: 0, y: 0, z: 0 };
            let _: () = msg_send![blit_encoder,
                copyFromTexture: uv_src
                sourceSlice: 0u64
                sourceLevel: 0u64
                sourceOrigin: uv_origin
                sourceSize: uv_size
                toTexture: uv_dst as *mut objc::runtime::Object
                destinationSlice: 0u64
                destinationLevel: 0u64
                destinationOrigin: uv_origin
            ];

            log::trace!(
                "GPU blit: Y {}x{}, UV {}x{}",
                y_width,
                y_height,
                uv_width,
                uv_height
            );
            return true;
        }

        log::debug!("Could not get Metal textures from wgpu for GPU blit");
        false
    }

    /// Render video frame to screen
    /// Automatically selects the correct pipeline based on current pixel format
    /// Priority: External Texture (true zero-copy) > NV12 > YUV420P
    fn render_video(&self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
        // Priority 1: Use External Texture pipeline if available (hardware YUV->RGB conversion)
        // This is the true zero-copy path with automatic color space conversion
        if let (Some(ref pipeline), Some(ref bind_group)) = (
            &self.external_texture_pipeline,
            &self.external_texture_bind_group,
        ) {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Video Pass (External Texture)"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });

            render_pass.set_pipeline(pipeline);
            render_pass.set_bind_group(0, bind_group, &[]);
            render_pass.draw(0..6, 0..1);
            return;
        }

        // Priority 2: Fallback to format-specific pipelines (manual YUV->RGB in shader)
        let (pipeline, bind_group) = match self.current_format {
            PixelFormat::NV12 => {
                if let Some(ref bg) = self.nv12_bind_group {
                    // Use HDR tone mapping pipeline for PQ content on SDR displays
                    let pipeline = if self.current_transfer_function == TransferFunction::PQ {
                        &self.nv12_hdr_pipeline
                    } else {
                        &self.nv12_pipeline
                    };
                    (pipeline, bg)
                } else {
                    return; // No bind group ready
                }
            }
            PixelFormat::YUV420P => {
                if let Some(ref bg) = self.video_bind_group {
                    (&self.video_pipeline, bg)
                } else {
                    return; // No bind group ready
                }
            }
            PixelFormat::P010 => {
                // P010 is 10-bit HDR format - use HDR pipeline for PQ content
                if let Some(ref bg) = self.nv12_bind_group {
                    let pipeline = if self.current_transfer_function == TransferFunction::PQ {
                        &self.nv12_hdr_pipeline
                    } else {
                        &self.nv12_pipeline
                    };
                    (pipeline, bg)
                } else {
                    return; // No bind group ready
                }
            }
        };

        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Video Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });

        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.draw(0..6, 0..1); // Draw 6 vertices (2 triangles = 1 quad)
    }

    /// Render frame and return UI actions plus optional repaint delay
    /// The Duration indicates when the next repaint should happen (for idle throttling)
    pub fn render(&mut self, app: &App) -> Result<(Vec<UiAction>, Option<Duration>)> {
        profile_scope!("render");

        // Get surface texture with SMART error recovery for swapchain issues
        // Key insight: During fullscreen transitions, the window size updates AFTER
        // the surface error occurs. If we immediately "recover" with the old size,
        // we force DWM composition (scaling), causing 60Hz lock and input lag.
        // Instead, we YIELD to the event loop to let the Resized event propagate.
        let output = match self.surface.get_current_texture() {
            Ok(texture) => {
                // Success - reset error counter
                self.consecutive_surface_errors = 0;
                texture
            }
            Err(wgpu::SurfaceError::Outdated) | Err(wgpu::SurfaceError::Lost) => {
                self.consecutive_surface_errors += 1;

                // Check if window size differs from our config (resize pending)
                let current_window_size = self.window.inner_size();
                let config_matches_window = current_window_size.width == self.config.width
                    && current_window_size.height == self.config.height;

                if !config_matches_window {
                    // Window size changed - resize event should handle this
                    // Call resize directly to sync up
                    debug!(
                        "Swapchain outdated: window {}x{} != config {}x{} - resizing",
                        current_window_size.width,
                        current_window_size.height,
                        self.config.width,
                        self.config.height
                    );
                    self.resize(current_window_size);

                    // Retry after resize
                    match self.surface.get_current_texture() {
                        Ok(texture) => {
                            self.consecutive_surface_errors = 0;
                            info!(
                                "Swapchain recovered after resize to {}x{}",
                                current_window_size.width, current_window_size.height
                            );
                            texture
                        }
                        Err(e) => {
                            debug!("Still failing after resize: {} - yielding", e);
                            return Ok((vec![], None));
                        }
                    }
                } else if self.consecutive_surface_errors < 10 {
                    // Sizes match but surface is outdated - likely a race condition
                    // YIELD to event loop to let Resized event arrive with correct size
                    debug!(
                        "Swapchain outdated (attempt {}/10): sizes match {}x{} - yielding to event loop",
                        self.consecutive_surface_errors,
                        self.config.width, self.config.height
                    );
                    return Ok((vec![], None));
                } else {
                    // Persistent error (10+ frames) - force recovery as fallback
                    warn!(
                        "Swapchain persistently outdated ({} attempts) - forcing recovery",
                        self.consecutive_surface_errors
                    );
                    if !self.recover_swapchain() {
                        return Ok((vec![], None));
                    }
                    match self.surface.get_current_texture() {
                        Ok(texture) => {
                            self.consecutive_surface_errors = 0;
                            texture
                        }
                        Err(e) => {
                            warn!("Failed to get texture after forced recovery: {}", e);
                            return Ok((vec![], None));
                        }
                    }
                }
            }
            Err(wgpu::SurfaceError::Timeout) => {
                // GPU is busy, skip this frame
                debug!("Surface timeout - skipping frame");
                return Ok((vec![], None));
            }
            Err(e) => {
                // Fatal error (e.g., OutOfMemory)
                return Err(anyhow::anyhow!("Surface error: {}", e));
            }
        };

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });

        // Update video texture if we have a frame
        if let Some(ref frame) = app.current_frame {
            profile_scope!("update_video");
            self.update_video(frame);
        }

        // Render video or clear based on state
        // Check for either YUV420P (video_bind_group) or NV12 (nv12_bind_group)
        let has_video = self.video_bind_group.is_some() || self.nv12_bind_group.is_some();
        if app.state == AppState::Streaming && has_video {
            profile_scope!("render_video");
            // Render video full-screen
            self.render_video(&mut encoder, &view);
        } else {
            // Clear pass for non-streaming states
            let _render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Clear Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.08,
                            g: 0.08,
                            b: 0.12,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
        }

        // Draw egui UI and collect actions
        let raw_input = self.egui_state.take_egui_input(&self.window);
        let mut actions: Vec<UiAction> = Vec::new();

        // === UI Optimization: Throttle stats updates to 200ms ===
        // This dramatically reduces CPU usage from stats panel rendering
        const STATS_UPDATE_INTERVAL: Duration = Duration::from_millis(200);
        if self.stats_last_update.elapsed() >= STATS_UPDATE_INTERVAL {
            self.cached_stats = Some(app.stats.clone());
            self.stats_last_update = Instant::now();

            // Detect resolution changes and show notification
            if !app.stats.resolution.is_empty() && app.stats.resolution != self.last_resolution {
                if !self.last_resolution.is_empty() {
                    // Resolution changed - create notification
                    self.resolution_notification = Some(ResolutionNotification::new(
                        &self.last_resolution,
                        &app.stats.resolution,
                    ));
                }
                self.last_resolution = app.stats.resolution.clone();
            }

            // Detect racing wheel connection and show notification
            if app.stats.wheel_count > 0 && app.stats.wheel_count != self.last_wheel_count {
                self.show_wheel_notification(app.stats.wheel_count);
            } else if app.stats.wheel_count == 0 && self.last_wheel_count > 0 {
                // Wheels disconnected - reset state
                self.last_wheel_count = 0;
            }
        }

        // Clean up expired notifications
        if let Some(ref notif) = self.resolution_notification {
            if notif.is_expired() {
                self.resolution_notification = None;
            }
        }
        if let Some(ref notif) = self.wheel_notification {
            if notif.is_expired() {
                self.wheel_notification = None;
            }
        }

        // Extract state needed for UI rendering
        let app_state = app.state;
        // Use cached stats for display (throttled to 200ms updates)
        let stats = self
            .cached_stats
            .clone()
            .unwrap_or_else(|| app.stats.clone());
        let show_stats = app.show_stats;
        let status_message = app.status_message.clone();
        let error_message = app.error_message.clone();
        let selected_game = app.selected_game.clone();
        let stats_position = self.stats_panel.position;
        let stats_visible = self.stats_panel.visible;
        let show_settings = app.show_settings;
        let settings = app.settings.clone();
        let login_providers = app.login_providers.clone();
        let selected_provider_index = app.selected_provider_index;
        let is_loading = app.is_loading;
        let login_url = app.login_url.clone();
        let show_welcome_popup = app.show_welcome_popup;
        let mut search_query = app.search_query.clone();
        let runtime = app.runtime.clone();

        // New state for tabs, subscription, library, popup
        let current_tab = app.current_tab;
        let subscription = app.subscription.clone();
        let selected_game_popup = app.selected_game_popup.clone();

        // Server/region state
        let servers = app.servers.clone();
        let selected_server_index = app.selected_server_index;
        let auto_server_selection = app.auto_server_selection;
        let ping_testing = app.ping_testing;
        let show_settings_modal = app.show_settings_modal;

        // Resolution notification data (extracted for use in closure)
        let resolution_notif = self.resolution_notification.as_ref().map(|n| {
            (
                n.old_resolution.clone(),
                n.new_resolution.clone(),
                n.direction,
                n.alpha(),
            )
        });

        // Wheel notification data (extracted for use in closure)
        let wheel_notif = self
            .wheel_notification
            .as_ref()
            .map(|n| (n.wheel_count, n.alpha()));

        // Queue times state
        let mut queue_servers = app.queue_servers.clone();
        let queue_loading = app.queue_loading;
        let queue_sort_mode = app.queue_sort_mode;
        let queue_region_filter = app.queue_region_filter.clone();
        let show_server_selection = app.show_server_selection;
        let selected_queue_server = app.selected_queue_server.clone();
        let pending_server_selection_game = app.pending_server_selection_game.clone();

        // Ads state (free tier)
        let ads_required = app.ads_required;
        let ads_remaining_secs = app.ads_remaining_secs;
        let ads_total_secs = app.ads_total_secs;

        // ZNow state
        let znow_apps = app.filtered_znow_apps().into_iter().cloned().collect::<Vec<_>>();
        let znow_loading = app.znow_loading;

        // File transfers (for drag & drop upload notifications)
        let file_transfers = app.file_transfers.clone();

        // Get games based on current tab
        // Optimization: Home tab uses game_sections, not games_list - avoid cloning games
        let games_list: Vec<(usize, crate::app::GameInfo)> = match current_tab {
            GamesTab::Home => {
                // Home tab renders from game_sections, return empty to avoid clone
                Vec::new()
            }
            GamesTab::AllGames | GamesTab::MyLibrary => {
                // Only clone filtered games for tabs that need them
                let query = app.search_query.to_lowercase();
                let source = if current_tab == GamesTab::MyLibrary {
                    &app.library_games
                } else {
                    &app.games
                };
                source
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| query.is_empty() || g.title.to_lowercase().contains(&query))
                    .map(|(i, g)| (i, g.clone()))
                    .collect()
            }
            GamesTab::QueueTimes => Vec::new(),
            GamesTab::ZNow => Vec::new(), // ZNow tab uses its own app list
        };

        // Get game sections for Home tab - only clone if on Home tab
        let game_sections = if current_tab == GamesTab::Home {
            app.game_sections.clone()
        } else {
            Vec::new()
        };

        // Clone texture map for rendering (avoid borrow issues)
        let game_textures = self.game_textures.clone();
        let mut new_textures: Vec<(String, egui::TextureHandle)> = Vec::new();

        let full_output;
        {
            profile_scope!("egui_run");
            full_output = self.egui_ctx.run_ui(raw_input, |ctx| {
                // Custom styling
                let mut style = (*ctx.global_style()).clone();
                style.visuals.window_fill = egui::Color32::from_rgb(20, 20, 30);
                style.visuals.panel_fill = egui::Color32::from_rgb(25, 25, 35);
                style.visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(35, 35, 50);
                style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(45, 45, 65);
                style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(60, 60, 90);
                style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(80, 180, 80);
                style.visuals.selection.bg_fill = egui::Color32::from_rgb(60, 120, 60);
                ctx.set_global_style(style);

                match app_state {
                    AppState::Login => {
                        // Reduce idle CPU usage on login screen
                        ctx.request_repaint_after(Duration::from_millis(100));

                        render_login_screen(
                            ctx,
                            &login_providers,
                            selected_provider_index,
                            &status_message,
                            is_loading,
                            login_url.as_deref(),
                            &mut actions,
                        );

                        // Show welcome popup for first-time users
                        if show_welcome_popup {
                            render_welcome_popup(ctx, &mut actions);
                        }
                    }
                    AppState::Games => {
                        // Update image cache for async loading
                        image_cache::update_cache();

                        // === UI Optimization: Reduce idle repaints ===
                        // When in Games view with no user interaction, we only need to repaint
                        // occasionally to check for newly loaded images. This reduces CPU from
                        // 100% to ~5% when idle in the game library.
                        // Note: User interactions (mouse, keyboard) will trigger immediate repaints
                        // via the winit event system, so responsiveness is not affected.
                        ctx.request_repaint_after(Duration::from_millis(100));

                        self.render_games_screen(
                            ctx,
                            &games_list,
                            &game_sections,
                            &mut search_query,
                            &status_message,
                            show_settings,
                            &settings,
                            &runtime,
                            &game_textures,
                            &mut new_textures,
                            current_tab,
                            subscription.as_ref(),
                            selected_game_popup.as_ref(),
                            &servers,
                            selected_server_index,
                            auto_server_selection,
                            ping_testing,
                            show_settings_modal,
                            app.show_session_conflict,
                            app.show_av1_warning,
                            app.show_alliance_warning,
                            crate::auth::get_selected_provider()
                                .login_provider_display_name
                                .as_str(),
                            &app.active_sessions,
                            app.pending_game_launch.as_ref(),
                            &mut queue_servers,
                            queue_loading,
                            queue_sort_mode,
                            &queue_region_filter,
                            show_server_selection,
                            &selected_queue_server,
                            pending_server_selection_game.as_ref(),
                            &znow_apps,
                            znow_loading,
                            &mut actions,
                        );
                    }
                    AppState::Session => {
                        // Session screen shows loading spinner, update at 30fps for smooth animation
                        ctx.request_repaint_after(Duration::from_millis(33));

                        // Show ads screen if ads are required (free tier)
                        if ads_required {
                            render_ads_required_screen(
                                ctx,
                                &selected_game,
                                ads_remaining_secs,
                                ads_total_secs,
                                &mut actions,
                            );
                        } else {
                            render_session_screen(
                                ctx,
                                &selected_game,
                                &status_message,
                                &error_message,
                                &mut actions,
                            );
                        }
                    }
                    AppState::Streaming => {
                        // Render stats overlay
                        if show_stats && stats_visible {
                            render_stats_panel(ctx, &stats, stats_position);
                        }

                        // Render resolution change notification
                        if let Some((old_res, new_res, direction, alpha)) = &resolution_notif {
                            render_resolution_notification(
                                ctx, old_res, new_res, *direction, *alpha,
                            );
                        }

                        // Render racing wheel connection notification
                        if let Some((wheel_count, alpha)) = wheel_notif {
                            render_wheel_notification(ctx, wheel_count, alpha);
                        }

                        // Render file transfer notifications
                        if !file_transfers.is_empty() {
                            render_file_transfer_notifications(ctx, &file_transfers, &mut actions);
                        }

                        // Small overlay hint
                        egui::Area::new(egui::Id::new("stream_hint"))
                            .anchor(egui::Align2::CENTER_TOP, [0.0, 10.0])
                            .interactable(false)
                            .show(ctx, |ui| {
                                ui.label(
                                    egui::RichText::new(
                                        "Ctrl+Shift+Q to stop • F3 stats • F11 fullscreen",
                                    )
                                    .color(egui::Color32::from_rgba_unmultiplied(
                                        255, 255, 255, 100,
                                    ))
                                    .size(12.0),
                                );
                            });
                    }
                }
            });
        } // end profile_scope!("egui_run")

        // Check if search query changed
        if search_query != app.search_query {
            // If user starts typing a search and is on Home tab, switch to All Games tab
            // so they can see the filtered results
            if !search_query.is_empty() && current_tab == GamesTab::Home {
                actions.push(UiAction::SwitchTab(GamesTab::AllGames));
            }
            actions.push(UiAction::UpdateSearch(search_query));
        }

        // Apply newly loaded textures to the cache
        for (url, texture) in new_textures {
            self.game_textures.insert(url, texture);
        }

        self.egui_state
            .handle_platform_output(&self.window, full_output.platform_output);

        let clipped_primitives = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);

        // Update egui textures
        for (id, image_delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, image_delta);
        }

        // Render egui
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.size.width, self.size.height],
            pixels_per_point: self.window.scale_factor() as f32,
        };

        self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &clipped_primitives,
            &screen_descriptor,
        );

        {
            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Egui Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });

            // forget_lifetime is safe here as render_pass is dropped before encoder.finish()
            let mut render_pass = render_pass.forget_lifetime();
            self.egui_renderer
                .render(&mut render_pass, &clipped_primitives, &screen_descriptor);
        }

        // Free egui textures
        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        {
            profile_scope!("gpu_submit");
            self.queue.submit(std::iter::once(encoder.finish()));
        }
        {
            profile_scope!("present");
            output.present();
        }

        // Return repaint delay based on app state for idle throttling
        // This is set by request_repaint_after() calls in the UI code
        let repaint_delay = match app.state {
            AppState::Login | AppState::Games => Some(Duration::from_millis(100)),
            AppState::Session => Some(Duration::from_millis(33)), // 30fps for spinner
            AppState::Streaming => None,                          // No delay when streaming
        };

        Ok((actions, repaint_delay))
    }

    // render_login_screen moved to screens/login.rs

    fn render_games_screen(
        &self,
        ctx: &egui::Context,
        games: &[(usize, crate::app::GameInfo)],
        game_sections: &[crate::app::GameSection],
        search_query: &mut String,
        _status_message: &str,
        _show_settings: bool,
        settings: &crate::app::Settings,
        _runtime: &tokio::runtime::Handle,
        game_textures: &HashMap<String, egui::TextureHandle>,
        new_textures: &mut Vec<(String, egui::TextureHandle)>,
        current_tab: GamesTab,
        subscription: Option<&crate::app::SubscriptionInfo>,
        selected_game_popup: Option<&crate::app::GameInfo>,
        servers: &[crate::app::ServerInfo],
        selected_server_index: usize,
        auto_server_selection: bool,
        ping_testing: bool,
        show_settings_modal: bool,
        show_session_conflict: bool,
        show_av1_warning: bool,
        show_alliance_warning: bool,
        alliance_provider_name: &str,
        active_sessions: &[ActiveSessionInfo],
        pending_game_launch: Option<&GameInfo>,
        queue_servers: &mut Vec<crate::api::QueueServerInfo>,
        queue_loading: bool,
        queue_sort_mode: crate::app::QueueSortMode,
        queue_region_filter: &crate::app::QueueRegionFilter,
        show_server_selection: bool,
        selected_queue_server: &Option<String>,
        pending_server_selection_game: Option<&GameInfo>,
        znow_apps: &[crate::app::ZNowApp],
        znow_loading: bool,
        actions: &mut Vec<UiAction>,
    ) {
        // Top bar with tabs, search, and logout - subscription info moved to bottom
        egui::Panel::top("top_bar")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(22, 22, 30))
                    .inner_margin(egui::Margin {
                        left: 0,
                        right: 0,
                        top: 10,
                        bottom: 10,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(15.0);

                    // Logo
                    ui.label(
                        egui::RichText::new("OpenNOW")
                            .size(24.0)
                            .color(egui::Color32::from_rgb(118, 185, 0))
                            .strong(),
                    );

                    ui.add_space(20.0);

                    // Tab buttons - solid style like login button
                    let home_selected = current_tab == GamesTab::Home;
                    let all_games_selected = current_tab == GamesTab::AllGames;
                    let library_selected = current_tab == GamesTab::MyLibrary;
                    let queue_times_selected = current_tab == GamesTab::QueueTimes;

                    // Home tab button
                    let home_btn = egui::Button::new(
                        egui::RichText::new("Home")
                            .size(13.0)
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .fill(if home_selected {
                        egui::Color32::from_rgb(118, 185, 0)
                    } else {
                        egui::Color32::from_rgb(50, 50, 65)
                    })
                    .corner_radius(6.0);

                    if ui.add_sized([70.0, 32.0], home_btn).clicked() && !home_selected {
                        actions.push(UiAction::SwitchTab(GamesTab::Home));
                    }

                    ui.add_space(8.0);

                    let all_games_btn = egui::Button::new(
                        egui::RichText::new("All Games")
                            .size(13.0)
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .fill(if all_games_selected {
                        egui::Color32::from_rgb(118, 185, 0)
                    } else {
                        egui::Color32::from_rgb(50, 50, 65)
                    })
                    .corner_radius(6.0);

                    if ui.add_sized([90.0, 32.0], all_games_btn).clicked() && !all_games_selected {
                        actions.push(UiAction::SwitchTab(GamesTab::AllGames));
                    }

                    ui.add_space(8.0);

                    let library_btn = egui::Button::new(
                        egui::RichText::new("My Library")
                            .size(13.0)
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .fill(if library_selected {
                        egui::Color32::from_rgb(118, 185, 0)
                    } else {
                        egui::Color32::from_rgb(50, 50, 65)
                    })
                    .corner_radius(6.0);

                    if ui.add_sized([90.0, 32.0], library_btn).clicked() && !library_selected {
                        actions.push(UiAction::SwitchTab(GamesTab::MyLibrary));
                    }

                    ui.add_space(8.0);

                    // Queue Times tab button (for free tier users)
                    let queue_times_btn = egui::Button::new(
                        egui::RichText::new("🕐 Queue Times")
                            .size(13.0)
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .fill(if queue_times_selected {
                        egui::Color32::from_rgb(118, 185, 0)
                    } else {
                        egui::Color32::from_rgb(50, 50, 65)
                    })
                    .corner_radius(6.0);

                    if ui
                        .add_sized([120.0, 32.0], queue_times_btn)
                        .on_hover_text("View queue times for servers")
                        .clicked()
                        && !queue_times_selected
                    {
                        actions.push(UiAction::SwitchTab(GamesTab::QueueTimes));
                    }

                    ui.add_space(8.0);

                    // ZNow tab button
                    let znow_selected = current_tab == GamesTab::ZNow;
                    let znow_btn = egui::Button::new(
                        egui::RichText::new("ZNow")
                            .size(13.0)
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .fill(if znow_selected {
                        egui::Color32::from_rgb(118, 185, 0)
                    } else {
                        egui::Color32::from_rgb(50, 50, 65)
                    })
                    .corner_radius(6.0);

                    if ui
                        .add_sized([80.0, 32.0], znow_btn)
                        .on_hover_text("Launch portable apps in GFN")
                        .clicked()
                        && !znow_selected
                    {
                        actions.push(UiAction::SwitchTab(GamesTab::ZNow));
                    }

                    ui.add_space(20.0);

                    // Search box in the middle
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(35, 35, 45))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin {
                            left: 10,
                            right: 10,
                            top: 6,
                            bottom: 6,
                        })
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(60, 60, 75)))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("🔍")
                                        .size(12.0)
                                        .color(egui::Color32::from_rgb(120, 120, 140)),
                                );
                                ui.add_space(6.0);
                                let search = egui::TextEdit::singleline(search_query)
                                    .hint_text("Search games...")
                                    .desired_width(200.0)
                                    .frame(false)
                                    .text_color(egui::Color32::WHITE);
                                ui.add(search);
                            });
                        });

                    // Right side content
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(15.0);

                        // Logout button - solid style
                        let logout_btn = egui::Button::new(
                            egui::RichText::new("Logout")
                                .size(13.0)
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(50, 50, 65))
                        .corner_radius(6.0);

                        if ui.add_sized([80.0, 32.0], logout_btn).clicked() {
                            actions.push(UiAction::Logout);
                        }

                        ui.add_space(10.0);

                        // Settings button - between hours and logout
                        let settings_btn =
                            egui::Button::new(egui::RichText::new("⚙").size(16.0).color(
                                if show_settings_modal {
                                    egui::Color32::from_rgb(118, 185, 0)
                                } else {
                                    egui::Color32::WHITE
                                },
                            ))
                            .fill(if show_settings_modal {
                                egui::Color32::from_rgb(50, 70, 50)
                            } else {
                                egui::Color32::from_rgb(50, 50, 65)
                            })
                            .corner_radius(6.0);

                        if ui.add_sized([36.0, 32.0], settings_btn).clicked() {
                            actions.push(UiAction::ToggleSettingsModal);
                        }
                    });
                });
            });

        // Bottom bar with subscription stats
        egui::Panel::bottom("bottom_bar")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(22, 22, 30))
                    .inner_margin(egui::Margin {
                        left: 15,
                        right: 15,
                        top: 8,
                        bottom: 8,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(sub) = subscription {
                        // Membership tier badge
                        let (tier_bg, tier_fg) = match sub.membership_tier.as_str() {
                            // Ultimate: Gold/Bronze theme
                            "ULTIMATE" => (
                                egui::Color32::from_rgb(80, 60, 10),
                                egui::Color32::from_rgb(255, 215, 0),
                            ),
                            // Priority/Performance: Brown theme
                            "PERFORMANCE" | "PRIORITY" => (
                                egui::Color32::from_rgb(70, 40, 20),
                                egui::Color32::from_rgb(205, 175, 149),
                            ),
                            // Free: Gray theme
                            _ => (
                                egui::Color32::from_rgb(45, 45, 45),
                                egui::Color32::from_rgb(180, 180, 180),
                            ),
                        };

                        egui::Frame::new()
                            .fill(tier_bg)
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin {
                                left: 8,
                                right: 8,
                                top: 4,
                                bottom: 4,
                            })
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(&sub.membership_tier)
                                        .size(11.0)
                                        .color(tier_fg)
                                        .strong(),
                                );
                            });

                        // Alliance badge (if using an Alliance partner)
                        if crate::auth::get_selected_provider().is_alliance_partner() {
                            ui.add_space(8.0);
                            egui::Frame::new()
                                .fill(egui::Color32::from_rgb(30, 80, 130))
                                .corner_radius(4.0)
                                .inner_margin(egui::Margin {
                                    left: 8,
                                    right: 8,
                                    top: 4,
                                    bottom: 4,
                                })
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new("ALLIANCE")
                                            .size(11.0)
                                            .color(egui::Color32::from_rgb(100, 180, 255))
                                            .strong(),
                                    );
                                });
                        }

                        ui.add_space(20.0);

                        // Hours icon and remaining
                        ui.label(
                            egui::RichText::new("⏱")
                                .size(14.0)
                                .color(egui::Color32::GRAY),
                        );
                        ui.add_space(5.0);

                        // Show ∞ for unlimited subscriptions, otherwise show hours
                        if sub.is_unlimited {
                            ui.label(
                                egui::RichText::new("∞")
                                    .size(15.0)
                                    .color(egui::Color32::from_rgb(118, 185, 0))
                                    .strong(),
                            );
                        } else {
                            let hours_color = if sub.remaining_hours > 5.0 {
                                egui::Color32::from_rgb(118, 185, 0)
                            } else if sub.remaining_hours > 1.0 {
                                egui::Color32::from_rgb(255, 200, 50)
                            } else {
                                egui::Color32::from_rgb(255, 80, 80)
                            };

                            ui.label(
                                egui::RichText::new(format!("{:.1}h", sub.remaining_hours))
                                    .size(13.0)
                                    .color(hours_color)
                                    .strong(),
                            );
                            ui.label(
                                egui::RichText::new(format!(" / {:.0}h", sub.total_hours))
                                    .size(12.0)
                                    .color(egui::Color32::GRAY),
                            );
                        }

                        ui.add_space(20.0);

                        // Storage icon and space (if available)
                        if sub.has_persistent_storage {
                            if let Some(storage_gb) = sub.storage_size_gb {
                                ui.label(
                                    egui::RichText::new("💾")
                                        .size(14.0)
                                        .color(egui::Color32::GRAY),
                                );
                                ui.add_space(5.0);
                                ui.label(
                                    egui::RichText::new(format!("{} GB", storage_gb))
                                        .size(13.0)
                                        .color(egui::Color32::from_rgb(100, 180, 255)),
                                );
                            }
                        }
                    } else {
                        ui.label(
                            egui::RichText::new("Loading subscription info...")
                                .size(12.0)
                                .color(egui::Color32::GRAY),
                        );
                    }

                    // Right side: server info
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Show selected server
                        if auto_server_selection {
                            let best_server = servers
                                .iter()
                                .filter(|s| {
                                    s.status == crate::app::ServerStatus::Online
                                        && s.ping_ms.is_some()
                                })
                                .min_by_key(|s| s.ping_ms.unwrap_or(9999));

                            if let Some(server) = best_server {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "🌐 Auto: {} ({}ms)",
                                        server.name,
                                        server.ping_ms.unwrap_or(0)
                                    ))
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(118, 185, 0)),
                                );
                            } else if ping_testing {
                                ui.label(
                                    egui::RichText::new("🌐 Testing servers...")
                                        .size(12.0)
                                        .color(egui::Color32::GRAY),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new("🌐 Auto (waiting for ping)")
                                        .size(12.0)
                                        .color(egui::Color32::GRAY),
                                );
                            }
                        } else if let Some(server) = servers.get(selected_server_index) {
                            let ping_text = server
                                .ping_ms
                                .map(|p| format!(" ({}ms)", p))
                                .unwrap_or_default();
                            ui.label(
                                egui::RichText::new(format!("🌐 {}{}", server.name, ping_text))
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(100, 180, 255)),
                            );
                        }
                    });
                });
            });

        // Main content area
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(15.0);

            // Render based on current tab
            match current_tab {
                GamesTab::Home => {
                    // Home tab - horizontal scrolling sections
                    if game_sections.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(100.0);
                            ui.label(
                                egui::RichText::new("Loading sections...")
                                    .size(14.0)
                                    .color(egui::Color32::from_rgb(120, 120, 120))
                            );
                        });
                    } else {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.add_space(5.0);

                                for section in game_sections {
                                    // Section header
                                    ui.horizontal(|ui| {
                                        ui.add_space(10.0);
                                        ui.label(
                                            egui::RichText::new(&section.title)
                                                .size(18.0)
                                                .strong()
                                                .color(egui::Color32::WHITE)
                                        );
                                    });

                                    ui.add_space(10.0);

                                    // Horizontal scroll of game cards
                                    ui.horizontal(|ui| {
                                        ui.add_space(10.0);
                                        egui::ScrollArea::horizontal()
                                            .id_salt(&section.title)
                                            .auto_shrink([false, false])
                                            .show(ui, |ui| {
                                                ui.horizontal(|ui| {
                                                    for (idx, game) in section.games.iter().enumerate() {
                                                        Self::render_game_card(ui, ctx, idx, game, _runtime, game_textures, new_textures, actions);
                                                        ui.add_space(12.0);
                                                    }
                                                });
                                            });
                                    });

                                    ui.add_space(20.0);
                                }
                            });
                    }
                }
                GamesTab::QueueTimes => {
                    // Check if we have any ping data (to know if ping test is still running)
                    let has_ping_data = queue_servers.iter().any(|s| s.ping_ms.is_some());

                    // Get recommended server (only if we have ping data)
                    let recommended_server = if has_ping_data {
                        crate::api::get_auto_selected_server(queue_servers)
                    } else {
                        None
                    };

                    // Aggregated location data (grouped by display_name within a region)
                    #[derive(Debug, Clone)]
                    struct LocationInfo {
                        display_name: String,
                        avg_queue_position: i32,
                        avg_eta_seconds: Option<i64>,
                        best_ping_ms: Option<u32>,
                        server_count: usize,
                        has_5080: bool,
                        has_4080: bool,
                    }

                    // Apply region filter to servers
                    let filtered_servers: Vec<&crate::api::QueueServerInfo> = queue_servers.iter()
                        .filter(|s| match queue_region_filter {
                            crate::app::QueueRegionFilter::All => true,
                            crate::app::QueueRegionFilter::Region(ref region) => &s.region == region,
                        })
                        .collect();

                    // Group servers by region, then by location (display_name)
                    let mut regions: std::collections::HashMap<String, std::collections::HashMap<String, Vec<&crate::api::QueueServerInfo>>> = std::collections::HashMap::new();

                    for server in filtered_servers.iter() {
                        let region_entry = regions.entry(server.region.clone()).or_insert_with(std::collections::HashMap::new);
                        let location_entry = region_entry.entry(server.display_name.clone()).or_insert_with(Vec::new);
                        location_entry.push(*server);
                    }

                    // Build aggregated location info for each region
                    let mut region_locations: std::collections::HashMap<String, Vec<LocationInfo>> = std::collections::HashMap::new();

                    for (region, locations) in &regions {
                        let mut location_list: Vec<LocationInfo> = Vec::new();

                        for (display_name, servers) in locations {
                            let count = servers.len();
                            let avg_queue = if count > 0 {
                                let sum: i64 = servers.iter().map(|s| s.queue_position as i64).sum();
                                let avg_i64 = sum / count as i64;
                                avg_i64.clamp(i32::MIN as i64, i32::MAX as i64) as i32
                            } else {
                                0
                            };
                            let avg_eta = {
                                let eta_sum: i64 = servers.iter().filter_map(|s| s.eta_seconds).sum();
                                let eta_count = servers.iter().filter(|s| s.eta_seconds.is_some()).count();
                                if eta_count > 0 { Some(eta_sum / eta_count as i64) } else { None }
                            };
                            let best_ping = servers.iter().filter_map(|s| s.ping_ms).min();
                            let has_5080 = servers.iter().any(|s| s.is_5080_server);
                            let has_4080 = servers.iter().any(|s| s.is_4080_server);

                            location_list.push(LocationInfo {
                                display_name: display_name.clone(),
                                avg_queue_position: avg_queue,
                                avg_eta_seconds: avg_eta,
                                best_ping_ms: best_ping,
                                server_count: count,
                                has_5080,
                                has_4080,
                            });
                        }

                        // Sort locations based on the selected sort mode
                        // All sorts use display_name as a tiebreaker for stable ordering
                        match queue_sort_mode {
                            crate::app::QueueSortMode::BestValue => {
                                // Sort by a combined score (lower is better)
                                location_list.sort_by(|a, b| {
                                    let score_a = a.best_ping_ms.unwrap_or(500) as f64
                                        + (a.avg_eta_seconds.unwrap_or(0) as f64 / 60.0 * 0.5).min(100.0);
                                    let score_b = b.best_ping_ms.unwrap_or(500) as f64
                                        + (b.avg_eta_seconds.unwrap_or(0) as f64 / 60.0 * 0.5).min(100.0);
                                    score_a.partial_cmp(&score_b)
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                        .then_with(|| a.display_name.cmp(&b.display_name))
                                });
                            }
                            crate::app::QueueSortMode::QueueTime => {
                                location_list.sort_by(|a, b| {
                                    let eta_a = a.avg_eta_seconds.unwrap_or(i64::MAX);
                                    let eta_b = b.avg_eta_seconds.unwrap_or(i64::MAX);
                                    eta_a.cmp(&eta_b)
                                        .then_with(|| a.display_name.cmp(&b.display_name))
                                });
                            }
                            crate::app::QueueSortMode::Ping => {
                                location_list.sort_by(|a, b| {
                                    let ping_a = a.best_ping_ms.unwrap_or(u32::MAX);
                                    let ping_b = b.best_ping_ms.unwrap_or(u32::MAX);
                                    ping_a.cmp(&ping_b)
                                        .then_with(|| a.display_name.cmp(&b.display_name))
                                });
                            }
                            crate::app::QueueSortMode::Alphabetical => {
                                location_list.sort_by(|a, b| a.display_name.cmp(&b.display_name));
                            }
                        }
                        region_locations.insert(region.clone(), location_list);
                    }

                    // Sort regions by priority
                    let mut region_keys: Vec<_> = regions.keys().cloned().collect();
                    region_keys.sort_by(|a, b| {
                        let order = |r: &str| match r {
                            "US" => 0, "EU" => 1, "CA" => 2, "JP" => 3, "KR" => 4, "THAI" => 5, "MY" => 6,
                            "SG" => 7, "TW" => 8, "AU" => 9, "LATAM" => 10, "TR" => 11, "SA" => 12,
                            _ => 100
                        };
                        order(a).cmp(&order(b)).then(a.cmp(b))
                    });

                    // Header with title and controls
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);

                        // Title
                        ui.label(
                            egui::RichText::new("Queue Times")
                                .size(22.0)
                                .strong()
                                .color(egui::Color32::WHITE)
                        );

                        ui.add_space(12.0);

                        // Region count badge
                        if queue_loading {
                            ui.spinner();
                        } else {
                            egui::Frame::new()
                                .fill(egui::Color32::from_rgb(40, 40, 55))
                                .corner_radius(12.0)
                                .inner_margin(egui::Margin { left: 10, right: 10, top: 4, bottom: 4 })
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(format!("{} regions", region_keys.len()))
                                            .size(12.0)
                                            .color(egui::Color32::from_rgb(150, 150, 150))
                                    );
                                });
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(16.0);

                            // Refresh button
                            let refresh_btn = egui::Button::new(
                                egui::RichText::new("↻ Refresh")
                                    .size(12.0)
                                    .color(egui::Color32::WHITE)
                            )
                            .fill(egui::Color32::from_rgb(50, 50, 65))
                            .corner_radius(6.0);

                            if ui.add(refresh_btn).clicked() {
                                actions.push(UiAction::RefreshQueueTimes);
                            }
                        });
                    });

                    ui.add_space(16.0);

                    if queue_servers.is_empty() && !queue_loading {
                        // Empty state
                        ui.vertical_centered(|ui| {
                            ui.add_space(100.0);
                            ui.label(
                                egui::RichText::new("📊")
                                    .size(48.0)
                            );
                            ui.add_space(16.0);
                            ui.label(
                                egui::RichText::new("No Queue Data Available")
                                    .size(18.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(180, 180, 180))
                            );
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("Click Refresh to load queue times")
                                    .size(14.0)
                                    .color(egui::Color32::from_rgb(120, 120, 120))
                            );
                        });
                    } else {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.add_space(8.0);

                                // Recommended server card
                                ui.horizontal(|ui| {
                                    ui.add_space(16.0);

                                    if !has_ping_data {
                                        // Ping test still running - show loading state
                                        egui::Frame::new()
                                            .fill(egui::Color32::from_rgb(35, 35, 50))
                                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 80, 100)))
                                            .corner_radius(12.0)
                                            .inner_margin(egui::Margin::same(16))
                                            .show(ui, |ui| {
                                                ui.set_width(ui.available_width() - 32.0);
                                                ui.horizontal(|ui| {
                                                    ui.spinner();
                                                    ui.add_space(12.0);
                                                    ui.vertical(|ui| {
                                                        ui.label(
                                                            egui::RichText::new("⭐ RECOMMENDED")
                                                                .size(11.0)
                                                                .strong()
                                                                .color(egui::Color32::from_rgb(100, 100, 120))
                                                        );
                                                        ui.add_space(4.0);
                                                        ui.label(
                                                            egui::RichText::new("Waiting for ping test to finish...")
                                                                .size(14.0)
                                                                .color(egui::Color32::from_rgb(140, 140, 160))
                                                        );
                                                    });
                                                });
                                            });
                                    } else if let Some(best) = recommended_server {
                                        egui::Frame::new()
                                            .fill(egui::Color32::from_rgb(25, 45, 25))
                                            .stroke(egui::Stroke::new(1.5, egui::Color32::from_rgb(118, 185, 0)))
                                            .corner_radius(12.0)
                                            .inner_margin(egui::Margin::same(16))
                                            .show(ui, |ui| {
                                                ui.set_width(ui.available_width() - 32.0);
                                                ui.horizontal(|ui| {
                                                    // Star icon and recommended label
                                                    ui.label(
                                                        egui::RichText::new("⭐ RECOMMENDED")
                                                            .size(11.0)
                                                            .strong()
                                                            .color(egui::Color32::from_rgb(118, 185, 0))
                                                    );

                                                    ui.add_space(12.0);

                                                    // Server info
                                                    ui.label(
                                                        egui::RichText::new(&best.display_name)
                                                            .size(16.0)
                                                            .strong()
                                                            .color(egui::Color32::WHITE)
                                                    );

                                                    ui.add_space(8.0);

                                                    // GPU badge
                                                    let gpu_text = if best.is_5080_server {
                                                        "5080"
                                                    } else if best.is_4080_server {
                                                        "4080"
                                                    } else {
                                                        "Unknown"
                                                    };
                                                    egui::Frame::new()
                                                        .fill(egui::Color32::from_rgb(118, 185, 0).gamma_multiply(0.3))
                                                        .corner_radius(4.0)
                                                        .inner_margin(egui::Margin { left: 6, right: 6, top: 2, bottom: 2 })
                                                        .show(ui, |ui| {
                                                            ui.label(
                                                                egui::RichText::new(gpu_text)
                                                                    .size(10.0)
                                                                    .strong()
                                                                    .color(egui::Color32::from_rgb(118, 185, 0))
                                                            );
                                                        });

                                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                        // Wait time
                                                        let eta_text = crate::api::format_queue_eta(best.eta_seconds);
                                                        ui.label(
                                                            egui::RichText::new(format!("~{}", eta_text))
                                                                .size(14.0)
                                                                .color(egui::Color32::from_rgb(118, 185, 0))
                                                        );

                                                        ui.add_space(12.0);

                                                        // Queue position in box
                                                        let position_color = if best.queue_position <= 0 {
                                                            egui::Color32::from_rgb(118, 185, 0)
                                                        } else if best.queue_position <= 5 {
                                                            // Low queue: green
                                                            egui::Color32::from_rgb(118, 185, 0)
                                                        } else if best.queue_position <= 15 {
                                                            // Medium queue: orange
                                                            egui::Color32::from_rgb(255, 165, 0)
                                                        } else {
                                                            // High queue: red
                                                            egui::Color32::from_rgb(230, 80, 80)
                                                        };
                                                        let pos_text = if best.queue_position <= 0 {
                                                            "0".to_string()
                                                        } else {
                                                            format!("{}", best.queue_position)
                                                        };
                                                        egui::Frame::new()
                                                            .fill(position_color.gamma_multiply(0.2))
                                                            .corner_radius(4.0)
                                                            .inner_margin(egui::Margin { left: 8, right: 8, top: 3, bottom: 3 })
                                                            .show(ui, |ui| {
                                                                ui.label(
                                                                    egui::RichText::new(pos_text)
                                                                        .size(13.0)
                                                                        .strong()
                                                                        .color(position_color)
                                                                );
                                                            });

                                                        ui.add_space(12.0);

                                                        // Ping
                                                        if let Some(ping) = best.ping_ms {
                                                            ui.label(
                                                                egui::RichText::new(format!("{}ms", ping))
                                                                    .size(13.0)
                                                                    .color(egui::Color32::from_rgb(140, 180, 140))
                                                            );
                                                        }
                                                    });
                                                });
                                            });
                                    }
                                });

                                ui.add_space(20.0);

                                // Region sections with locations (using CollapsingHeader)
                                for region in &region_keys {
                                    if let Some(locations) = region_locations.get(region) {
                                        let (flag, region_name) = match region.as_str() {
                                            "US" => ("🇺🇸", "United States"),
                                            "EU" => ("🇪🇺", "Europe"),
                                            "CA" => ("🇨🇦", "Canada"),
                                            "JP" => ("🇯🇵", "Japan"),
                                            "THAI" => ("🇹🇭", "Thailand"),
                                            "MY" => ("🇲🇾", "Malaysia"),
                                            "KR" => ("🇰🇷", "South Korea"),
                                            "SG" => ("🇸🇬", "Singapore"),
                                            "TW" => ("🇹🇼", "Taiwan"),
                                            "AU" => ("🇦🇺", "Australia"),
                                            "LATAM" => ("🌎", "Latin America"),
                                            "TR" => ("🇹🇷", "Turkey"),
                                            "SA" => ("🇸🇦", "Saudi Arabia"),
                                            "AF" => ("🌍", "Africa"),
                                            "RU" => ("🇷🇺", "Russia"),
                                            _ => ("🌐", region.as_str()),
                                        };

                                        // Region container with padding
                                        ui.add_space(4.0);
                                        ui.horizontal(|ui| {
                                            ui.add_space(16.0);
                                            ui.vertical(|ui| {
                                                ui.set_width(ui.available_width() - 32.0);

                                                // Use CollapsingHeader for expandable regions
                                                let header_text = format!("{} {} ({} locations)", flag, region_name, locations.len());
                                                egui::CollapsingHeader::new(
                                                    egui::RichText::new(header_text)
                                                        .size(15.0)
                                                        .strong()
                                                        .color(egui::Color32::WHITE)
                                                )
                                                .default_open(true)
                                                .show(ui, |ui| {
                                                    // Location rows within this region
                                                    for location in locations {
                                                        ui.horizontal(|ui| {
                                                            ui.add_space(20.0); // Indent under region

                                                            egui::Frame::new()
                                                                .fill(egui::Color32::from_rgb(28, 28, 38))
                                                                .corner_radius(6.0)
                                                                .inner_margin(egui::Margin { left: 12, right: 12, top: 8, bottom: 8 })
                                                                .show(ui, |ui| {
                                                                    ui.set_width(ui.available_width() - 36.0);
                                                                    ui.horizontal(|ui| {
                                                                        // Location name
                                                                        ui.allocate_ui_with_layout(
                                                                            egui::vec2(110.0, 20.0),
                                                                            egui::Layout::left_to_right(egui::Align::Center),
                                                                            |ui| {
                                                                                ui.label(
                                                                                    egui::RichText::new(&location.display_name)
                                                                                        .size(13.0)
                                                                                        .color(egui::Color32::WHITE)
                                                                                );
                                                                            }
                                                                        );

                                                                        // Server count if > 1
                                                                        if location.server_count > 1 {
                                                                            egui::Frame::new()
                                                                                .fill(egui::Color32::from_rgb(45, 45, 60))
                                                                                .corner_radius(4.0)
                                                                                .inner_margin(egui::Margin { left: 5, right: 5, top: 2, bottom: 2 })
                                                                                .show(ui, |ui| {
                                                                                    ui.label(
                                                                                        egui::RichText::new(format!("x{}", location.server_count))
                                                                                            .size(9.0)
                                                                                            .color(egui::Color32::from_rgb(120, 120, 140))
                                                                                    );
                                                                                });
                                                                            ui.add_space(4.0);
                                                                        }

                                                                        // GPU badges
                                                                        if location.has_5080 {
                                                                            let gpu_color = egui::Color32::from_rgb(118, 185, 0);
                                                                            egui::Frame::new()
                                                                                .fill(gpu_color.gamma_multiply(0.2))
                                                                                .corner_radius(4.0)
                                                                                .inner_margin(egui::Margin { left: 5, right: 5, top: 2, bottom: 2 })
                                                                                .show(ui, |ui| {
                                                                                    ui.label(
                                                                                        egui::RichText::new("5080")
                                                                                            .size(9.0)
                                                                                            .strong()
                                                                                            .color(gpu_color)
                                                                                    );
                                                                                });
                                                                            ui.add_space(4.0);
                                                                        }
                                                                        if location.has_4080 {
                                                                            let gpu_color = egui::Color32::from_rgb(100, 160, 220);
                                                                            egui::Frame::new()
                                                                                .fill(gpu_color.gamma_multiply(0.2))
                                                                                .corner_radius(4.0)
                                                                                .inner_margin(egui::Margin { left: 5, right: 5, top: 2, bottom: 2 })
                                                                                .show(ui, |ui| {
                                                                                    ui.label(
                                                                                        egui::RichText::new("4080")
                                                                                            .size(9.0)
                                                                                            .strong()
                                                                                            .color(gpu_color)
                                                                                    );
                                                                                });
                                                                        }

                                                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                                            // ETA
                                                                            let eta_text = crate::api::format_queue_eta(location.avg_eta_seconds);
                                                                            let eta_color = if location.avg_eta_seconds.unwrap_or(0) <= 0 {
                                                                                egui::Color32::from_rgb(118, 185, 0)
                                                                            } else if location.avg_eta_seconds.unwrap_or(0) < 300 {
                                                                                egui::Color32::from_rgb(255, 200, 50)
                                                                            } else {
                                                                                egui::Color32::from_rgb(150, 150, 150)
                                                                            };

                                                                            ui.label(
                                                                                egui::RichText::new(format!("~{}", eta_text))
                                                                                    .size(12.0)
                                                                                    .color(eta_color)
                                                                            );

                                                                            ui.add_space(12.0);

                                                                            // Queue position
                                                                            let position_color = if location.avg_queue_position <= 0 {
                                                                                egui::Color32::from_rgb(118, 185, 0)
                                                                            } else if location.avg_queue_position < 20 {
                                                                                egui::Color32::from_rgb(255, 200, 50)
                                                                            } else if location.avg_queue_position < 100 {
                                                                                egui::Color32::from_rgb(255, 150, 80)
                                                                            } else {
                                                                                egui::Color32::from_rgb(255, 100, 100)
                                                                            };

                                                                            egui::Frame::new()
                                                                                .fill(position_color.gamma_multiply(0.2))
                                                                                .corner_radius(4.0)
                                                                                .inner_margin(egui::Margin { left: 8, right: 8, top: 3, bottom: 3 })
                                                                                .show(ui, |ui| {
                                                                                    ui.label(
                                                                                        egui::RichText::new(format!("{}", location.avg_queue_position))
                                                                                            .size(12.0)
                                                                                            .strong()
                                                                                            .color(position_color)
                                                                                    );
                                                                                });

                                                                            ui.add_space(12.0);

                                                                            // Ping
                                                                            if let Some(ping) = location.best_ping_ms {
                                                                                let ping_color = if ping < 50 {
                                                                                    egui::Color32::from_rgb(118, 185, 0)
                                                                                } else if ping < 100 {
                                                                                    egui::Color32::from_rgb(255, 200, 50)
                                                                                } else {
                                                                                    egui::Color32::from_rgb(255, 150, 80)
                                                                                };
                                                                                ui.label(
                                                                                    egui::RichText::new(format!("{}ms", ping))
                                                                                        .size(12.0)
                                                                                        .color(ping_color)
                                                                                );
                                                                            } else if !has_ping_data {
                                                                                ui.spinner();
                                                                            } else {
                                                                                ui.label(
                                                                                    egui::RichText::new("-")
                                                                                        .size(12.0)
                                                                                        .color(egui::Color32::from_rgb(80, 80, 100))
                                                                                );
                                                                            }
                                                                        });
                                                                    });
                                                                });
                                                        });
                                                        ui.add_space(4.0);
                                                    }
                                                });
                                            });
                                        });
                                    }
                                }

                                // Attribution footer
                                ui.add_space(16.0);
                                ui.vertical_centered(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new("Powered by")
                                                .size(11.0)
                                                .color(egui::Color32::from_rgb(80, 80, 80))
                                        );
                                        ui.add_space(4.0);
                                        if ui.add(
                                            egui::Label::new(
                                                egui::RichText::new("PrintedWaste")
                                                    .size(11.0)
                                                    .color(egui::Color32::from_rgb(118, 185, 0))
                                                    .underline()
                                            ).sense(egui::Sense::click())
                                        ).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                            if let Err(e) = open::that("https://printedwaste.com/gfn/") {
                                                warn!("Failed to open PrintedWaste link: {}", e);
                                            }
                                        }
                                    });
                                });
                                ui.add_space(20.0);
                            });
                    }
                }
                GamesTab::AllGames | GamesTab::MyLibrary => {
                    // Grid view for All Games and My Library tabs
                    let header_text = match current_tab {
                        GamesTab::AllGames => format!("All Games ({} available)", games.len()),
                        GamesTab::MyLibrary => format!("My Library ({} games)", games.len()),
                        _ => String::new(),
                    };

                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(header_text)
                                .size(20.0)
                                .strong()
                                .color(egui::Color32::WHITE)
                        );
                    });

                    ui.add_space(20.0);

                    if games.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(100.0);
                            let empty_text = match current_tab {
                                GamesTab::AllGames => "No games found",
                                GamesTab::MyLibrary => "Your library is empty.\nPurchase games from Steam, Epic, or other stores to see them here.",
                                _ => "",
                            };
                            ui.label(
                                egui::RichText::new(empty_text)
                                    .size(14.0)
                                    .color(egui::Color32::from_rgb(120, 120, 120))
                            );
                        });
                    } else {
                        // Games grid with VIRTUAL SCROLLING - only render visible rows
                        // This dramatically reduces CPU usage from rendering 648 games to ~20-30
                        let available_width = ui.available_width();
                        let card_width = 220.0;
                        let spacing = 16.0;
                        let num_columns = ((available_width + spacing) / (card_width + spacing)).floor() as usize;
                        let num_columns = num_columns.max(2).min(6);

                        // Card height including image (124px) + title area (~60px) + spacing
                        let row_height = 124.0 + 60.0 + spacing;
                        let total_games = games.len();
                        let total_rows = (total_games + num_columns - 1) / num_columns;

                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show_viewport(ui, |ui, viewport| {
                                // Calculate which rows are visible
                                let first_visible_row = (viewport.min.y / row_height).floor() as usize;
                                let last_visible_row = ((viewport.max.y / row_height).ceil() as usize).min(total_rows);

                                // Add buffer rows for smoother scrolling
                                let first_row = first_visible_row.saturating_sub(1);
                                let last_row = (last_visible_row + 1).min(total_rows);

                                // Reserve space for rows before visible area
                                if first_row > 0 {
                                    ui.allocate_space(egui::vec2(available_width, first_row as f32 * row_height));
                                }

                                ui.horizontal(|ui| {
                                    ui.add_space(10.0);
                                    ui.vertical(|ui| {
                                        // Only render visible rows
                                        for row in first_row..last_row {
                                            let start_idx = row * num_columns;
                                            let end_idx = (start_idx + num_columns).min(total_games);

                                            ui.horizontal(|ui| {
                                                ui.spacing_mut().item_spacing.x = spacing;
                                                for game_idx in start_idx..end_idx {
                                                    if let Some((idx, game)) = games.get(game_idx) {
                                                        Self::render_game_card(ui, ctx, *idx, game, _runtime, game_textures, new_textures, actions);
                                                    }
                                                }
                                            });
                                            ui.add_space(spacing);
                                        }
                                    });
                                });

                                // Reserve space for rows after visible area
                                let remaining_rows = total_rows.saturating_sub(last_row);
                                if remaining_rows > 0 {
                                    ui.allocate_space(egui::vec2(available_width, remaining_rows as f32 * row_height));
                                }
                            });
                    }
                }
                GamesTab::ZNow => {
                    // ZNow portable apps tab
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new("ZNow - Portable Apps")
                                .size(20.0)
                                .strong()
                                .color(egui::Color32::WHITE)
                        );
                    });

                    ui.add_space(15.0);

                    // Refresh button
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        let refresh_btn = egui::Button::new(
                            egui::RichText::new("Refresh Apps")
                                .size(13.0)
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(50, 50, 65))
                        .corner_radius(6.0);

                        if ui.add_sized([120.0, 32.0], refresh_btn).clicked() {
                            actions.push(UiAction::RefreshZNowApps);
                        }
                    });

                    ui.add_space(20.0);

                    // Apps grid (znow_apps extracted at function start)
                    if znow_apps.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(100.0);
                            if znow_loading {
                                ui.label(
                                    egui::RichText::new("Loading apps...")
                                        .size(14.0)
                                        .color(egui::Color32::from_rgb(120, 120, 120))
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new("No apps available.\nClick 'Refresh Apps' to load the catalog.")
                                        .size(14.0)
                                        .color(egui::Color32::from_rgb(120, 120, 120))
                                );
                            }
                        });
                    } else {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let available_width = ui.available_width() - 20.0; // padding
                                let card_width = 180.0;
                                let card_height = 160.0;
                                let spacing = 16.0;
                                let num_columns = ((available_width + spacing) / (card_width + spacing)).floor() as usize;
                                let num_columns = num_columns.max(2).min(6);

                                ui.add_space(10.0);

                                // Use Grid for proper alignment
                                egui::Grid::new("znow_apps_grid")
                                    .num_columns(num_columns)
                                    .spacing([spacing, spacing])
                                    .min_col_width(card_width)
                                    .show(ui, |ui| {
                                        for (idx, app_item) in znow_apps.iter().enumerate() {
                                            // App card with fixed height
                                            egui::Frame::new()
                                                .fill(egui::Color32::from_rgb(35, 35, 50))
                                                .corner_radius(8.0)
                                                .inner_margin(egui::Margin::same(12))
                                                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(60, 60, 75)))
                                                .show(ui, |ui| {
                                                    ui.set_width(card_width - 24.0);
                                                    ui.set_height(card_height - 24.0);

                                                    ui.vertical(|ui| {
                                                        // Icon and name row
                                                        ui.horizontal(|ui| {
                                                            // App icon placeholder
                                                            let icon_size = egui::vec2(48.0, 48.0);
                                                            let (_, rect) = ui.allocate_space(icon_size);
                                                            ui.painter().rect_filled(
                                                                rect,
                                                                8.0,
                                                                egui::Color32::from_rgb(70, 70, 90),
                                                            );
                                                            let initial = app_item.name.chars().next().unwrap_or('?').to_uppercase().to_string();
                                                            ui.painter().text(
                                                                rect.center(),
                                                                egui::Align2::CENTER_CENTER,
                                                                &initial,
                                                                egui::FontId::proportional(20.0),
                                                                egui::Color32::WHITE,
                                                            );

                                                            ui.add_space(8.0);

                                                            ui.vertical(|ui| {
                                                                // App name
                                                                ui.label(
                                                                    egui::RichText::new(&app_item.name)
                                                                        .size(13.0)
                                                                        .strong()
                                                                        .color(egui::Color32::WHITE)
                                                                );
                                                                // Category
                                                                ui.label(
                                                                    egui::RichText::new(&app_item.category)
                                                                        .size(10.0)
                                                                        .color(egui::Color32::from_rgb(120, 120, 140))
                                                                );
                                                            });
                                                        });

                                                        ui.add_space(8.0);

                                                        // Description (truncated)
                                                        let desc = if app_item.description.len() > 50 {
                                                            format!("{}...", &app_item.description[..47])
                                                        } else {
                                                            app_item.description.clone()
                                                        };
                                                        ui.label(
                                                            egui::RichText::new(desc)
                                                                .size(10.0)
                                                                .color(egui::Color32::from_rgb(150, 150, 160))
                                                        );

                                                        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                                                            // Launch button at bottom
                                                            let launch_btn = egui::Button::new(
                                                                egui::RichText::new("Launch")
                                                                    .size(12.0)
                                                                    .color(egui::Color32::WHITE),
                                                            )
                                                            .fill(egui::Color32::from_rgb(118, 185, 0))
                                                            .corner_radius(4.0);

                                                            if ui.add_sized([card_width - 48.0, 26.0], launch_btn).clicked() {
                                                                actions.push(UiAction::LaunchZNowSession(app_item.clone()));
                                                            }
                                                        });
                                                    });
                                                });

                                            // New row after num_columns items
                                            if (idx + 1) % num_columns == 0 {
                                                ui.end_row();
                                            }
                                        }
                                    });
                            });
                    }
                }
            }
        });

        // Game detail popup
        if let Some(game) = selected_game_popup {
            Self::render_game_popup(ctx, game, game_textures, subscription, actions);
        }

        // Server selection modal (for free tier users)
        if show_server_selection {
            if let Some(game) = pending_server_selection_game {
                Self::render_server_selection_modal(
                    ctx,
                    game,
                    queue_servers,
                    queue_loading,
                    selected_queue_server,
                    actions,
                );
            }
        }

        // Settings modal
        if show_settings_modal {
            render_settings_modal(
                ctx,
                settings,
                servers,
                selected_server_index,
                auto_server_selection,
                ping_testing,
                subscription,
                actions,
            );
        }

        // Session conflict dialog
        if show_session_conflict {
            render_session_conflict_dialog(ctx, active_sessions, pending_game_launch, actions);
        }

        // AV1 hardware warning dialog
        if show_av1_warning {
            render_av1_warning_dialog(ctx, actions);
        }

        // Alliance experimental warning dialog
        if show_alliance_warning {
            render_alliance_warning_dialog(ctx, alliance_provider_name, actions);
        }
    }

    // Note: render_settings_modal, render_session_conflict_dialog, render_av1_warning_dialog
    // have been moved to src/gui/screens/dialogs.rs
    // render_login_screen, render_session_screen moved to src/gui/screens/

    /// Render the game detail popup
    fn render_game_popup(
        ctx: &egui::Context,
        game: &crate::app::GameInfo,
        game_textures: &HashMap<String, egui::TextureHandle>,
        subscription: Option<&crate::app::SubscriptionInfo>,
        actions: &mut Vec<UiAction>,
    ) {
        // Check if user is free tier (show server selection modal instead of direct launch).
        // If subscription info is not available, default to treating the user as non-free
        // to avoid incorrectly restricting paid users when data hasn't loaded yet.
        let is_free_tier = subscription
            .map(|s| s.membership_tier == "FREE")
            .unwrap_or(false);
        let popup_width = 450.0;
        let popup_height = 500.0;

        // Modal overlay (darkens background)
        egui::Area::new(egui::Id::new("modal_overlay"))
            .fixed_pos([0.0, 0.0])
            .interactable(true)
            .order(egui::Order::Background) // Draw behind windows
            .show(ctx, |ui| {
                let screen_rect = ctx.input(|i| i.viewport_rect());
                ui.painter()
                    .rect_filled(screen_rect, 0.0, egui::Color32::from_black_alpha(200));
                // Close popup on background click
                if ui
                    .allocate_rect(screen_rect, egui::Sense::click())
                    .clicked()
                {
                    actions.push(UiAction::CloseGamePopup);
                }
            });

        egui::Window::new("Game Details")
            .collapsible(false)
            .resizable(false)
            .fixed_size([popup_width, popup_height])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    // Game image
                    if let Some(ref image_url) = game.image_url {
                        if let Some(texture) = game_textures.get(image_url) {
                            let image_size = egui::vec2(popup_width - 40.0, 150.0);
                            ui.add(
                                egui::Image::new(texture)
                                    .fit_to_exact_size(image_size)
                                    .corner_radius(8.0),
                            );
                        } else {
                            // Placeholder
                            let placeholder_size = egui::vec2(popup_width - 40.0, 150.0);
                            let (_, rect) = ui.allocate_space(placeholder_size);
                            ui.painter().rect_filled(
                                rect,
                                8.0,
                                egui::Color32::from_rgb(50, 50, 70),
                            );
                            let initial = game
                                .title
                                .chars()
                                .next()
                                .unwrap_or('?')
                                .to_uppercase()
                                .to_string();
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                initial,
                                egui::FontId::proportional(48.0),
                                egui::Color32::from_rgb(100, 100, 130),
                            );
                        }
                    }

                    ui.add_space(15.0);

                    // Game title
                    ui.label(
                        egui::RichText::new(&game.title)
                            .size(20.0)
                            .strong()
                            .color(egui::Color32::WHITE),
                    );

                    ui.add_space(8.0);

                    // Platform selector (if multiple variants) or store badge (single variant)
                    if game.variants.len() > 1 {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Platform:")
                                    .size(12.0)
                                    .color(egui::Color32::GRAY),
                            );

                            // Show platform buttons
                            for (idx, variant) in game.variants.iter().enumerate() {
                                let is_selected = idx == game.selected_variant_index;
                                let btn_color = if is_selected {
                                    egui::Color32::from_rgb(100, 180, 255) // Bright blue for selected
                                } else {
                                    egui::Color32::from_rgb(60, 60, 80) // Dark for unselected
                                };
                                let text_color = if is_selected {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::LIGHT_GRAY
                                };

                                let btn = egui::Button::new(
                                    egui::RichText::new(variant.store.to_uppercase())
                                        .size(11.0)
                                        .color(text_color),
                                )
                                .fill(btn_color)
                                .corner_radius(4.0)
                                .min_size(egui::vec2(60.0, 24.0));

                                if ui.add(btn).clicked() && !is_selected {
                                    actions.push(UiAction::SelectVariant(idx));
                                }
                            }
                        });
                    } else {
                        // Single store badge (existing behavior)
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Store:")
                                    .size(12.0)
                                    .color(egui::Color32::GRAY),
                            );
                            ui.label(
                                egui::RichText::new(&game.store.to_uppercase())
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(100, 180, 255))
                                    .strong(),
                            );
                        });
                    }

                    // Publisher if available
                    if let Some(ref publisher) = game.publisher {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Publisher:")
                                    .size(12.0)
                                    .color(egui::Color32::GRAY),
                            );
                            ui.label(
                                egui::RichText::new(publisher)
                                    .size(12.0)
                                    .color(egui::Color32::LIGHT_GRAY),
                            );
                        });
                    }

                    ui.add_space(8.0);

                    // GFN Status (Play Type and Membership)
                    if let Some(ref play_type) = game.play_type {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Type:")
                                    .size(12.0)
                                    .color(egui::Color32::GRAY),
                            );
                            let color = if play_type == "INSTALL_TO_PLAY" {
                                egui::Color32::from_rgb(255, 180, 50) // Orange
                            } else {
                                egui::Color32::from_rgb(100, 200, 100) // Green
                            };
                            ui.label(
                                egui::RichText::new(play_type)
                                    .size(12.0)
                                    .color(color)
                                    .strong(),
                            );
                        });
                    }

                    if let Some(ref tier) = game.membership_tier_label {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Requires:")
                                    .size(12.0)
                                    .color(egui::Color32::GRAY),
                            );
                            ui.label(
                                egui::RichText::new(tier)
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(118, 185, 0)) // Nvidia Green
                                    .strong(),
                            );
                        });
                    }

                    if let Some(ref text) = game.playability_text {
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(text)
                                    .size(11.0)
                                    .color(egui::Color32::LIGHT_GRAY),
                            )
                            .wrap(),
                        );
                    }

                    ui.add_space(20.0);

                    // Description
                    if let Some(ref desc) = game.description {
                        ui.label(
                            egui::RichText::new("About this game:")
                                .size(14.0)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .max_height(100.0)
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(desc)
                                        .size(13.0)
                                        .color(egui::Color32::LIGHT_GRAY),
                                );
                            });
                        ui.add_space(15.0);
                    } else {
                        ui.add_space(20.0);
                    }

                    // Buttons
                    ui.horizontal(|ui| {
                        // Play button (for free tier, show server selection first)
                        let play_btn = egui::Button::new(
                            egui::RichText::new("  Play Now  ").size(16.0).strong(),
                        )
                        .fill(egui::Color32::from_rgb(70, 180, 70))
                        .min_size(egui::vec2(120.0, 40.0));

                        if ui.add(play_btn).clicked() {
                            if is_free_tier {
                                // Free tier: show server selection modal
                                actions.push(UiAction::ShowServerSelection(game.clone()));
                                actions.push(UiAction::CloseGamePopup);
                            } else {
                                // Paid tier: launch directly
                                actions.push(UiAction::LaunchGameDirect(game.clone()));
                                actions.push(UiAction::CloseGamePopup);
                            }
                        }

                        ui.add_space(20.0);

                        // Close button
                        let close_btn =
                            egui::Button::new(egui::RichText::new("  Close  ").size(14.0))
                                .fill(egui::Color32::from_rgb(60, 60, 80))
                                .min_size(egui::vec2(80.0, 40.0));

                        if ui.add(close_btn).clicked() {
                            actions.push(UiAction::CloseGamePopup);
                        }
                    });
                });
            });
    }

    /// Render the server selection modal (for free tier users choosing a server before launching)
    fn render_server_selection_modal(
        ctx: &egui::Context,
        game: &crate::app::GameInfo,
        queue_servers: &[crate::api::QueueServerInfo],
        queue_loading: bool,
        selected_server: &Option<String>,
        actions: &mut Vec<UiAction>,
    ) {
        // Modal overlay
        egui::Area::new(egui::Id::new("server_selection_overlay"))
            .fixed_pos([0.0, 0.0])
            .interactable(true)
            .order(egui::Order::Background)
            .show(ctx, |ui| {
                let screen_rect = ctx.input(|i| i.viewport_rect());
                ui.painter()
                    .rect_filled(screen_rect, 0.0, egui::Color32::from_black_alpha(200));
                if ui
                    .allocate_rect(screen_rect, egui::Sense::click())
                    .clicked()
                {
                    actions.push(UiAction::CloseServerSelection);
                }
            });

        // Check if we have ping data
        let has_ping_data = queue_servers.iter().any(|s| s.ping_ms.is_some());

        // Get recommended server (best score across all servers)
        let recommended = if has_ping_data {
            crate::api::get_auto_selected_server(queue_servers)
        } else {
            None
        };

        // Group servers by region (already simple: "US", "EU", etc. from queue API) and find best server per region
        let mut region_best: std::collections::HashMap<String, &crate::api::QueueServerInfo> =
            std::collections::HashMap::new();

        for server in queue_servers {
            let entry = region_best.entry(server.region.clone()).or_insert(server);
            // Replace if this server has better score
            let current_score = crate::api::calculate_server_score(entry);
            let new_score = crate::api::calculate_server_score(server);
            if new_score < current_score {
                *entry = server;
            }
        }

        // Sort regions by priority
        let mut region_keys: Vec<_> = region_best.keys().cloned().collect();
        region_keys.sort_by(|a, b| {
            let order = |r: &str| match r {
                "US" => 0,
                "EU" => 1,
                "CA" => 2,
                "JP" => 3,
                "KR" => 4,
                "THAI" => 5,
                "MY" => 6,
                "SG" => 7,
                "TW" => 8,
                "AU" => 9,
                "LATAM" => 10,
                "TR" => 11,
                "SA" => 12,
                _ => 100,
            };
            order(a).cmp(&order(b)).then(a.cmp(b))
        });

        egui::Window::new("Choose Server")
            .collapsible(false)
            .resizable(false)
            .fixed_size([520.0, 480.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    // Game title
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Launching:")
                                .size(14.0)
                                .color(egui::Color32::GRAY),
                        );
                        ui.label(
                            egui::RichText::new(&game.title)
                                .size(14.0)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                    });

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(10.0);

                    // Auto/Recommended option
                    let auto_selected = selected_server.is_none();
                    let auto_frame_fill = if auto_selected {
                        egui::Color32::from_rgb(30, 50, 30)
                    } else {
                        egui::Color32::from_rgb(35, 35, 50)
                    };
                    let auto_stroke = if auto_selected {
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(118, 185, 0))
                    } else {
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(60, 60, 80))
                    };

                    let auto_response = egui::Frame::new()
                        .fill(auto_frame_fill)
                        .stroke(auto_stroke)
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("⭐").size(18.0));
                                ui.add_space(8.0);
                                ui.vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new("Auto (Recommended)")
                                            .size(15.0)
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    );
                                    if !has_ping_data {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            ui.add_space(8.0);
                                            ui.label(
                                                egui::RichText::new("Testing ping to servers...")
                                                    .size(12.0)
                                                    .color(egui::Color32::from_rgb(140, 140, 160)),
                                            );
                                        });
                                    } else if let Some(best) = recommended {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{} • {}ms • ~{}",
                                                best.display_name,
                                                best.ping_ms.unwrap_or(0),
                                                crate::api::format_queue_eta(best.eta_seconds)
                                            ))
                                            .size(12.0)
                                            .color(egui::Color32::from_rgb(118, 185, 0)),
                                        );
                                    }
                                });
                            });
                        })
                        .response;

                    if auto_response.interact(egui::Sense::click()).clicked() {
                        actions.push(UiAction::SelectQueueServer(None));
                    }

                    ui.add_space(12.0);

                    // Region list
                    ui.label(
                        egui::RichText::new("Or choose a region:")
                            .size(12.0)
                            .color(egui::Color32::from_rgb(120, 120, 120)),
                    );

                    ui.add_space(8.0);

                    if queue_loading && queue_servers.is_empty() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("Loading servers...")
                                    .size(13.0)
                                    .color(egui::Color32::GRAY),
                            );
                        });
                    } else {
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .show(ui, |ui| {
                                // Show regions in sorted order
                                for region in &region_keys {
                                    if let Some(best_server) = region_best.get(region) {
                                        let (flag, region_name) = match region.as_str() {
                                            "US" => ("🇺🇸", "United States"),
                                            "EU" => ("🇪🇺", "Europe"),
                                            "CA" => ("🇨🇦", "Canada"),
                                            "JP" => ("🇯🇵", "Japan"),
                                            "THAI" => ("🇹🇭", "Thailand"),
                                            "MY" => ("🇲🇾", "Malaysia"),
                                            "KR" => ("🇰🇷", "South Korea"),
                                            "SG" => ("🇸🇬", "Singapore"),
                                            "TW" => ("🇹🇼", "Taiwan"),
                                            "AU" => ("🇦🇺", "Australia"),
                                            "LATAM" => ("🌎", "Latin America"),
                                            "TR" => ("🇹🇷", "Turkey"),
                                            "SA" => ("🇸🇦", "Saudi Arabia"),
                                            "AF" => ("🌍", "Africa"),
                                            "RU" => ("🇷🇺", "Russia"),
                                            _ => ("🌐", region.as_str()),
                                        };

                                        let is_selected = selected_server.as_ref()
                                            == Some(&best_server.server_id);
                                        let frame_fill = if is_selected {
                                            egui::Color32::from_rgb(40, 50, 70)
                                        } else {
                                            egui::Color32::from_rgb(30, 30, 42)
                                        };
                                        let frame_stroke = if is_selected {
                                            egui::Stroke::new(
                                                1.5,
                                                egui::Color32::from_rgb(100, 160, 220),
                                            )
                                        } else {
                                            egui::Stroke::NONE
                                        };

                                        let server_response = egui::Frame::new()
                                            .fill(frame_fill)
                                            .stroke(frame_stroke)
                                            .corner_radius(6.0)
                                            .inner_margin(egui::Margin::symmetric(12, 10))
                                            .show(ui, |ui| {
                                                ui.set_width(ui.available_width());
                                                ui.horizontal(|ui| {
                                                    // Flag and region name
                                                    ui.label(egui::RichText::new(flag).size(18.0));
                                                    ui.add_space(8.0);
                                                    ui.allocate_ui_with_layout(
                                                        egui::vec2(130.0, 20.0),
                                                        egui::Layout::left_to_right(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            ui.label(
                                                                egui::RichText::new(region_name)
                                                                    .size(14.0)
                                                                    .strong()
                                                                    .color(egui::Color32::WHITE),
                                                            );
                                                        },
                                                    );

                                                    // Best server location in this region
                                                    ui.label(
                                                        egui::RichText::new(
                                                            &best_server.display_name,
                                                        )
                                                        .size(11.0)
                                                        .color(egui::Color32::from_rgb(
                                                            120, 120, 140,
                                                        )),
                                                    );

                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            // ETA
                                                            let eta_text =
                                                                crate::api::format_queue_eta(
                                                                    best_server.eta_seconds,
                                                                );
                                                            let eta_color = if best_server
                                                                .eta_seconds
                                                                .unwrap_or(0)
                                                                <= 0
                                                            {
                                                                egui::Color32::from_rgb(118, 185, 0)
                                                            } else if best_server
                                                                .eta_seconds
                                                                .unwrap_or(0)
                                                                < 300
                                                            {
                                                                egui::Color32::from_rgb(
                                                                    255, 200, 50,
                                                                )
                                                            } else {
                                                                egui::Color32::from_rgb(
                                                                    150, 150, 150,
                                                                )
                                                            };
                                                            ui.label(
                                                                egui::RichText::new(format!(
                                                                    "~{}",
                                                                    eta_text
                                                                ))
                                                                .size(11.0)
                                                                .color(eta_color),
                                                            );

                                                            ui.add_space(10.0);

                                                            // Queue position in box
                                                            let queue_color = if best_server
                                                                .queue_position
                                                                <= 0
                                                            {
                                                                egui::Color32::from_rgb(118, 185, 0)
                                                            } else if best_server.queue_position
                                                                < 20
                                                            {
                                                                egui::Color32::from_rgb(
                                                                    255, 200, 50,
                                                                )
                                                            } else if best_server.queue_position
                                                                < 100
                                                            {
                                                                egui::Color32::from_rgb(
                                                                    255, 150, 80,
                                                                )
                                                            } else {
                                                                egui::Color32::from_rgb(
                                                                    255, 100, 100,
                                                                )
                                                            };
                                                            egui::Frame::new()
                                                                .fill(
                                                                    queue_color.gamma_multiply(0.2),
                                                                )
                                                                .corner_radius(3.0)
                                                                .inner_margin(
                                                                    egui::Margin::symmetric(6, 2),
                                                                )
                                                                .show(ui, |ui| {
                                                                    ui.label(
                                                                        egui::RichText::new(
                                                                            format!(
                                                                                "{}",
                                                                                best_server
                                                                                    .queue_position
                                                                            ),
                                                                        )
                                                                        .size(11.0)
                                                                        .strong()
                                                                        .color(queue_color),
                                                                    );
                                                                });

                                                            ui.add_space(10.0);

                                                            // Ping
                                                            if let Some(ping) = best_server.ping_ms
                                                            {
                                                                let ping_color = if ping < 50 {
                                                                    egui::Color32::from_rgb(
                                                                        118, 185, 0,
                                                                    )
                                                                } else if ping < 100 {
                                                                    egui::Color32::from_rgb(
                                                                        255, 200, 50,
                                                                    )
                                                                } else {
                                                                    egui::Color32::from_rgb(
                                                                        255, 150, 80,
                                                                    )
                                                                };
                                                                ui.label(
                                                                    egui::RichText::new(format!(
                                                                        "{}ms",
                                                                        ping
                                                                    ))
                                                                    .size(11.0)
                                                                    .color(ping_color),
                                                                );
                                                            } else {
                                                                ui.spinner();
                                                            }
                                                        },
                                                    );
                                                });
                                            })
                                            .response;

                                        if server_response.interact(egui::Sense::click()).clicked()
                                        {
                                            actions.push(UiAction::SelectQueueServer(Some(
                                                best_server.server_id.clone(),
                                            )));
                                        }

                                        ui.add_space(4.0);
                                    }
                                }
                            });
                    }

                    ui.add_space(16.0);

                    // Buttons
                    ui.horizontal(|ui| {
                        // Launch button
                        let launch_btn = egui::Button::new(
                            egui::RichText::new("  Launch Game  ").size(15.0).strong(),
                        )
                        .fill(egui::Color32::from_rgb(70, 180, 70))
                        .min_size(egui::vec2(140.0, 38.0));

                        if ui.add(launch_btn).clicked() {
                            actions.push(UiAction::LaunchWithServer(
                                game.clone(),
                                selected_server.clone(),
                            ));
                        }

                        ui.add_space(12.0);

                        // Cancel button
                        let cancel_btn =
                            egui::Button::new(egui::RichText::new("  Cancel  ").size(14.0))
                                .fill(egui::Color32::from_rgb(60, 60, 80))
                                .min_size(egui::vec2(90.0, 38.0));

                        if ui.add(cancel_btn).clicked() {
                            actions.push(UiAction::CloseServerSelection);
                        }
                    });

                    // Attribution footer
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Powered by")
                                .size(10.0)
                                .color(egui::Color32::from_rgb(80, 80, 80)),
                        );
                        ui.add_space(4.0);
                        if ui
                            .add(
                                egui::Label::new(
                                    egui::RichText::new("PrintedWaste")
                                        .size(10.0)
                                        .color(egui::Color32::from_rgb(118, 185, 0))
                                        .underline(),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            if let Err(e) = open::that("https://printedwaste.com/gfn/") {
                                warn!("Failed to open PrintedWaste link: {}", e);
                            }
                        }
                    });
                });
            });
    }

    fn render_game_card(
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        _idx: usize,
        game: &crate::app::GameInfo,
        runtime: &tokio::runtime::Handle,
        game_textures: &HashMap<String, egui::TextureHandle>,
        new_textures: &mut Vec<(String, egui::TextureHandle)>,
        actions: &mut Vec<UiAction>,
    ) {
        // Card dimensions - larger for better visibility
        let card_width = 220.0;
        let image_height = 124.0; // 16:9 aspect ratio

        // Make the entire card clickable
        let game_for_click = game.clone();

        let response = egui::Frame::new()
            .fill(egui::Color32::from_rgb(28, 28, 36))
            .corner_radius(8.0)
            .inner_margin(0.0)
            .show(ui, |ui| {
                ui.set_min_width(card_width);

                ui.vertical(|ui| {
                    // Game box art image - full width, no padding
                    if let Some(ref image_url) = game.image_url {
                        // Check if texture is already loaded
                        if let Some(texture) = game_textures.get(image_url) {
                            // Display the image with rounded top corners
                            let size = egui::vec2(card_width, image_height);
                            ui.add(
                                egui::Image::new(texture)
                                    .fit_to_exact_size(size)
                                    .corner_radius(egui::CornerRadius {
                                        nw: 8,
                                        ne: 8,
                                        sw: 0,
                                        se: 0,
                                    }),
                            );
                        } else {
                            // Check if image data is available in cache
                            if let Some((pixels, width, height)) = image_cache::get_image(image_url)
                            {
                                // Create egui texture from pixels
                                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                    [width as usize, height as usize],
                                    &pixels,
                                );
                                let texture = ctx.load_texture(
                                    image_url,
                                    color_image,
                                    egui::TextureOptions::LINEAR,
                                );
                                new_textures.push((image_url.clone(), texture.clone()));

                                // Display immediately
                                let size = egui::vec2(card_width, image_height);
                                ui.add(
                                    egui::Image::new(&texture)
                                        .fit_to_exact_size(size)
                                        .corner_radius(egui::CornerRadius {
                                            nw: 8,
                                            ne: 8,
                                            sw: 0,
                                            se: 0,
                                        }),
                                );
                            } else {
                                // Request loading
                                image_cache::request_image(image_url, runtime);

                                // Show placeholder
                                let placeholder_rect =
                                    ui.allocate_space(egui::vec2(card_width, image_height));
                                ui.painter().rect_filled(
                                    placeholder_rect.1,
                                    egui::CornerRadius {
                                        nw: 8,
                                        ne: 8,
                                        sw: 0,
                                        se: 0,
                                    },
                                    egui::Color32::from_rgb(40, 40, 55),
                                );
                                // Loading spinner effect
                                ui.painter().text(
                                    placeholder_rect.1.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "...",
                                    egui::FontId::proportional(16.0),
                                    egui::Color32::from_rgb(80, 80, 100),
                                );
                            }
                        }
                    } else {
                        // No image URL - show placeholder with game initial
                        let placeholder_rect =
                            ui.allocate_space(egui::vec2(card_width, image_height));
                        ui.painter().rect_filled(
                            placeholder_rect.1,
                            egui::CornerRadius {
                                nw: 8,
                                ne: 8,
                                sw: 0,
                                se: 0,
                            },
                            egui::Color32::from_rgb(45, 45, 65),
                        );
                        // Show first letter of game title
                        let initial = game
                            .title
                            .chars()
                            .next()
                            .unwrap_or('?')
                            .to_uppercase()
                            .to_string();
                        ui.painter().text(
                            placeholder_rect.1.center(),
                            egui::Align2::CENTER_CENTER,
                            initial,
                            egui::FontId::proportional(40.0),
                            egui::Color32::from_rgb(80, 80, 110),
                        );
                    }

                    // Text content area with padding
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            // Game title (truncated if too long)
                            let title = if game.title.chars().count() > 24 {
                                let truncated: String = game.title.chars().take(21).collect();
                                format!("{}...", truncated)
                            } else {
                                game.title.clone()
                            };
                            ui.label(
                                egui::RichText::new(title)
                                    .size(13.0)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            );

                            // Store badge
                            ui.label(
                                egui::RichText::new(game.store.to_uppercase())
                                    .size(10.0)
                                    .color(egui::Color32::from_rgb(100, 180, 255)),
                            );
                        });
                    });
                    ui.add_space(8.0);
                });
            });

        // Hover effect - green glow
        let card_rect = response.response.rect;
        if ui.rect_contains_pointer(card_rect) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            ui.painter().rect_stroke(
                card_rect,
                8.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(118, 185, 0)),
                egui::StrokeKind::Outside,
            );
        }

        if response.response.interact(egui::Sense::click()).clicked() {
            actions.push(UiAction::OpenGamePopup(game_for_click));
        }
    }

    // Note: render_session_conflict_dialog, render_av1_warning_dialog, render_session_screen
    // have been moved to src/gui/screens/dialogs.rs and screens/session.rs
}

// End of impl Renderer block
// Below is the standalone render_stats_panel function

/// Render stats panel (standalone function)
fn render_stats_panel(
    ctx: &egui::Context,
    stats: &crate::media::StreamStats,
    position: crate::app::StatsPosition,
) {
    use egui::{Align2, Color32, FontId, RichText};

    let (anchor, offset) = match position {
        crate::app::StatsPosition::BottomLeft => (Align2::LEFT_BOTTOM, [10.0, -10.0]),
        crate::app::StatsPosition::BottomRight => (Align2::RIGHT_BOTTOM, [-10.0, -10.0]),
        crate::app::StatsPosition::TopLeft => (Align2::LEFT_TOP, [10.0, 10.0]),
        crate::app::StatsPosition::TopRight => (Align2::RIGHT_TOP, [-10.0, 10.0]),
    };

    egui::Area::new(egui::Id::new("stats_panel"))
        .anchor(anchor, offset)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(0, 0, 0, 200))
                .corner_radius(4.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.set_min_width(200.0);

                    // Resolution and HDR status
                    let res_text = if stats.resolution.is_empty() {
                        "Connecting...".to_string()
                    } else {
                        stats.resolution.clone()
                    };

                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(res_text)
                                .font(FontId::monospace(13.0))
                                .color(Color32::WHITE),
                        );

                        // HDR indicator
                        if stats.is_hdr {
                            ui.label(
                                RichText::new(" HDR")
                                    .font(FontId::monospace(13.0))
                                    .color(Color32::from_rgb(255, 180, 0)), // Orange/gold for HDR
                            );
                        }
                    });

                    // Decoded FPS vs Render FPS (shows if renderer is bottlenecked)
                    let decode_fps = stats.fps;
                    let render_fps = stats.render_fps;
                    let target_fps = stats.target_fps as f32;

                    // Decode FPS color
                    let decode_color = if target_fps > 0.0 {
                        let ratio = decode_fps / target_fps;
                        if ratio >= 0.8 {
                            Color32::GREEN
                        } else if ratio >= 0.5 {
                            Color32::YELLOW
                        } else {
                            Color32::from_rgb(255, 100, 100)
                        }
                    } else {
                        Color32::WHITE
                    };

                    // Render FPS color (critical - this is what you actually see)
                    let render_color = if target_fps > 0.0 {
                        let ratio = render_fps / target_fps;
                        if ratio >= 0.8 {
                            Color32::GREEN
                        } else if ratio >= 0.5 {
                            Color32::YELLOW
                        } else {
                            Color32::from_rgb(255, 100, 100)
                        }
                    } else {
                        Color32::WHITE
                    };

                    // Show both FPS values
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("Decode: {:.0}", decode_fps))
                                .font(FontId::monospace(11.0))
                                .color(decode_color),
                        );
                        ui.label(
                            RichText::new(format!(" | Render: {:.0}", render_fps))
                                .font(FontId::monospace(11.0))
                                .color(render_color),
                        );
                        if stats.target_fps > 0 {
                            ui.label(
                                RichText::new(format!(" / {} fps", stats.target_fps))
                                    .font(FontId::monospace(11.0))
                                    .color(Color32::GRAY),
                            );
                        }
                    });

                    // Codec and bitrate
                    if !stats.codec.is_empty() {
                        ui.label(
                            RichText::new(format!(
                                "{} | {:.1} Mbps",
                                stats.codec, stats.bitrate_mbps
                            ))
                            .font(FontId::monospace(11.0))
                            .color(Color32::LIGHT_GRAY),
                        );
                    }

                    // Latency (decode pipeline)
                    let latency_color = if stats.latency_ms < 30.0 {
                        Color32::GREEN
                    } else if stats.latency_ms < 60.0 {
                        Color32::YELLOW
                    } else {
                        Color32::RED
                    };

                    ui.label(
                        RichText::new(format!("Decode: {:.0} ms", stats.latency_ms))
                            .font(FontId::monospace(11.0))
                            .color(latency_color),
                    );

                    // Network RTT (round-trip time from ICE)
                    if stats.rtt_ms > 0.0 {
                        let rtt_color = if stats.rtt_ms < 30.0 {
                            Color32::GREEN
                        } else if stats.rtt_ms < 60.0 {
                            Color32::YELLOW
                        } else {
                            Color32::RED
                        };

                        ui.label(
                            RichText::new(format!("RTT: {:.0} ms", stats.rtt_ms))
                                .font(FontId::monospace(11.0))
                                .color(rtt_color),
                        );
                    } else {
                        ui.label(
                            RichText::new("RTT: N/A")
                                .font(FontId::monospace(11.0))
                                .color(Color32::GRAY),
                        );
                    }

                    // Estimated end-to-end latency (motion-to-photon)
                    if stats.estimated_e2e_ms > 0.0 {
                        let e2e_color = if stats.estimated_e2e_ms < 80.0 {
                            Color32::GREEN
                        } else if stats.estimated_e2e_ms < 150.0 {
                            Color32::YELLOW
                        } else {
                            Color32::RED
                        };

                        ui.label(
                            RichText::new(format!("E2E: ~{:.0} ms", stats.estimated_e2e_ms))
                                .font(FontId::monospace(11.0))
                                .color(e2e_color),
                        );
                    }

                    // Input rate and client-side latency
                    if stats.input_rate > 0.0 || stats.input_latency_ms > 0.0 {
                        let rate_str = if stats.input_rate > 0.0 {
                            format!("{:.0}/s", stats.input_rate)
                        } else {
                            "0/s".to_string()
                        };
                        let latency_str = if stats.input_latency_ms > 0.001 {
                            format!("{:.2}ms", stats.input_latency_ms)
                        } else {
                            "<0.01ms".to_string()
                        };
                        ui.label(
                            RichText::new(format!("Input: {} ({})", rate_str, latency_str))
                                .font(FontId::monospace(10.0))
                                .color(Color32::GRAY),
                        );
                    }

                    // Frame delivery latency (RTP to decode)
                    if stats.frame_delivery_ms > 0.0 {
                        let delivery_color = if stats.frame_delivery_ms < 10.0 {
                            Color32::GREEN
                        } else if stats.frame_delivery_ms < 20.0 {
                            Color32::YELLOW
                        } else {
                            Color32::RED
                        };
                        ui.label(
                            RichText::new(format!(
                                "Frame delivery: {:.1} ms",
                                stats.frame_delivery_ms
                            ))
                            .font(FontId::monospace(10.0))
                            .color(delivery_color),
                        );
                    }

                    if stats.packet_loss > 0.0 {
                        let loss_color = if stats.packet_loss < 1.0 {
                            Color32::YELLOW
                        } else {
                            Color32::RED
                        };

                        ui.label(
                            RichText::new(format!("Packet Loss: {:.1}%", stats.packet_loss))
                                .font(FontId::monospace(11.0))
                                .color(loss_color),
                        );
                    }

                    // Decode and render times
                    if stats.decode_time_ms > 0.0 || stats.render_time_ms > 0.0 {
                        ui.label(
                            RichText::new(format!(
                                "Decode: {:.1} ms | Render: {:.1} ms",
                                stats.decode_time_ms, stats.render_time_ms
                            ))
                            .font(FontId::monospace(10.0))
                            .color(Color32::GRAY),
                        );
                    }

                    // Frame stats
                    if stats.frames_received > 0 {
                        ui.label(
                            RichText::new(format!(
                                "Frames: {} rx, {} dec, {} drop",
                                stats.frames_received, stats.frames_decoded, stats.frames_dropped
                            ))
                            .font(FontId::monospace(10.0))
                            .color(Color32::DARK_GRAY),
                        );
                    }

                    // GPU and server info
                    if !stats.gpu_type.is_empty() || !stats.server_region.is_empty() {
                        let info = format!(
                            "{}{}{}",
                            stats.gpu_type,
                            if !stats.gpu_type.is_empty() && !stats.server_region.is_empty() {
                                " | "
                            } else {
                                ""
                            },
                            stats.server_region
                        );

                        ui.label(
                            RichText::new(info)
                                .font(FontId::monospace(10.0))
                                .color(Color32::DARK_GRAY),
                        );
                    }
                });
        });
}

/// Render resolution change notification popup (animated, center-top)
fn render_resolution_notification(
    ctx: &egui::Context,
    old_res: &str,
    new_res: &str,
    direction: ResolutionDirection,
    alpha: f32,
) {
    use egui::{Align2, Color32, FontId, RichText};

    // Calculate alpha for animation (0-255)
    let alpha_u8 = (alpha * 255.0) as u8;

    // Colors based on direction (using ASCII-compatible symbols)
    let (arrow, color, label) = match direction {
        ResolutionDirection::Up => (
            "+",
            Color32::from_rgba_unmultiplied(100, 255, 100, alpha_u8),
            "Quality Increased",
        ),
        ResolutionDirection::Down => (
            "-",
            Color32::from_rgba_unmultiplied(255, 150, 100, alpha_u8),
            "Quality Decreased",
        ),
        ResolutionDirection::Same => (
            "=",
            Color32::from_rgba_unmultiplied(200, 200, 200, alpha_u8),
            "Quality Changed",
        ),
    };

    // Slide-in animation: start 20px above, slide down to final position
    let slide_offset = (1.0 - alpha.min(1.0)) * -20.0;

    egui::Area::new(egui::Id::new("resolution_notification"))
        .anchor(Align2::CENTER_TOP, [0.0, 40.0 + slide_offset])
        .interactable(false)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(
                    20,
                    20,
                    25,
                    (alpha * 230.0) as u8,
                ))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(16, 12))
                .stroke(egui::Stroke::new(
                    1.0,
                    Color32::from_rgba_unmultiplied(80, 80, 90, alpha_u8),
                ))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;

                        // Title with arrow
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(arrow)
                                    .font(FontId::proportional(18.0))
                                    .color(color),
                            );
                            ui.label(
                                RichText::new(label).font(FontId::proportional(14.0)).color(
                                    Color32::from_rgba_unmultiplied(255, 255, 255, alpha_u8),
                                ),
                            );
                        });

                        // Resolution change details
                        ui.horizontal(|ui| {
                            // Old resolution (strikethrough effect with dim color)
                            ui.label(
                                RichText::new(old_res).font(FontId::monospace(13.0)).color(
                                    Color32::from_rgba_unmultiplied(150, 150, 150, alpha_u8),
                                ),
                            );
                            ui.label(
                                RichText::new("->").font(FontId::monospace(13.0)).color(
                                    Color32::from_rgba_unmultiplied(200, 200, 200, alpha_u8),
                                ),
                            );
                            // New resolution (bright)
                            ui.label(
                                RichText::new(new_res)
                                    .font(FontId::monospace(13.0))
                                    .strong()
                                    .color(color),
                            );
                        });
                    });
                });
        });

    // Request repaint for smooth animation
    ctx.request_repaint();
}

/// Render racing wheel connection notification popup (animated, center-top)
/// Shows when a racing wheel is detected during streaming session
fn render_wheel_notification(ctx: &egui::Context, wheel_count: usize, alpha: f32) {
    use egui::{Align2, Color32, FontId, RichText};

    // Calculate alpha for animation (0-255)
    let alpha_u8 = (alpha * 255.0) as u8;

    // Green color for wheel detection (positive feedback)
    let accent_color = Color32::from_rgba_unmultiplied(100, 200, 100, alpha_u8);

    // Slide-in animation: start 20px above, slide down to final position
    let slide_offset = (1.0 - alpha.min(1.0)) * -20.0;

    egui::Area::new(egui::Id::new("wheel_notification"))
        .anchor(Align2::CENTER_TOP, [0.0, 100.0 + slide_offset]) // Below resolution notification
        .interactable(false)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(
                    20,
                    30,
                    25,
                    (alpha * 230.0) as u8,
                ))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(16, 12))
                .stroke(egui::Stroke::new(
                    1.0,
                    Color32::from_rgba_unmultiplied(60, 100, 70, alpha_u8),
                ))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;

                        // Title with steering wheel icon (using ASCII-compatible symbol)
                        ui.horizontal(|ui| {
                            // Use a circle/wheel-like character that's cross-platform compatible
                            ui.label(
                                RichText::new("(O)")
                                    .font(FontId::monospace(16.0))
                                    .color(accent_color),
                            );
                            ui.label(
                                RichText::new("Racing Wheel Detected")
                                    .font(FontId::proportional(14.0))
                                    .strong()
                                    .color(Color32::from_rgba_unmultiplied(
                                        255, 255, 255, alpha_u8,
                                    )),
                            );
                        });

                        // Wheel count and info
                        ui.horizontal(|ui| {
                            let wheel_text = if wheel_count == 1 {
                                "1 wheel connected".to_string()
                            } else {
                                format!("{} wheels connected", wheel_count)
                            };
                            ui.label(
                                RichText::new(wheel_text)
                                    .font(FontId::monospace(12.0))
                                    .color(Color32::from_rgba_unmultiplied(
                                        180, 180, 180, alpha_u8,
                                    )),
                            );
                        });

                        // Supported features hint
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Wheel, pedals, and shifter input active")
                                    .font(FontId::proportional(11.0))
                                    .color(Color32::from_rgba_unmultiplied(
                                        140, 160, 140, alpha_u8,
                                    )),
                            );
                        });
                    });
                });
        });

    // Request repaint for smooth animation
    ctx.request_repaint();
}

/// Render file transfer notification popups (bottom-right corner)
/// Shows upload progress with file name, progress bar, and speed
fn render_file_transfer_notifications(
    ctx: &egui::Context,
    transfers: &[crate::app::types::FileTransfer],
    actions: &mut Vec<crate::app::UiAction>,
) {
    use crate::app::types::FileTransferState;
    use egui::{Align2, Color32, FontId, RichText};

    // Show up to 3 transfers at once
    let visible_transfers: Vec<_> = transfers.iter().take(3).collect();

    for (idx, transfer) in visible_transfers.iter().enumerate() {
        let y_offset = 80.0 + (idx as f32 * 90.0); // Stack vertically

        egui::Area::new(egui::Id::new(format!("file_transfer_{}", transfer.id)))
            .anchor(Align2::RIGHT_BOTTOM, [-20.0, -y_offset])
            .interactable(true)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(25, 25, 35, 240))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .stroke(egui::Stroke::new(1.0, Color32::from_rgb(60, 60, 80)))
                    .show(ui, |ui| {
                        ui.set_width(280.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 6.0;

                            // Header row with file name and close button
                            ui.horizontal(|ui| {
                                // Upload icon
                                ui.label(
                                    RichText::new("^")
                                        .font(FontId::monospace(14.0))
                                        .color(Color32::from_rgb(100, 180, 255)),
                                );

                                // File name (truncated)
                                let display_name = if transfer.file_name.len() > 25 {
                                    format!("{}...", &transfer.file_name[..22])
                                } else {
                                    transfer.file_name.clone()
                                };
                                ui.label(
                                    RichText::new(&display_name)
                                        .font(FontId::proportional(13.0))
                                        .color(Color32::WHITE),
                                );

                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    // Close/dismiss button
                                    let close_btn = egui::Button::new(
                                        RichText::new("x")
                                            .font(FontId::proportional(12.0))
                                            .color(Color32::from_rgb(150, 150, 150)),
                                    )
                                    .fill(Color32::TRANSPARENT)
                                    .frame(false);

                                    if ui.add(close_btn).clicked() {
                                        actions.push(crate::app::UiAction::DismissFileTransfer(
                                            transfer.id.clone(),
                                        ));
                                    }
                                });
                            });

                            // Progress bar
                            let progress = transfer.progress_percent() as f32 / 100.0;
                            let (bar_color, status_text) = match &transfer.state {
                                FileTransferState::Pending => (
                                    Color32::from_rgb(100, 100, 120),
                                    "Starting...".to_string(),
                                ),
                                FileTransferState::Uploading => (
                                    Color32::from_rgb(100, 180, 255),
                                    format!("{}%", transfer.progress_percent()),
                                ),
                                FileTransferState::Complete => (
                                    Color32::from_rgb(100, 200, 100),
                                    "Complete".to_string(),
                                ),
                                FileTransferState::Failed(err) => (
                                    Color32::from_rgb(220, 80, 80),
                                    format!("Failed: {}", if err.len() > 20 { &err[..20] } else { err }),
                                ),
                            };

                            // Custom progress bar
                            let bar_height = 6.0;
                            let bar_rect = ui.available_rect_before_wrap();
                            let bar_rect = egui::Rect::from_min_size(
                                bar_rect.min,
                                egui::vec2(ui.available_width(), bar_height),
                            );
                            ui.allocate_space(egui::vec2(ui.available_width(), bar_height));

                            // Background
                            ui.painter().rect_filled(
                                bar_rect,
                                3.0,
                                Color32::from_rgb(40, 40, 50),
                            );

                            // Progress fill
                            if progress > 0.0 {
                                let fill_rect = egui::Rect::from_min_size(
                                    bar_rect.min,
                                    egui::vec2(bar_rect.width() * progress, bar_height),
                                );
                                ui.painter().rect_filled(fill_rect, 3.0, bar_color);
                            }

                            ui.add_space(2.0);

                            // Status row: size, speed, status
                            ui.horizontal(|ui| {
                                // File size
                                ui.label(
                                    RichText::new(transfer.size_string())
                                        .font(FontId::monospace(10.0))
                                        .color(Color32::from_rgb(140, 140, 150)),
                                );

                                // Speed (only when uploading)
                                if matches!(transfer.state, FileTransferState::Uploading) {
                                    ui.label(
                                        RichText::new(format!("| {}", transfer.speed_string()))
                                            .font(FontId::monospace(10.0))
                                            .color(Color32::from_rgb(100, 180, 255)),
                                    );
                                }

                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    // Status text
                                    let status_color = match &transfer.state {
                                        FileTransferState::Complete => Color32::from_rgb(100, 200, 100),
                                        FileTransferState::Failed(_) => Color32::from_rgb(220, 80, 80),
                                        _ => Color32::from_rgb(150, 150, 160),
                                    };
                                    ui.label(
                                        RichText::new(&status_text)
                                            .font(FontId::proportional(10.0))
                                            .color(status_color),
                                    );
                                });
                            });

                            // Cancel button (only when uploading)
                            if matches!(transfer.state, FileTransferState::Uploading | FileTransferState::Pending) {
                                ui.add_space(4.0);
                                let cancel_btn = egui::Button::new(
                                    RichText::new("Cancel")
                                        .font(FontId::proportional(11.0))
                                        .color(Color32::from_rgb(200, 100, 100)),
                                )
                                .fill(Color32::from_rgb(50, 35, 35))
                                .corner_radius(4.0);

                                if ui.add_sized([ui.available_width(), 22.0], cancel_btn).clicked() {
                                    actions.push(crate::app::UiAction::CancelFileTransfer(
                                        transfer.id.clone(),
                                    ));
                                }
                            }
                        });
                    });
            });
    }

    // Request repaint for progress updates
    if !transfers.is_empty() {
        ctx.request_repaint();
    }
}
