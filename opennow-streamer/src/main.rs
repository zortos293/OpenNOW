//! OpenNow Streamer - Native GeForce NOW Client
//!
//! A high-performance, cross-platform streaming client for GFN.

#![recursion_limit = "256"]

mod api;
mod app;
mod auth;
mod gui;
mod input;
mod media;
mod profiling;
mod utils;
mod webrtc;
mod znow;

// Re-export profiling functions for use throughout the codebase
#[allow(unused_imports)]
pub use profiling::frame_mark;

use anyhow::Result;
use log::info;
use parking_lot::Mutex;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, KeyEvent, Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::WindowId;

use app::{App, AppState, UiAction};
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
}

/// Convert winit KeyCode to Windows Virtual Key code
fn keycode_to_vk(key: PhysicalKey) -> u16 {
    match key {
        PhysicalKey::Code(code) => match code {
            // Letters
            KeyCode::KeyA => 0x41,
            KeyCode::KeyB => 0x42,
            KeyCode::KeyC => 0x43,
            KeyCode::KeyD => 0x44,
            KeyCode::KeyE => 0x45,
            KeyCode::KeyF => 0x46,
            KeyCode::KeyG => 0x47,
            KeyCode::KeyH => 0x48,
            KeyCode::KeyI => 0x49,
            KeyCode::KeyJ => 0x4A,
            KeyCode::KeyK => 0x4B,
            KeyCode::KeyL => 0x4C,
            KeyCode::KeyM => 0x4D,
            KeyCode::KeyN => 0x4E,
            KeyCode::KeyO => 0x4F,
            KeyCode::KeyP => 0x50,
            KeyCode::KeyQ => 0x51,
            KeyCode::KeyR => 0x52,
            KeyCode::KeyS => 0x53,
            KeyCode::KeyT => 0x54,
            KeyCode::KeyU => 0x55,
            KeyCode::KeyV => 0x56,
            KeyCode::KeyW => 0x57,
            KeyCode::KeyX => 0x58,
            KeyCode::KeyY => 0x59,
            KeyCode::KeyZ => 0x5A,
            // Numbers
            KeyCode::Digit1 => 0x31,
            KeyCode::Digit2 => 0x32,
            KeyCode::Digit3 => 0x33,
            KeyCode::Digit4 => 0x34,
            KeyCode::Digit5 => 0x35,
            KeyCode::Digit6 => 0x36,
            KeyCode::Digit7 => 0x37,
            KeyCode::Digit8 => 0x38,
            KeyCode::Digit9 => 0x39,
            KeyCode::Digit0 => 0x30,
            // Function keys
            KeyCode::F1 => 0x70,
            KeyCode::F2 => 0x71,
            KeyCode::F3 => 0x72,
            KeyCode::F4 => 0x73,
            KeyCode::F5 => 0x74,
            KeyCode::F6 => 0x75,
            KeyCode::F7 => 0x76,
            KeyCode::F8 => 0x77,
            KeyCode::F9 => 0x78,
            KeyCode::F10 => 0x79,
            KeyCode::F11 => 0x7A,
            KeyCode::F12 => 0x7B,
            // Special keys
            KeyCode::Escape => 0x1B,
            KeyCode::Tab => 0x09,
            KeyCode::CapsLock => 0x14,
            KeyCode::ShiftLeft => 0xA0,
            KeyCode::ShiftRight => 0xA1,
            KeyCode::ControlLeft => 0xA2,
            KeyCode::ControlRight => 0xA3,
            KeyCode::AltLeft => 0xA4,
            KeyCode::AltRight => 0xA5,
            KeyCode::SuperLeft => 0x5B,
            KeyCode::SuperRight => 0x5C,
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
            KeyCode::Numpad0 => 0x60,
            KeyCode::Numpad1 => 0x61,
            KeyCode::Numpad2 => 0x62,
            KeyCode::Numpad3 => 0x63,
            KeyCode::Numpad4 => 0x64,
            KeyCode::Numpad5 => 0x65,
            KeyCode::Numpad6 => 0x66,
            KeyCode::Numpad7 => 0x67,
            KeyCode::Numpad8 => 0x68,
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
        }
    }

    /// Get GFN modifier flags from current modifier state
    fn get_modifier_flags(&self) -> u16 {
        let state = self.modifiers.state();
        let mut flags = 0u16;
        if state.shift_key() {
            flags |= 0x01;
        } // GFN_MOD_SHIFT
        if state.control_key() {
            flags |= 0x02;
        } // GFN_MOD_CTRL
        if state.alt_key() {
            flags |= 0x04;
        } // GFN_MOD_ALT
        if state.super_key() {
            flags |= 0x08;
        } // GFN_MOD_META
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

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };

        // Let egui handle events first
        let response = renderer.handle_event(&event);

        // Request redraw based on app state:
        // - When streaming: always honor egui repaint (low latency needed)
        // - When in session setup: always repaint (need to show progress updates)
        // - When not streaming: only repaint on actual user interaction events
        //   (egui's request_repaint_after handles timed repaints via ControlFlow)
        let app_state = self.app.lock().state;
        let should_repaint = match app_state {
            AppState::Streaming | AppState::Session => response.repaint,
            _ => {
                // Only repaint on actual input events, not egui's internal repaint requests
                matches!(
                    event,
                    WindowEvent::MouseInput { .. }
                        | WindowEvent::MouseWheel { .. }
                        | WindowEvent::KeyboardInput { .. }
                        | WindowEvent::CursorMoved { .. }
                        | WindowEvent::Resized(_)
                        | WindowEvent::Focused(_)
                )
            }
        };

        if should_repaint {
            renderer.window().request_redraw();
        }

        match event {
            WindowEvent::CloseRequested => {
                info!("Window close requested");
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                renderer.resize(size);
                // Save window size to settings (only when not fullscreen)
                if !renderer.is_fullscreen() && size.width > 0 && size.height > 0 {
                    let mut app = self.app.lock();
                    app.handle_action(UiAction::UpdateWindowSize(size.width, size.height));
                }
            }
            // Ctrl+Shift+Q to stop streaming (instead of ESC to avoid accidental stops)
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
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
                event:
                    KeyEvent {
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
                event:
                    KeyEvent {
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
                event:
                    KeyEvent {
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
            // F8 to toggle mouse lock during streaming (for windowed mode)
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::F8),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                let mut app = self.app.lock();
                if app.state == AppState::Streaming {
                    // Toggle cursor capture state
                    app.cursor_captured = !app.cursor_captured;

                    if app.cursor_captured {
                        renderer.lock_cursor();
                        // Resume raw input when locking
                        #[cfg(any(target_os = "windows", target_os = "macos"))]
                        input::resume_raw_input();
                        info!("F8: Mouse locked");
                    } else {
                        renderer.unlock_cursor();
                        // Pause raw input when unlocking
                        #[cfg(any(target_os = "windows", target_os = "macos"))]
                        input::pause_raw_input();
                        info!("F8: Mouse unlocked");
                    }
                }
            }
            // Ctrl+V to paste clipboard text into remote session
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(KeyCode::KeyV),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } if self.modifiers.state().control_key() && !self.modifiers.state().shift_key() => {
                let app = self.app.lock();
                if app.state == AppState::Streaming && app.settings.clipboard_paste_enabled {
                    if let Some(ref input_handler) = app.input_handler {
                        info!("Ctrl+V pressed - pasting clipboard to remote session");
                        let char_count = input_handler.handle_clipboard_paste();
                        if char_count > 0 {
                            info!("Pasted {} characters to remote session", char_count);
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers;
            }
            WindowEvent::KeyboardInput { event, .. } => {
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
                            PhysicalKey::Code(KeyCode::ShiftLeft)
                                | PhysicalKey::Code(KeyCode::ShiftRight)
                                | PhysicalKey::Code(KeyCode::ControlLeft)
                                | PhysicalKey::Code(KeyCode::ControlRight)
                                | PhysicalKey::Code(KeyCode::AltLeft)
                                | PhysicalKey::Code(KeyCode::AltRight)
                                | PhysicalKey::Code(KeyCode::SuperLeft)
                                | PhysicalKey::Code(KeyCode::SuperRight)
                        );
                        let modifiers = if is_modifier_key {
                            0
                        } else {
                            self.get_modifier_flags()
                        };

                        // Only send if we have a valid VK code
                        if vk_code != 0 {
                            input_handler.handle_key(vk_code, pressed, modifiers);
                        }
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                let mut app = self.app.lock();
                if app.state == AppState::Streaming {
                    if !focused {
                        // Lost focus - release all keys to prevent sticky keys
                        if let Some(ref input_handler) = app.input_handler {
                            log::info!("Window lost focus - releasing all keys");
                            input_handler.release_all_keys();
                        }
                        // Pause raw input while unfocused
                        #[cfg(any(target_os = "windows", target_os = "macos"))]
                        input::pause_raw_input();
                    } else {
                        // Regained focus - re-lock cursor if it was captured
                        if app.cursor_captured {
                            log::info!("Window regained focus - re-locking cursor");
                            renderer.lock_cursor();
                            // Resume raw input
                            #[cfg(any(target_os = "windows", target_os = "macos"))]
                            input::resume_raw_input();

                            // Request keyframe to recover video stream after focus loss
                            // This prevents freeze caused by corrupted NAL data during unfocused state
                            let runtime = self.runtime.clone();
                            runtime.spawn(async {
                                log::info!("Requesting keyframe after focus regain");
                                webrtc::request_keyframe().await;
                            });
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
                // Mark frame for Tracy profiler (if enabled)
                profiling::frame_mark();

                let mut app_guard = self.app.lock();
                let is_streaming = app_guard.state == AppState::Streaming;

                // Check for streaming state change to lock/unlock cursor and start/stop raw input
                if is_streaming && !self.was_streaming {
                    // Just started streaming - lock cursor, start raw input, disable vsync
                    renderer.lock_cursor();
                    renderer.set_vsync(false); // Immediate mode for lowest latency
                    self.was_streaming = true;

                    // Start Raw Input for unaccelerated mouse movement (Windows/macOS)
                    #[cfg(any(target_os = "windows", target_os = "macos"))]
                    {
                        match input::start_raw_input() {
                            Ok(()) => info!("Raw input enabled - mouse acceleration disabled"),
                            Err(e) => log::warn!(
                                "Failed to start raw input: {} - using winit fallback",
                                e
                            ),
                        }
                    }
                } else if !is_streaming && self.was_streaming {
                    // Just stopped streaming - unlock cursor, stop raw input, enable vsync
                    renderer.unlock_cursor();
                    renderer.set_vsync(true); // VSync for low CPU usage in UI
                    self.was_streaming = false;

                    // Stop raw input
                    #[cfg(any(target_os = "windows", target_os = "macos"))]
                    {
                        input::stop_raw_input();
                    }
                }

                app_guard.update();

                match renderer.render(&app_guard) {
                    Ok((actions, repaint_after)) => {
                        // Apply UI actions to app state
                        for action in actions {
                            app_guard.handle_action(action);
                        }

                        // Schedule next repaint based on egui's request
                        // This enables idle throttling (e.g., 10 FPS when not interacting)
                        if !is_streaming {
                            if let Some(delay) = repaint_after {
                                if !delay.is_zero() {
                                    // Schedule a repaint after the delay
                                    let wake_time = std::time::Instant::now() + delay;
                                    event_loop.set_control_flow(
                                        winit::event_loop::ControlFlow::WaitUntil(wake_time),
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Render error: {}", e);
                    }
                }

                drop(app_guard);

                // Don't request redraw here - let about_to_wait handle frame pacing
                // This ensures render rate matches decode rate (e.g., 120fps)
                // Previously this caused double the frame rate (240+ fps) because
                // both RedrawRequested and about_to_wait were requesting redraws
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
            // Drag & drop file transfer
            WindowEvent::DroppedFile(path) => {
                let app_state = {
                    let app = self.app.lock();
                    app.state
                };
                // Only accept file drops during streaming (when connected to GFN session)
                if app_state == AppState::Streaming {
                    info!("File dropped: {:?}", path);
                    let mut app = self.app.lock();
                    app.handle_action(UiAction::FileDropped(path));
                }
            }
            WindowEvent::HoveredFile(path) => {
                let app_state = {
                    let app = self.app.lock();
                    app.state
                };
                if app_state == AppState::Streaming {
                    debug!("File hovering: {:?}", path);
                }
            }
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
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
        let Some(ref renderer) = self.renderer else {
            return;
        };

        let app_guard = self.app.lock();
        let app_state = app_guard.state;
        // Check if there's a new frame from the decoder before requesting redraw
        // This prevents rendering faster than decode rate, saving GPU cycles
        let has_new_frame = app_guard
            .shared_frame
            .as_ref()
            .map(|sf| sf.has_new_frame())
            .unwrap_or(false);
        drop(app_guard);

        // Dynamically switch control flow based on app state
        // Poll during streaming for lowest latency, Wait for menus to save CPU
        match app_state {
            AppState::Streaming => {
                _event_loop.set_control_flow(ControlFlow::Poll);
                // Only request redraw when decoder has produced a new frame
                // This synchronizes render rate to decode rate, avoiding wasted GPU cycles
                if has_new_frame {
                    renderer.window().request_redraw();
                }
            }
            AppState::Session => {
                // During session setup, poll at a reasonable rate (30 FPS) to show progress
                // This ensures status updates are visible without wasting CPU
                let wake_time = std::time::Instant::now() + std::time::Duration::from_millis(33);
                _event_loop.set_control_flow(ControlFlow::WaitUntil(wake_time));
                renderer.window().request_redraw();
            }
            _ => {
                _event_loop.set_control_flow(ControlFlow::Wait);
                // When not streaming, rely entirely on event-driven redraws
                // ControlFlow::Wait will block until an event arrives
                // This reduces CPU usage from 100% to <5% when idle
            }
        }
    }
}

fn main() -> Result<()> {
    // Initialize profiling (Tracy) if enabled
    // Build with: cargo build --release --features tracy
    // Returns true if it initialized logging (we should skip env_logger)
    let profiling_initialized_logging = profiling::init();

    // Initialize logging (only if profiling didn't already set it up)
    if !profiling_initialized_logging {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    info!("OpenNow Streamer v{}", env!("CARGO_PKG_VERSION"));
    info!("Platform: {}", std::env::consts::OS);

    #[cfg(feature = "tracy")]
    info!("Tracy profiler ENABLED - connect with Tracy Profiler application");

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
