//! OpenNow Streamer - Native GeForce NOW Client
//!
//! A high-performance, cross-platform streaming client for GFN.

#![recursion_limit = "256"]

mod api;
mod app;
mod auth;
mod gui_iced;
mod input;
mod media;
mod profiling;
mod utils;
mod webrtc;

// Use iced-based GUI
use gui_iced as gui;

// Re-export profiling functions for use throughout the codebase
#[allow(unused_imports)]
pub use profiling::frame_mark;

use anyhow::Result;
use log::info;
use parking_lot::Mutex;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, KeyEvent, Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::WindowId;

use app::{App, AppState, UiAction, UserEvent};
use gui::Renderer;
use iced_winit::conversion;
use iced_winit::core::Event as IcedEvent;

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
    /// Track window focus state for CPU optimization
    /// When unfocused, we skip rendering entirely to save CPU
    window_focused: bool,
    /// Event loop proxy for decoder to signal new frames
    /// Passed to SharedFrame so decoder can wake event loop on frame write
    event_loop_proxy: EventLoopProxy<UserEvent>,
    /// Last frame time for menu UI rate limiting (60 FPS cap)
    /// Prevents 80%+ CPU from uncapped hover-triggered redraws
    last_menu_frame: std::time::Instant,
    /// Collected iced events to pass to render
    iced_events: Vec<IcedEvent>,
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
    fn new(runtime: tokio::runtime::Handle, event_loop_proxy: EventLoopProxy<UserEvent>) -> Self {
        let mut inner_app = App::new(runtime.clone());
        // Set the event loop proxy for push-based frame delivery
        inner_app.set_event_loop_proxy(event_loop_proxy.clone());
        let app = Arc::new(Mutex::new(inner_app));
        Self {
            runtime,
            app,
            renderer: None,
            modifiers: Modifiers::default(),
            was_streaming: false,
            window_focused: true, // Assume focused on startup
            event_loop_proxy,
            last_menu_frame: std::time::Instant::now(),
            iced_events: Vec::new(),
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

impl ApplicationHandler<UserEvent> for OpenNowApp {
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

        // Convert window event to iced event and collect for render
        let scale_factor = renderer.window().scale_factor() as f32;
        if let Some(iced_event) = conversion::window_event(
            event.clone(),
            scale_factor,
            self.modifiers.state(),
        ) {
            self.iced_events.push(iced_event);
        }

        // Let renderer handle events for cursor tracking, etc.
        let response = renderer.handle_event(&event);

        // Request redraw based on app state:
        // - When streaming: always honor egui repaint (low latency needed)
        // - When in session setup: always repaint (need to show progress updates)
        // - When in menus: throttle to 60 FPS to prevent 80%+ CPU from hover spam
        let app_state = self.app.lock().state;
        let should_repaint = match app_state {
            AppState::Streaming | AppState::Session => response.repaint,
            _ => {
                // Menu states: throttle redraws to 60 FPS max
                // egui returns repaint=true on every hover change, causing 80%+ CPU
                // at uncapped frame rate. Throttling to 60 FPS (~16.6ms) drops to ~5-10%
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(self.last_menu_frame);
                let min_frame_time = std::time::Duration::from_micros(16667); // 60 FPS
                
                // Always allow immediate response to clicks/keyboard/resize
                let is_immediate_event = matches!(
                    event,
                    WindowEvent::MouseInput { .. }
                        | WindowEvent::MouseWheel { .. }
                        | WindowEvent::KeyboardInput { .. }
                        | WindowEvent::Resized(_)
                        | WindowEvent::Focused(_)
                );
                
                if is_immediate_event || (response.repaint && elapsed >= min_frame_time) {
                    self.last_menu_frame = now;
                    true
                } else {
                    false
                }
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
                // Get borderless setting from app settings
                let use_borderless = {
                    let app = self.app.lock();
                    app.settings.borderless
                };
                renderer.toggle_fullscreen(use_borderless);
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
                // Track focus state for CPU optimization
                self.window_focused = focused;
                
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
                
                // Request redraw when regaining focus to refresh UI
                if focused {
                    renderer.window().request_redraw();
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
                let app_state = app_guard.state;

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

                // Gather data for rendering
                let games = app_guard.games.clone();
                let library_games = app_guard.library_games.clone();
                let game_sections = app_guard.game_sections.clone();
                let status_message = app_guard.status_message.clone();
                let user_name: Option<String> = app_guard.user_info.as_ref()
                    .map(|u| u.display_name.clone());
                let video_frame = app_guard.current_frame.take();
                let user_name_ref = user_name.as_deref();
                let servers = app_guard.servers.clone();
                let selected_server_index = app_guard.selected_server_index;
                let subscription = app_guard.subscription.clone();
                let show_settings = app_guard.show_settings_modal;
                let selected_game_popup = app_guard.selected_game_popup.clone();
                let show_session_conflict = app_guard.show_session_conflict;
                let show_av1_warning = app_guard.show_av1_warning;
                let show_alliance_warning = app_guard.show_alliance_warning;
                let show_welcome = app_guard.show_welcome_popup;
                let settings = app_guard.settings.clone();
                let show_stats = app_guard.show_stats;
                let stats = app_guard.stats.clone();
                let decoder_backend = app_guard.active_decoder_backend.clone();
                let login_providers = app_guard.login_providers.clone();
                let selected_provider_index = app_guard.selected_provider_index;
                
                // Take collected iced events
                let events: Vec<IcedEvent> = std::mem::take(&mut self.iced_events);
                
                // Render with iced
                match renderer.render(
                    app_state,
                    &games,
                    &library_games,
                    &game_sections,
                    &status_message,
                    user_name_ref,
                    &servers,
                    selected_server_index,
                    subscription.as_ref(),
                    video_frame.as_ref(),
                    &events,
                    show_settings,
                    selected_game_popup.as_ref(),
                    show_session_conflict,
                    show_av1_warning,
                    show_alliance_warning,
                    show_welcome,
                    &settings,
                    &self.runtime,
                    show_stats,
                    &stats,
                    &decoder_backend,
                    &login_providers,
                    selected_provider_index,
                ) {
                    Ok(actions) => {
                        // Apply UI actions to app state
                        for action in actions {
                            app_guard.handle_action(action);
                        }

                        // === CPU OPTIMIZATION: No WaitUntil scheduling in UI states ===
                        // Previously we used repaint_after to schedule WaitUntil, causing
                        // continuous polling even when idle. Now we use pure event-driven:
                        // - ControlFlow::Wait blocks until OS event (mouse, keyboard, etc.)
                        // - VSync (Fifo) handles frame pacing when we do render
                        // - CPU drops from ~80% to <1% when idle
                        //
                        // The only exception is Session state (spinner animation) when focused
                        if !is_streaming && app_state != AppState::Session {
                            // Login/Games: Pure event-driven, no polling
                            event_loop.set_control_flow(ControlFlow::Wait);
                        }
                        // Session state and Streaming handled in about_to_wait
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

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        // Handle custom events from decoder thread
        match event {
            UserEvent::FrameReady => {
                // New frame available from decoder - request redraw immediately
                // This is the push-based notification that eliminates polling
                if let Some(ref renderer) = self.renderer {
                    renderer.window().request_redraw();
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

        // === CPU OPTIMIZATION: Pure event-driven rendering when unfocused ===
        // When the window is not focused and we're not streaming, use ControlFlow::Wait
        // This completely suspends the thread until an OS event arrives, dropping CPU to ~0%
        if !self.window_focused && app_state != AppState::Streaming {
            _event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }

        // Dynamically switch control flow based on app state
        // === PUSH-BASED FRAME DELIVERY ===
        // During streaming, the decoder sends UserEvent::FrameReady via EventLoopProxy
        // This wakes the event loop immediately when frames arrive, eliminating polling
        // CPU usage drops from ~30% (4ms polling) to ~5-10% (event-driven)
        match app_state {
            AppState::Streaming => {
                if has_new_frame {
                    renderer.window().request_redraw();
                }
                // Use pure event-driven mode - decoder will wake us via UserEvent::FrameReady
                // No polling needed - this is the key CPU optimization
                _event_loop.set_control_flow(ControlFlow::Wait);
            }
            AppState::Session => {
                // During session setup, poll at 30 FPS for spinner animation (only when focused)
                let wake_time = std::time::Instant::now() + std::time::Duration::from_millis(33);
                _event_loop.set_control_flow(ControlFlow::WaitUntil(wake_time));
                renderer.window().request_redraw();
            }
            _ => {
                // === KEY OPTIMIZATION: Pure event-driven mode for Login/Games ===
                // Use ControlFlow::Wait - CPU sleeps until user interaction
                // VSync (Fifo) handles frame pacing when we do render
                // This drops CPU from ~80% to <1% when idle
                _event_loop.set_control_flow(ControlFlow::Wait);
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

    // Create event loop with user event support for cross-thread frame notifications
    // This allows the decoder to wake the event loop when frames are ready
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    
    // Create proxy for decoder to signal new frames
    let event_loop_proxy = event_loop.create_proxy();
    
    // Use Wait by default for low CPU usage in menus
    // Dynamically switch to Poll during active streaming for lowest latency
    event_loop.set_control_flow(ControlFlow::Wait);

    // Create application handler with event loop proxy
    let mut app = OpenNowApp::new(runtime.handle().clone(), event_loop_proxy);

    // Run event loop with application handler
    event_loop.run_app(&mut app)?;

    Ok(())
}
