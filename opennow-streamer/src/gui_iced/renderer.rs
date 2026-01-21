//! Iced Renderer Integration
//!
//! Integrates iced with our wgpu rendering pipeline.
//! Based on iced's integration example.

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use std::sync::Arc;

use iced_wgpu::graphics::{Shell, Viewport};
use iced_wgpu::{Engine, Renderer as IcedRenderer, wgpu};
use iced_winit::Clipboard;
use iced_winit::conversion;
use iced_winit::core::mouse;
use iced_winit::core::renderer;
use iced_winit::core::time::Instant;
use iced_winit::core::window;
use iced_winit::core::{Event, Font, Pixels, Size, Theme};
use iced_winit::runtime::user_interface::{self, UserInterface};
use iced_winit::winit;

use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::ModifiersState;
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowAttributes};

use super::controls::Controls;
use super::image_cache;
use super::shaders::{NV12_SHADER, P010_SHADER};
use crate::app::{AppState, GameInfo, GameSection, ServerInfo, Settings, SubscriptionInfo, UiAction};
use crate::media::{ColorRange, ColorSpace, PixelFormat, VideoFrame};



/// Response from handling an event
#[derive(Debug, Clone, Default)]
pub struct EventResponse {
    /// Whether the UI needs to be repainted
    pub repaint: bool,
    /// Whether the event was consumed by the UI
    pub consumed: bool,
}

/// Main renderer that combines iced UI with video frame rendering
pub struct Renderer {
    // Window
    window: Arc<Window>,
    fullscreen: bool,
    
    // wgpu state
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    surface_config: wgpu::SurfaceConfiguration,
    
    // iced state
    iced_renderer: IcedRenderer,
    viewport: Viewport,
    clipboard: Clipboard,
    cache: user_interface::Cache,
    cursor: mouse::Cursor,
    modifiers: ModifiersState,
    
    // UI controls
    controls: Controls,
    
    // Video rendering (for streaming) - NV12 format (Y + UV interleaved)
    nv12_pipeline: wgpu::RenderPipeline,
    nv12_bind_group_layout: wgpu::BindGroupLayout,
    video_sampler: wgpu::Sampler,
    y_texture: Option<wgpu::Texture>,
    uv_texture: Option<wgpu::Texture>,
    nv12_bind_group: Option<wgpu::BindGroup>,
    video_size: (u32, u32),
    current_format: PixelFormat,
    last_uploaded_frame_id: u64,
    /// Uniform buffer for color conversion parameters
    color_params_buffer: wgpu::Buffer,
    /// Current color range (0 = Limited, 1 = Full)
    current_color_range: u32,
    /// Current color space (0 = BT.709, 1 = BT.2020)
    current_color_space: u32,
    
    // P010 (10-bit HDR) pipeline
    p010_pipeline: wgpu::RenderPipeline,
    p010_bind_group_layout: wgpu::BindGroupLayout,
    p010_y_texture: Option<wgpu::Texture>,
    p010_uv_texture: Option<wgpu::Texture>,
    p010_bind_group: Option<wgpu::BindGroup>,
    
    // Stats
    frame_count: u64,
}

