//! OpenNow Streamer - Native GeForce NOW Client
//!
//! A high-performance, cross-platform streaming client for GFN.

#![recursion_limit = "256"]

mod app;
mod api;
mod auth;
mod gui;
mod input;
mod media;
mod webrtc;
mod utils;

use anyhow::Result;
use log::info;
use std::sync::Arc;
use parking_lot::Mutex;
use winit::application::ApplicationHandler;
use winit::event_loop::{ControlFlow, EventLoop, ActiveEventLoop};
use winit::event::{WindowEvent, KeyEvent, ElementState, DeviceEvent, DeviceId, Modifiers};
use winit::keyboard::{Key, NamedKey, PhysicalKey, KeyCode};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::WindowId;

use app::{App, AppState};
use gui::Renderer;

/// Application handler for winit 0.30+
struct OpenNowApp {
    /// Tokio runtime handle
    runtime: tokio::runtime::Handle,
    /// Application state (shared)
    app: Arc<Mutex<App>>,
    /// Renderer (created after window is available)
    renderer: Option<Renderer>,
    /// Current modifier state
    modifiers: Modifiers,
    /// Track if we were streaming (for cursor lock state changes)
    was_streaming: bool,
    /// Last frame time for frame rate limiting
    last_frame_time: std::time::Instant,
}

/// Convert winit KeyCode to Windows Virtual Key code
fn keycode_to_vk(key: PhysicalKey) -> u16 {
    match key {
        PhysicalKey::Code(code) => match code {
            // Letters
            KeyCode::KeyA => 0x41, KeyCode::KeyB => 0x42, KeyCode::KeyC => 0x43,
            KeyCode::KeyD => 0x44, KeyCode::KeyE => 0x45, KeyCode::KeyF => 0x46,
            KeyCode::KeyG => 0x47, KeyCode::KeyH => 0x48, KeyCode::KeyI => 0x49,
            KeyCode::KeyJ => 0x4A, KeyCode::KeyK => 0x4B, KeyCode::KeyL => 0x4C,
            KeyCode::KeyM => 0x4D, KeyCode::KeyN => 0x4E, KeyCode::KeyO => 0x4F,
            KeyCode::KeyP => 0x50, KeyCode::KeyQ => 0x51, KeyCode::KeyR => 0x52,
            KeyCode::KeyS => 0x53, KeyCode::KeyT => 0x54, KeyCode::KeyU => 0x55,
            KeyCode::KeyV => 0x56, KeyCode::KeyW => 0x57, KeyCode::KeyX => 0x58,
            KeyCode::KeyY => 0x59, KeyCode::KeyZ => 0x5A,
            // Numbers
            KeyCode::Digit1 => 0x31, KeyCode::Digit2 => 0x32, KeyCode::Digit3 => 0x33,
            KeyCode::Digit4 => 0x34, KeyCode::Digit5 => 0x35, KeyCode::Digit6 => 0x36,
            KeyCode::Digit7 => 0x37, KeyCode::Digit8 => 0x38, KeyCode::Digit9 => 0x39,
            KeyCode::Digit0 => 0x30,
            // Function keys
            KeyCode::F1 => 0x70, KeyCode::F2 => 0x71, KeyCode::F3 => 0x72,
            KeyCode::F4 => 0x73, KeyCode::F5 => 0x74, KeyCode::F6 => 0x75,
            KeyCode::F7 => 0x76, KeyCode::F8 => 0x77, KeyCode::F9 => 0x78,
            KeyCode::F10 => 0x79, KeyCode::F11 => 0x7A, KeyCode::F12 => 0x7B,
            // Special keys
            KeyCode::Escape => 0x1B,
            KeyCode::Tab => 0x09,
            KeyCode::CapsLock => 0x14,
            KeyCode::ShiftLeft => 0xA0, KeyCode::ShiftRight => 0xA1,
            KeyCode::ControlLeft => 0xA2, KeyCode::ControlRight => 0xA3,
            KeyCode::AltLeft => 0xA4, KeyCode::AltRight => 0xA5,
            KeyCode::SuperLeft => 0x5B, KeyCode::SuperRight => 0x5C,
            KeyCode::Space => 0x20,
            KeyCode::Enter => 0x0D,
            KeyCode::Backspace => 0x08,
            KeyCode::Delete => 0x2E,
            KeyCode::Insert => 0x2D,
            KeyCode::Home => 0x24,
            KeyCode::End => 0x23,
            KeyCode::PageUp => 0x21,
            KeyCode::PageDown => 0x22,
            // Arrow keys
            KeyCode::ArrowUp => 0x26,
            KeyCode::ArrowDown => 0x28,
            KeyCode::ArrowLeft => 0x25,
            KeyCode::ArrowRight => 0x27,
            // Numpad
            KeyCode::Numpad0 => 0x60, KeyCode::Numpad1 => 0x61, KeyCode::Numpad2 => 0x62,
            KeyCode::Numpad3 => 0x63, KeyCode::Numpad4 => 0x64, KeyCode::Numpad5 => 0x65,
            KeyCode::Numpad6 => 0x66, KeyCode::Numpad7 => 0x67, KeyCode::Numpad8 => 0x68,
            KeyCode::Numpad9 => 0x69,
            KeyCode::NumpadAdd => 0x6B,
            KeyCode::NumpadSubtract => 0x6D,
            KeyCode::NumpadMultiply => 0x6A,
            KeyCode::NumpadDivide => 0x6F,
            KeyCode::NumpadDecimal => 0x6E,
            KeyCode::NumpadEnter => 0x0D,
            KeyCode::NumLock => 0x90,
            // Punctuation
            KeyCode::Minus => 0xBD,
            KeyCode::Equal => 0xBB,
            KeyCode::BracketLeft => 0xDB,
            KeyCode::BracketRight => 0xDD,
            KeyCode::Backslash => 0xDC,
            KeyCode::Semicolon => 0xBA,
            KeyCode::Quote => 0xDE,
            KeyCode::Backquote => 0xC0,
            KeyCode::Comma => 0xBC,
            KeyCode::Period => 0xBE,
            KeyCode::Slash => 0xBF,
            KeyCode::ScrollLock => 0x91,
            KeyCode::Pause => 0x13,
            KeyCode::PrintScreen => 0x2C,
            _ => 0,
        },
        PhysicalKey::Unidentified(_) => 0,
    }
}

impl OpenNowApp {
    fn new(runtime: tokio::runtime::Handle) -> Self {
        let app = Arc::new(Mutex::new(App::new(runtime.clone())));
        Self {
            runtime,
            app,
            renderer: None,
            modifiers: Modifiers::default(),
            was_streaming: false,
            last_frame_time: std::time::Instant::now(),
        }
    }

    /// Get GFN modifier flags from current modifier state
    fn get_modifier_flags(&self) -> u16 {
        let state = self.modifiers.state();
        let mut flags = 0u16;
        if state.shift_key() { flags |= 0x01; }  // GFN_MOD_SHIFT
        if state.control_key() { flags |= 0x02; } // GFN_MOD_CTRL
        if state.alt_key() { flags |= 0x04; }     // GFN_MOD_ALT
        if state.super_key() { flags |= 0x08; }   // GFN_MOD_META
        flags
    }
}

impl ApplicationHandler for OpenNowApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Create renderer when window is available
        if self.renderer.is_none() {
            info!("Creating renderer...");
            match pollster::block_on(Renderer::new(event_loop)) {
                Ok(renderer) => {
                    info!("Renderer initialized");
                    self.renderer = Some(renderer);
                }
                Err(e) => {
                    log::error!("Failed to create renderer: {}", e);
                    event_loop.exit();
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _window_id: WindowId, event: WindowEvent) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };

        // Let egui handle events first
        let _ = renderer.handle_event(&event);

        match event {
            WindowEvent::CloseRequested => {
                info!("Window close requested");
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                renderer.resize(size);
            }
            // Ctrl+Shift+Q to stop streaming (instead of ESC to avoid accidental stops)
            WindowEvent::KeyboardInput {
                event: KeyEvent {
                    physical_key: PhysicalKey::Code(KeyCode::KeyQ),
                    state: ElementState::Pressed,
                    ..
                },
                ..
            } if self.modifiers.state().control_key() && self.modifiers.state().shift_key() => {
                let mut app = self.app.lock();
                if app.state == AppState::Streaming {
                    info!("Ctrl+Shift+Q pressed - terminating session");
                    app.terminate_current_session();
                }
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent {
                    logical_key: Key::Named(NamedKey::F11),
                    state: ElementState::Pressed,
                    ..
                },
                ..
            } => {
                renderer.toggle_fullscreen();
                // Lock cursor when entering fullscreen during streaming
                let app = self.app.lock();
                if app.state == AppState::Streaming {
                    if renderer.is_fullscreen() {
                        renderer.lock_cursor();
                    } else {
                        renderer.unlock_cursor();
                    }
                }
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent {
                    logical_key: Key::Named(NamedKey::F3),
                    state: ElementState::Pressed,
                    ..
                },
                ..
            } => {
                let mut app = self.app.lock();
                app.toggle_stats();
            }
            // Ctrl+Shift+F10 to toggle anti-AFK mode
            WindowEvent::KeyboardInput {
                event: KeyEvent {
                    physical_key: PhysicalKey::Code(KeyCode::F10),
                    state: ElementState::Pressed,
                    ..
                },
                ..
            } if self.modifiers.state().control_key() && self.modifiers.state().shift_key() => {
                let mut app = self.app.lock();
                if app.state == AppState::Streaming {
                    app.toggle_anti_afk();
                }
            }
            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers;
            }
            WindowEvent::KeyboardInput {
                event,
                ..
            } => {
                // Forward keyboard input to InputHandler when streaming
                let app = self.app.lock();
                if app.state == AppState::Streaming && app.cursor_captured {
                    // Skip key repeat events (they cause sticky keys)
                    if event.repeat {
                        return;
                    }

                    if let Some(ref input_handler) = app.input_handler {
                        // Convert to Windows VK code (GFN expects VK codes, not scancodes)
                        let vk_code = keycode_to_vk(event.physical_key);
                        let pressed = event.state == ElementState::Pressed;

                        // Don't include modifier flags when the key itself is a modifier
                        let is_modifier_key = matches!(
                            event.physical_key,
                            PhysicalKey::Code(KeyCode::ShiftLeft) |
                            PhysicalKey::Code(KeyCode::ShiftRight) |
                            PhysicalKey::Code(KeyCode::ControlLeft) |
                            PhysicalKey::Code(KeyCode::ControlRight) |
                            PhysicalKey::Code(KeyCode::AltLeft) |
                            PhysicalKey::Code(KeyCode::AltRight) |
                            PhysicalKey::Code(KeyCode::SuperLeft) |
                            PhysicalKey::Code(KeyCode::SuperRight)
                        );
                        let modifiers = if is_modifier_key { 0 } else { self.get_modifier_flags() };

                        // Only send if we have a valid VK code
                        if vk_code != 0 {
                            input_handler.handle_key(vk_code, pressed, modifiers);
                        }
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                // Release all keys when focus is lost to prevent sticky keys
                if !focused {
                    let app = self.app.lock();
                    if app.state == AppState::Streaming {
                        if let Some(ref input_handler) = app.input_handler {
                            log::info!("Window lost focus - releasing all keys");
                            input_handler.release_all_keys();
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let app = self.app.lock();
                if app.state == AppState::Streaming {
                    if let Some(ref input_handler) = app.input_handler {
                        let wheel_delta = match delta {
                            winit::event::MouseScrollDelta::LineDelta(_, y) => (y * 120.0) as i16,
                            winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y as i16,
                        };
                        input_handler.handle_wheel(wheel_delta);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                // Frame rate limiting - sync to stream target FPS when streaming
                let mut app_guard = self.app.lock();
                let target_fps = if app_guard.state == AppState::Streaming {
                    app_guard.stats.target_fps.max(60) // Use stream's target FPS (min 60)
                } else {
                    // UI mode: Sync to monitor refresh rate (default 60 if detection fails)
                    renderer.window().current_monitor()
                        .and_then(|m| m.refresh_rate_millihertz())
                        .map(|mhz| (mhz as f32 / 1000.0).ceil() as u32)
                        .unwrap_or(60)
                        .max(30) // Ensure at least 30 FPS to avoid division by zero
                };
                drop(app_guard);

                let frame_duration = std::time::Duration::from_secs_f64(1.0 / target_fps.max(1) as f64);
                let elapsed = self.last_frame_time.elapsed();
                if elapsed < frame_duration {
                    // Sleep for remaining time (avoid busy loop)
                    let sleep_time = frame_duration - elapsed;
                    if sleep_time.as_micros() > 500 {
                        std::thread::sleep(sleep_time - std::time::Duration::from_micros(500));
                    }
                }
                self.last_frame_time = std::time::Instant::now();

                let mut app_guard = self.app.lock();
                app_guard.update();

                // Check for streaming state change to lock/unlock cursor and start/stop raw input
                let is_streaming = app_guard.state == AppState::Streaming;
                if is_streaming && !self.was_streaming {
                    // Just started streaming - lock cursor and start raw input
                    renderer.lock_cursor();
                    self.was_streaming = true;

                    // Start Raw Input for unaccelerated mouse movement (Windows/macOS)
                    #[cfg(any(target_os = "windows", target_os = "macos"))]
                    {
                        match input::start_raw_input() {
                            Ok(()) => info!("Raw input enabled - mouse acceleration disabled"),
                            Err(e) => log::warn!("Failed to start raw input: {} - using winit fallback", e),
                        }
                    }
                } else if !is_streaming && self.was_streaming {
                    // Just stopped streaming - unlock cursor and stop raw input
                    renderer.unlock_cursor();
                    self.was_streaming = false;

                    // Stop raw input
                    #[cfg(any(target_os = "windows", target_os = "macos"))]
                    {
                        input::stop_raw_input();
                    }
                }

                match renderer.render(&app_guard) {
                    Ok(actions) => {
                        // Apply UI actions to app state
                        for action in actions {
                            app_guard.handle_action(action);
                        }
                    }
                    Err(e) => {
                        log::error!("Render error: {}", e);
                    }
                }

                drop(app_guard);
                renderer.window().request_redraw();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let app = self.app.lock();
                if app.state == AppState::Streaming {
                    if let Some(ref input_handler) = app.input_handler {
                        input_handler.handle_mouse_button(button, state);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let app = self.app.lock();
                if app.state == AppState::Streaming {
                    if let Some(ref input_handler) = app.input_handler {
                        input_handler.handle_cursor_move(position.x, position.y);
                    }
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _device_id: DeviceId, event: DeviceEvent) {
        // Only use winit's MouseMotion as fallback when raw input is not active
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if input::is_raw_input_active() {
            return; // Raw input handles mouse movement
        }

        if let DeviceEvent::MouseMotion { delta } = event {
            let app = self.app.lock();
            if app.state == AppState::Streaming && app.cursor_captured {
                if let Some(ref input_handler) = app.input_handler {
                    input_handler.handle_mouse_delta(delta.0 as i16, delta.1 as i16);
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        let Some(ref mut renderer) = self.renderer else { return };

        let mut app_guard = self.app.lock();
        let is_streaming = app_guard.state == AppState::Streaming;

        // Dynamically switch control flow based on streaming state
        // Poll during streaming for lowest latency, Wait for menus to save CPU
        if is_streaming {
            _event_loop.set_control_flow(ControlFlow::Poll);
        } else {
            _event_loop.set_control_flow(ControlFlow::Wait);
        }

        if is_streaming {
            // NOTE: Mouse input is handled directly by the raw input thread via set_raw_input_sender()
            // No polling needed here - raw input sends directly to the WebRTC input channel
            // This keeps mouse latency minimal and independent of render rate

            // Check if there's a new frame available BEFORE sleeping
            // This minimizes latency by rendering new frames immediately
            let has_new_frame = app_guard.shared_frame.as_ref()
                .map(|sf| sf.has_new_frame())
                .unwrap_or(false);

            let target_fps = app_guard.stats.target_fps.max(60);
            drop(app_guard); // Release lock before potential sleep

            // Only sleep if no new frame is available
            // This ensures frames are rendered as soon as they arrive
            if !has_new_frame {
                let frame_duration = std::time::Duration::from_secs_f64(1.0 / target_fps.max(1) as f64);
                let elapsed = self.last_frame_time.elapsed();
                if elapsed < frame_duration {
                    let sleep_time = frame_duration - elapsed;
                    // Use shorter sleep (1ms max) to stay responsive to new frames
                    let max_sleep = std::time::Duration::from_millis(1);
                    let actual_sleep = sleep_time.min(max_sleep);
                    if actual_sleep.as_micros() > 200 {
                        std::thread::sleep(actual_sleep);
                    }
                }
            }
            self.last_frame_time = std::time::Instant::now();

            // Re-acquire lock for update and render
            let mut app_guard = self.app.lock();
            app_guard.update();

            match renderer.render(&app_guard) {
                Ok(actions) => {
                    for action in actions {
                        app_guard.handle_action(action);
                    }
                }
                Err(e) => {
                    // Surface errors are normal during resize, just log at debug
                    log::debug!("Render error: {}", e);
                }
            }
        } else {
            // Non-streaming: ControlFlow::Wait handles wakeup on input events
            // Redraws are triggered by window events (mouse, keyboard, resize, etc.)
            // No need to request_redraw here - that would create a continuous loop
            drop(app_guard);
        }
    }
}

fn main() -> Result<()> {
    // Initialize logging
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info")
    ).init();

    info!("OpenNow Streamer v{}", env!("CARGO_PKG_VERSION"));
    info!("Platform: {}", std::env::consts::OS);

    // Create tokio runtime for async operations
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Create event loop
    let event_loop = EventLoop::new()?;
    // Use Wait by default for low CPU usage in menus
    // Dynamically switch to Poll during active streaming for lowest latency
    event_loop.set_control_flow(ControlFlow::Wait);

    // Create application handler
    let mut app = OpenNowApp::new(runtime.handle().clone());

    // Run event loop with application handler
    event_loop.run_app(&mut app)?;

    Ok(())
}