impl Renderer {
    /// Create a new renderer
    pub async fn new(event_loop: &ActiveEventLoop) -> Result<Self> {
        // Load settings to get saved window size
        let settings = crate::app::Settings::load().unwrap_or_default();
        
        let default_size = PhysicalSize::new(1280u32, 720u32);
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
        
        let window = Arc::new(
            event_loop
                .create_window(window_attrs)
                .context("Failed to create window")?,
        );
        
        let size = window.inner_size();
        info!("Window created: {}x{}", size.width, size.height);
        
        // Create wgpu instance
        #[cfg(target_os = "windows")]
        let backends = wgpu::Backends::DX12;
        #[cfg(not(target_os = "windows"))]
        let backends = wgpu::Backends::all();
        
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });
        
        let surface = instance
            .create_surface(window.clone())
            .context("Failed to create surface")?;
        
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("Failed to find GPU adapter")?;
        
        info!("Using GPU: {}", adapter.get_info().name);
        
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("OpenNow Device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            })
            .await
            .context("Failed to create device")?;
        
        let capabilities = surface.get_capabilities(&adapter);
        let surface_format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .context("No suitable surface format")?;
        
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 1, // Minimum queue depth for lowest latency
        };
        surface.configure(&device, &surface_config);
        
        // Create iced viewport
        let viewport = Viewport::with_physical_size(
            Size::new(size.width, size.height),
            window.scale_factor() as f32,
        );
        
        // Create iced renderer
        let engine = Engine::new(
            &adapter,
            device.clone(),
            queue.clone(),
            surface_format,
            None,
            Shell::headless(),
        );
        let iced_renderer = IcedRenderer::new(engine, Font::default(), Pixels::from(16));
        
        let clipboard = Clipboard::connect(window.clone());
        
        // Create NV12 video rendering pipeline (for streaming)
        let nv12_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("NV12 Shader"),
            source: wgpu::ShaderSource::Wgsl(NV12_SHADER.into()),
        });
        
        let nv12_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
                // Color params uniform buffer
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
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
        
        // Create uniform buffer for color conversion parameters
        // Layout: [color_range: u32, color_space: u32, padding: u32, padding: u32] = 16 bytes
        let color_params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Color Params Buffer"),
            size: 16, // 4 bytes color_range + 4 bytes color_space + 8 bytes padding
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        
        info!("NV12 video pipeline created");
        
        // Create P010 (10-bit HDR) video rendering pipeline
        let p010_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("P010 Shader"),
            source: wgpu::ShaderSource::Wgsl(P010_SHADER.into()),
        });
        
        // P010 uses R16Unorm for Y and Rg16Unorm for UV (10-bit in 16-bit container)
        let p010_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("P010 Bind Group Layout"),
            entries: &[
                // Y texture (full resolution, R16)
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
                // UV texture (half resolution, Rg16 interleaved)
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
                // Color params uniform buffer
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        
        let p010_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("P010 Pipeline Layout"),
            bind_group_layouts: &[&p010_bind_group_layout],
            immediate_size: 0,
        });
        
        let p010_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("P010 Pipeline"),
            layout: Some(&p010_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &p010_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &p010_shader,
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
        
        info!("P010 video pipeline created");
        
        Ok(Self {
            window,
            fullscreen: false,
            device,
            queue,
            surface,
            surface_format,
            surface_config,
            iced_renderer,
            viewport,
            clipboard,
            cache: user_interface::Cache::new(),
            cursor: mouse::Cursor::Unavailable,
            modifiers: ModifiersState::empty(),
            controls: Controls::new(),
            nv12_pipeline,
            nv12_bind_group_layout,
            video_sampler,
            y_texture: None,
            uv_texture: None,
            nv12_bind_group: None,
            video_size: (0, 0),
            current_format: PixelFormat::NV12,
            last_uploaded_frame_id: 0,
            color_params_buffer,
            current_color_range: 0, // Default to Limited Range
            current_color_space: 0, // Default to BT.709
            p010_pipeline,
            p010_bind_group_layout,
            p010_y_texture: None,
            p010_uv_texture: None,
            p010_bind_group: None,
            frame_count: 0,
        })
    }
    
    /// Get window reference
    pub fn window(&self) -> &Window {
        &self.window
    }
    
    /// Handle window event - returns EventResponse for repaint decisions
    pub fn handle_event(&mut self, event: &WindowEvent) -> EventResponse {
        let mut response = EventResponse::default();
        
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = mouse::Cursor::Available(conversion::cursor_position(
                    *position,
                    self.viewport.scale_factor(),
                ));
                response.repaint = true;
            }
            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers.state();
            }
            WindowEvent::Resized(new_size) => {
                if new_size.width > 0 && new_size.height > 0 {
                    self.resize(*new_size);
                }
                response.repaint = true;
            }
            WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } | WindowEvent::KeyboardInput { .. } => {
                response.repaint = true;
                response.consumed = true;
            }
            _ => {}
        }
        
        // Check if iced would handle this event
        if conversion::window_event(
            event.clone(),
            self.window.scale_factor() as f32,
            self.modifiers,
        ).is_some() {
            response.repaint = true;
        }
        
        response
    }
    
    /// Resize the renderer
    pub fn resize(&mut self, new_size: PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        
        self.surface_config.width = new_size.width;
        self.surface_config.height = new_size.height;
        self.surface.configure(&self.device, &self.surface_config);
        
        self.viewport = Viewport::with_physical_size(
            Size::new(new_size.width, new_size.height),
            self.window.scale_factor() as f32,
        );
    }
    
    /// Update video textures from a decoded video frame
    /// Creates/recreates textures if size changes, then uploads Y and UV plane data
    fn update_video(&mut self, frame: &VideoFrame) {
        // Skip if this frame was already uploaded
        if frame.frame_id == self.last_uploaded_frame_id {
            return;
        }
        
        // Upload plane data from frame.y_plane/u_plane
        
        // Calculate UV plane dimensions (half resolution for 4:2:0 subsampling)
        let uv_width = (frame.width + 1) / 2;
        let uv_height = (frame.height + 1) / 2;
        
        // Check if we need to recreate textures (size or format change)
        let size_changed = self.video_size != (frame.width, frame.height);
        let format_changed = self.current_format != frame.format;
        
        if size_changed || format_changed {
            self.video_size = (frame.width, frame.height);
            self.current_format = frame.format;
            
            match frame.format {
                PixelFormat::P010 => {
                    // P010: 10-bit in 16-bit container, use R16Unorm/Rg16Unorm
                    let y_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("P010 Y Texture"),
                        size: wgpu::Extent3d {
                            width: frame.width,
                            height: frame.height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::R16Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    
                    let uv_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("P010 UV Texture"),
                        size: wgpu::Extent3d {
                            width: uv_width,
                            height: uv_height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rg16Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    
                    let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());
                    
                    let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("P010 Bind Group"),
                        layout: &self.p010_bind_group_layout,
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
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: self.color_params_buffer.as_entire_binding(),
                            },
                        ],
                    });
                    
                    self.p010_y_texture = Some(y_texture);
                    self.p010_uv_texture = Some(uv_texture);
                    self.p010_bind_group = Some(bind_group);
                    // Clear NV12 textures when using P010
                    self.y_texture = None;
                    self.uv_texture = None;
                    self.nv12_bind_group = None;
                    
                    info!(
                        "P010 textures created: {}x{} (UV: {}x{})",
                        frame.width, frame.height, uv_width, uv_height
                    );
                }
                PixelFormat::NV12 | PixelFormat::YUV420P => {
                    // NV12/YUV420P: 8-bit, use R8Unorm/Rg8Unorm
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
                        format: wgpu::TextureFormat::Rg8Unorm,
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
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: self.color_params_buffer.as_entire_binding(),
                            },
                        ],
                    });
                    
                    self.y_texture = Some(y_texture);
                    self.uv_texture = Some(uv_texture);
                    self.nv12_bind_group = Some(bind_group);
                    // Clear P010 textures when using NV12
                    self.p010_y_texture = None;
                    self.p010_uv_texture = None;
                    self.p010_bind_group = None;
                    
                    info!(
                        "NV12 textures created: {}x{} (UV: {}x{})",
                        frame.width, frame.height, uv_width, uv_height
                    );
                }
            }
        }
        
        // Upload texture data based on format
        match frame.format {
            PixelFormat::P010 => {
                // P010: 16-bit per sample, 2 bytes per Y pixel, 4 bytes per UV pixel pair
                if let Some(ref texture) = self.p010_y_texture {
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
                
                if let Some(ref texture) = self.p010_uv_texture {
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
            PixelFormat::NV12 | PixelFormat::YUV420P => {
                // NV12/YUV420P: 8-bit per sample
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
        
        // Update color params uniform if changed
        let new_color_range = match frame.color_range {
            ColorRange::Full => 1u32,
            ColorRange::Limited => 0u32,
        };
        let new_color_space = match frame.color_space {
            ColorSpace::BT2020 => 1u32,
            ColorSpace::BT709 | ColorSpace::BT601 => 0u32,
        };
        
        if new_color_range != self.current_color_range || new_color_space != self.current_color_space {
            // Write new color params to uniform buffer
            // Layout: [color_range: u32, color_space: u32, padding: u32, padding: u32]
            let color_params: [u32; 4] = [new_color_range, new_color_space, 0, 0];
            self.queue.write_buffer(
                &self.color_params_buffer,
                0,
                bytemuck::cast_slice(&color_params),
            );
            
            if new_color_range != self.current_color_range {
                info!("Color range changed to: {}", if new_color_range == 1 { "Full" } else { "Limited" });
            }
            if new_color_space != self.current_color_space {
                info!("Color space changed to: {}", if new_color_space == 1 { "BT.2020" } else { "BT.709" });
            }
            
            self.current_color_range = new_color_range;
            self.current_color_space = new_color_space;
        }
        
        // Mark this frame as uploaded
        self.last_uploaded_frame_id = frame.frame_id;
    }
    
    /// Render video frame to screen using the appropriate pipeline based on format
    fn render_video(&self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
        // Select pipeline and bind group based on current format
        let (pipeline, bind_group) = match self.current_format {
            PixelFormat::P010 => (&self.p010_pipeline, self.p010_bind_group.as_ref()),
            PixelFormat::NV12 | PixelFormat::YUV420P => (&self.nv12_pipeline, self.nv12_bind_group.as_ref()),
        };
        
        if let Some(bind_group) = bind_group {
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
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            
            render_pass.set_pipeline(pipeline);
            render_pass.set_bind_group(0, bind_group, &[]);
            render_pass.draw(0..6, 0..1); // Draw 6 vertices (2 triangles = 1 quad)
        }
    }
    
    /// Render a frame
    pub fn render(
        &mut self,
        app_state: AppState,
        games: &[GameInfo],
        library_games: &[GameInfo],
        game_sections: &[GameSection],
        status_message: &str,
        user_name: Option<&str>,
        servers: &[ServerInfo],
        selected_server_index: usize,
        subscription: Option<&SubscriptionInfo>,
        video_frame: Option<&VideoFrame>,
        events: &[Event],
        // Sync state from app
        show_settings: bool,
        selected_game_popup: Option<&GameInfo>,
        show_session_conflict: bool,
        show_av1_warning: bool,
        show_alliance_warning: bool,
        show_welcome: bool,
        settings: &Settings,
        runtime: &tokio::runtime::Handle,
        show_stats: bool,
        stats: &crate::media::StreamStats,
        decoder_backend: &str,
        login_providers: &[crate::auth::LoginProvider],
        selected_provider_index: usize,
    ) -> Result<Vec<UiAction>> {
        let mut ui_actions = Vec::new();
        
        // Update image cache and sync loaded images to controls
        image_cache::update_cache();
        
        // Request loading for game images that aren't loaded yet
        if app_state == AppState::Games {
            // Request images for visible games
            for game in games.iter().take(50) {
                if let Some(ref url) = game.image_url {
                    if !self.controls.loaded_images.contains_key(url) {
                        if let Some((pixels, width, height)) = image_cache::get_image(url) {
                            // Image is loaded, create iced handle
                            let handle = iced_widget::image::Handle::from_rgba(
                                width,
                                height,
                                (*pixels).clone(),
                            );
                            self.controls.loaded_images.insert(url.clone(), handle);
                        } else {
                            // Request loading
                            image_cache::request_image(url, runtime);
                        }
                    }
                }
            }
            
            // Also load images from game sections
            for section in game_sections {
                for game in section.games.iter().take(15) {
                    if let Some(ref url) = game.image_url {
                        if !self.controls.loaded_images.contains_key(url) {
                            if let Some((pixels, width, height)) = image_cache::get_image(url) {
                                let handle = iced_widget::image::Handle::from_rgba(
                                    width,
                                    height,
                                    (*pixels).clone(),
                                );
                                self.controls.loaded_images.insert(url.clone(), handle);
                            } else {
                                image_cache::request_image(url, runtime);
                            }
                        }
                    }
                }
            }
            
            // Also load images for library games
            for game in library_games.iter().take(50) {
                if let Some(ref url) = game.image_url {
                    if !self.controls.loaded_images.contains_key(url) {
                        if let Some((pixels, width, height)) = image_cache::get_image(url) {
                            let handle = iced_widget::image::Handle::from_rgba(
                                width,
                                height,
                                (*pixels).clone(),
                            );
                            self.controls.loaded_images.insert(url.clone(), handle);
                        } else {
                            image_cache::request_image(url, runtime);
                        }
                    }
                }
            }
        }
        
        // Sync controls state
        self.controls.sync_from_app(
            settings,
            show_settings,
            selected_game_popup,
            show_session_conflict,
            show_av1_warning,
            show_alliance_warning,
            show_welcome,
        );
        
        // Get surface texture
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(ui_actions);
            }
            Err(e) => {
                error!("Surface error: {:?}", e);
                return Ok(ui_actions);
            }
        };
        
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Render Encoder"),
        });
        
        // Update video texture if streaming and we have a frame
        if app_state == AppState::Streaming {
            if let Some(video) = video_frame {
                self.update_video(video);
            }
        }
        
        // Render video or clear based on state
        let has_video = self.nv12_bind_group.is_some() || self.p010_bind_group.is_some();
        if app_state == AppState::Streaming && has_video {
            // Render video full-screen using appropriate shader (NV12/P010)
            self.render_video(&mut encoder, &view);
        } else {
            // Clear with dark background for UI
            let _render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Clear Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.078,
                            g: 0.078,
                            b: 0.118,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        
        // Submit clear pass
        self.queue.submit([encoder.finish()]);
        
        // Build and render iced UI
        if app_state == AppState::Streaming {
            // During streaming, only show stats overlay if enabled
            if show_stats {
                let stats_view = self.controls.view_stats_overlay(stats, decoder_backend);
                let mut interface = UserInterface::build(
                    stats_view,
                    self.viewport.logical_size(),
                    std::mem::take(&mut self.cache),
                    &mut self.iced_renderer,
                );
                
                interface.draw(
                    &mut self.iced_renderer,
                    &Theme::Dark,
                    &renderer::Style::default(),
                    self.cursor,
                );
                self.cache = interface.into_cache();
                
                // Present iced rendering (overlay on top of video)
                self.iced_renderer.present(
                    None,
                    frame.texture.format(),
                    &view,
                    &self.viewport,
                );
            }
        } else {
            // Build user interface for non-streaming states
            let mut interface = UserInterface::build(
                self.controls.view(
                    app_state,
                    games,
                    library_games,
                    game_sections,
                    status_message,
                    user_name,
                    servers,
                    selected_server_index,
                    subscription,
                    login_providers,
                    selected_provider_index,
                ),
                self.viewport.logical_size(),
                std::mem::take(&mut self.cache),
                &mut self.iced_renderer,
            );
            
            // Process events
            let mut all_events = events.to_vec();
            all_events.push(Event::Window(window::Event::RedrawRequested(Instant::now())));
            
            let mut messages = Vec::new();
            let (state, _) = interface.update(
                &all_events,
                self.cursor,
                &mut self.iced_renderer,
                &mut self.clipboard,
                &mut messages,
            );
            
            // Update cursor
            if let user_interface::State::Updated { mouse_interaction, .. } = state {
                if let Some(icon) = iced_winit::conversion::mouse_interaction(mouse_interaction) {
                    self.window.set_cursor(icon);
                    self.window.set_cursor_visible(true);
                } else {
                    self.window.set_cursor_visible(false);
                }
            }
            
            // Draw the interface
            interface.draw(
                &mut self.iced_renderer,
                &Theme::Dark,
                &renderer::Style::default(),
                self.cursor,
            );
            self.cache = interface.into_cache();
            
            // Process messages and collect UI actions
            for message in messages {
                if let Some(action) = self.controls.update(message) {
                    ui_actions.push(action);
                }
            }
            
            // Present iced rendering
            self.iced_renderer.present(
                None,
                frame.texture.format(),
                &view,
                &self.viewport,
            );
        }
        
        // Present the frame
        frame.present();
        
        self.frame_count += 1;
        
        Ok(ui_actions)
    }
    
    /// Check if fullscreen
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }
    
    /// Toggle fullscreen (borderless parameter controls borderless vs exclusive)
    pub fn toggle_fullscreen(&mut self, _borderless: bool) {
        self.fullscreen = !self.fullscreen;
        if self.fullscreen {
            self.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            self.window.set_fullscreen(None);
        }
    }
    
    /// Lock cursor (for streaming)
    pub fn lock_cursor(&self) {
        let _ = self.window.set_cursor_grab(CursorGrabMode::Confined)
            .or_else(|_| self.window.set_cursor_grab(CursorGrabMode::Locked));
        self.window.set_cursor_visible(false);
    }
    
    /// Unlock cursor (for UI)
    pub fn unlock_cursor(&self) {
        let _ = self.window.set_cursor_grab(CursorGrabMode::None);
        self.window.set_cursor_visible(true);
    }
    
    /// Set VSync mode (true for vsync, false for immediate/low latency)
    pub fn set_vsync(&mut self, vsync: bool) {
        let present_mode = if vsync {
            wgpu::PresentMode::AutoVsync
        } else {
            wgpu::PresentMode::Immediate
        };
        self.surface_config.present_mode = present_mode;
        self.surface.configure(&self.device, &self.surface_config);
    }
    
    /// Set cursor grab mode
    pub fn set_cursor_grab(&self, grab: bool) {
        if grab {
            self.lock_cursor();
        } else {
            self.unlock_cursor();
        }
    }
    
    /// Update image cache (call from main loop)
    pub fn update_image_cache(&self) {
        image_cache::update_cache();
    }
    
    /// Check if new images loaded (for repaint trigger)
    pub fn has_newly_loaded_images(&self) -> bool {
        image_cache::has_newly_loaded_images()
    }
    
    /// Clear loaded images flag
    pub fn clear_loaded_flag(&self) {
        image_cache::clear_loaded_flag();
    }
    
    /// Request image loading
    pub fn request_image(&self, url: &str, runtime: &tokio::runtime::Handle) {
        image_cache::request_image(url, runtime);
    }
}
