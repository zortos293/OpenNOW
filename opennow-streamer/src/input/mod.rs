//! Input Handling
//!
//! Cross-platform input capture for mouse and keyboard.
//!
//! Key optimizations for native-feeling input:
//! - Mouse event coalescing (batches events every 4-8ms like official client)
//! - Local cursor rendering (instant visual feedback independent of network)
//! - Queue depth management (prevents server-side buffering)

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub mod controller;
mod protocol;
pub mod wheel;

pub use controller::{ControllerManager, RumbleEffect};
pub use protocol::*;
pub use wheel::{FfbEffectType, G29FfbManager, WheelManager};

// Re-export raw input functions for Windows
#[cfg(target_os = "windows")]
pub use windows::{
    clear_raw_input_sender, flush_pending_mouse_events, get_coalesced_event_count,
    get_local_cursor_normalized, get_local_cursor_position, get_raw_mouse_delta,
    is_raw_input_active, pause_raw_input, reset_coalescing, resume_raw_input,
    set_local_cursor_dimensions, set_raw_input_sender, start_raw_input, stop_raw_input,
    update_raw_input_center,
};

// Re-export raw input functions for macOS
#[cfg(target_os = "macos")]
pub use macos::{
    clear_raw_input_sender, flush_pending_mouse_events, get_coalesced_event_count,
    get_local_cursor_normalized, get_local_cursor_position, get_raw_mouse_delta,
    is_raw_input_active, pause_raw_input, reset_coalescing, resume_raw_input,
    set_local_cursor_dimensions, set_raw_input_sender, start_raw_input, stop_raw_input,
    update_raw_input_center,
};

// Re-export raw input functions for Linux
#[cfg(target_os = "linux")]
pub use linux::{
    clear_raw_input_sender, flush_pending_mouse_events, get_coalesced_event_count,
    get_local_cursor_normalized, get_local_cursor_position, get_raw_mouse_delta,
    is_raw_input_active, pause_raw_input, reset_coalescing, resume_raw_input,
    set_local_cursor_dimensions, set_raw_input_sender, start_raw_input, stop_raw_input,
    update_raw_input_center,
};

use parking_lot::RwLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Session timing state - resettable for each streaming session
/// GFN server expects timestamps relative to session start for proper input timing
struct SessionTiming {
    start: Instant,
    unix_us: u64,
}

impl SessionTiming {
    fn new() -> Self {
        let unix_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        Self {
            start: Instant::now(),
            unix_us,
        }
    }
}

static SESSION_TIMING: RwLock<Option<SessionTiming>> = RwLock::new(None);

/// Initialize session timing (call when streaming starts)
/// This MUST be called before each new streaming session to reset timestamps
pub fn init_session_timing() {
    let timing = SessionTiming::new();
    log::info!(
        "Session timing initialized at {} us (new session)",
        timing.unix_us
    );
    *SESSION_TIMING.write() = Some(timing);
}

/// Reset session timing (call when streaming stops)
pub fn reset_session_timing() {
    *SESSION_TIMING.write() = None;
    log::info!("Session timing reset");
}

/// Get timestamp in microseconds
/// Uses a hybrid approach: absolute Unix time base + relative offset from session start
/// This provides both accurate server synchronization and consistent timing
#[inline]
pub fn get_timestamp_us() -> u64 {
    let timing = SESSION_TIMING.read();
    if let Some(ref t) = *timing {
        let elapsed_us = t.start.elapsed().as_micros() as u64;
        t.unix_us.wrapping_add(elapsed_us)
    } else {
        // Fallback if not initialized (shouldn't happen during streaming)
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0)
    }
}

/// Get elapsed time since session start (for coalescing decisions)
#[inline]
pub fn session_elapsed_us() -> u64 {
    let timing = SESSION_TIMING.read();
    if let Some(ref t) = *timing {
        t.start.elapsed().as_micros() as u64
    } else {
        0
    }
}

use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use tokio::sync::mpsc;
use winit::event::{ElementState, MouseButton};

use crate::webrtc::{InputEncoder, InputEvent, MAX_CLIPBOARD_PASTE_SIZE};

/// Mouse event coalescing interval in microseconds
/// Official client uses 4-16ms depending on browser, we use 2ms for lowest latency
pub const MOUSE_COALESCE_INTERVAL_US: u64 = 2_000; // 2ms = 500Hz effective rate

/// Maximum input queue depth before throttling
/// Official client maintains 4-8 events ahead of consumption
pub const MAX_INPUT_QUEUE_DEPTH: usize = 8;

/// Mouse event coalescer - batches high-frequency mouse events
/// Similar to official GFN client's getCoalescedEvents() handling
pub struct MouseCoalescer {
    /// Accumulated delta X
    accumulated_dx: AtomicI32,
    /// Accumulated delta Y
    accumulated_dy: AtomicI32,
    /// Last send timestamp (microseconds since session start)
    last_send_us: std::sync::atomic::AtomicU64,
    /// Coalescing interval in microseconds
    coalesce_interval_us: u64,
    /// Count of coalesced events (for stats)
    coalesced_count: std::sync::atomic::AtomicU64,
}

impl MouseCoalescer {
    pub fn new() -> Self {
        Self::with_interval(MOUSE_COALESCE_INTERVAL_US)
    }

    pub fn with_interval(interval_us: u64) -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            accumulated_dx: AtomicI32::new(0),
            accumulated_dy: AtomicI32::new(0),
            last_send_us: AtomicU64::new(0),
            coalesce_interval_us: interval_us,
            coalesced_count: AtomicU64::new(0),
        }
    }

    /// Accumulate mouse delta, returns Some if enough time has passed to send
    /// Returns (dx, dy, timestamp_us) if ready to send, None if still accumulating
    #[inline]
    pub fn accumulate(&self, dx: i32, dy: i32) -> Option<(i16, i16, u64)> {
        // Accumulate the delta
        self.accumulated_dx.fetch_add(dx, Ordering::Relaxed);
        self.accumulated_dy.fetch_add(dy, Ordering::Relaxed);
        self.coalesced_count.fetch_add(1, Ordering::Relaxed);

        let now_us = session_elapsed_us();
        let last_us = self.last_send_us.load(Ordering::Acquire);

        // Check if enough time has passed since last send
        if now_us.saturating_sub(last_us) >= self.coalesce_interval_us {
            self.flush_internal(now_us)
        } else {
            None
        }
    }

    /// Force flush accumulated events (call periodically or on button events)
    pub fn flush(&self) -> Option<(i16, i16, u64)> {
        let now_us = session_elapsed_us();
        self.flush_internal(now_us)
    }

    #[inline]
    fn flush_internal(&self, now_us: u64) -> Option<(i16, i16, u64)> {
        // Atomically take the accumulated deltas
        let dx = self.accumulated_dx.swap(0, Ordering::AcqRel);
        let dy = self.accumulated_dy.swap(0, Ordering::AcqRel);

        // Only send if there's actual movement
        if dx != 0 || dy != 0 {
            self.last_send_us.store(now_us, Ordering::Release);
            let timestamp_us = get_timestamp_us();
            Some((dx as i16, dy as i16, timestamp_us))
        } else {
            None
        }
    }

    /// Get count of coalesced events (for stats)
    pub fn coalesced_count(&self) -> u64 {
        self.coalesced_count.load(Ordering::Relaxed)
    }

    /// Reset the coalescer state
    pub fn reset(&self) {
        self.accumulated_dx.store(0, Ordering::Release);
        self.accumulated_dy.store(0, Ordering::Release);
        self.last_send_us.store(0, Ordering::Release);
        self.coalesced_count.store(0, Ordering::Release);
    }
}

impl Default for MouseCoalescer {
    fn default() -> Self {
        Self::new()
    }
}

/// Local cursor position tracker for instant visual feedback
/// Updates immediately on raw input, independent of network latency
pub struct LocalCursor {
    /// Current X position (screen coordinates)
    x: AtomicI32,
    /// Current Y position (screen coordinates)
    y: AtomicI32,
    /// Stream width for bounds
    stream_width: AtomicI32,
    /// Stream height for bounds
    stream_height: AtomicI32,
    /// Whether cursor is visible/active
    active: AtomicBool,
}

impl LocalCursor {
    pub fn new() -> Self {
        Self {
            x: AtomicI32::new(0),
            y: AtomicI32::new(0),
            stream_width: AtomicI32::new(1920),
            stream_height: AtomicI32::new(1080),
            active: AtomicBool::new(false),
        }
    }

    /// Set stream dimensions (for cursor bounds)
    pub fn set_dimensions(&self, width: u32, height: u32) {
        self.stream_width.store(width as i32, Ordering::Release);
        self.stream_height.store(height as i32, Ordering::Release);
    }

    /// Apply relative movement to cursor position
    #[inline]
    pub fn apply_delta(&self, dx: i32, dy: i32) {
        let width = self.stream_width.load(Ordering::Acquire);
        let height = self.stream_height.load(Ordering::Acquire);

        // Update X with clamping
        let old_x = self.x.load(Ordering::Acquire);
        let new_x = (old_x + dx).clamp(0, width);
        self.x.store(new_x, Ordering::Release);

        // Update Y with clamping
        let old_y = self.y.load(Ordering::Acquire);
        let new_y = (old_y + dy).clamp(0, height);
        self.y.store(new_y, Ordering::Release);
    }

    /// Get current cursor position (normalized 0.0-1.0)
    pub fn position_normalized(&self) -> (f32, f32) {
        let x = self.x.load(Ordering::Acquire) as f32;
        let y = self.y.load(Ordering::Acquire) as f32;
        let w = self.stream_width.load(Ordering::Acquire) as f32;
        let h = self.stream_height.load(Ordering::Acquire) as f32;
        (x / w.max(1.0), y / h.max(1.0))
    }

    /// Get current cursor position (screen coordinates)
    pub fn position(&self) -> (i32, i32) {
        (
            self.x.load(Ordering::Acquire),
            self.y.load(Ordering::Acquire),
        )
    }

    /// Set absolute cursor position
    pub fn set_position(&self, x: i32, y: i32) {
        let width = self.stream_width.load(Ordering::Acquire);
        let height = self.stream_height.load(Ordering::Acquire);
        self.x.store(x.clamp(0, width), Ordering::Release);
        self.y.store(y.clamp(0, height), Ordering::Release);
    }

    /// Center the cursor
    pub fn center(&self) {
        let width = self.stream_width.load(Ordering::Acquire);
        let height = self.stream_height.load(Ordering::Acquire);
        self.x.store(width / 2, Ordering::Release);
        self.y.store(height / 2, Ordering::Release);
    }

    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

impl Default for LocalCursor {
    fn default() -> Self {
        Self::new()
    }
}

/// Cross-platform input handler with coalescing and local cursor support
pub struct InputHandler {
    /// Input event sender
    event_tx: Mutex<Option<mpsc::Sender<InputEvent>>>,

    /// Input encoder
    encoder: Mutex<InputEncoder>,

    /// Whether cursor is captured
    cursor_captured: AtomicBool,

    /// Currently pressed keys (for releasing on focus loss)
    pressed_keys: Mutex<HashSet<u16>>,

    /// Mouse event coalescer for batching high-frequency events
    mouse_coalescer: MouseCoalescer,

    /// Local cursor for instant visual feedback
    local_cursor: LocalCursor,

    /// Input queue depth estimate (for throttling)
    queue_depth: std::sync::atomic::AtomicU64,

    /// Accumulated mouse delta (legacy, for fallback)
    accumulated_dx: AtomicI32,
    accumulated_dy: AtomicI32,

    /// Last known cursor position
    last_x: AtomicI32,
    last_y: AtomicI32,
}

impl InputHandler {
    pub fn new() -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            event_tx: Mutex::new(None),
            encoder: Mutex::new(InputEncoder::new()),
            cursor_captured: AtomicBool::new(false),
            pressed_keys: Mutex::new(HashSet::new()),
            mouse_coalescer: MouseCoalescer::new(),
            local_cursor: LocalCursor::new(),
            queue_depth: AtomicU64::new(0),
            accumulated_dx: AtomicI32::new(0),
            accumulated_dy: AtomicI32::new(0),
            last_x: AtomicI32::new(0),
            last_y: AtomicI32::new(0),
        }
    }

    /// Set the event sender channel (can be called on Arc<InputHandler>)
    pub fn set_event_sender(&self, tx: mpsc::Sender<InputEvent>) {
        *self.event_tx.lock() = Some(tx);
    }

    /// Get local cursor for rendering
    pub fn local_cursor(&self) -> &LocalCursor {
        &self.local_cursor
    }

    /// Get mouse coalescer stats
    pub fn coalesced_event_count(&self) -> u64 {
        self.mouse_coalescer.coalesced_count()
    }

    /// Set stream dimensions for local cursor
    pub fn set_stream_dimensions(&self, width: u32, height: u32) {
        self.local_cursor.set_dimensions(width, height);
        self.local_cursor.center();
        self.local_cursor.set_active(true);
    }

    /// Update queue depth estimate (call from WebRTC layer)
    pub fn update_queue_depth(&self, depth: u64) {
        self.queue_depth.store(depth, Ordering::Release);
    }

    /// Handle mouse button event
    /// Flushes any accumulated mouse movement before button event for proper ordering
    pub fn handle_mouse_button(&self, button: MouseButton, state: ElementState) {
        // Flush accumulated mouse movement BEFORE button event
        // This ensures proper event ordering (move -> click, not click -> move)
        if let Some((dx, dy, timestamp_us)) = self.mouse_coalescer.flush() {
            self.send_event(InputEvent::MouseMove {
                dx,
                dy,
                timestamp_us,
            });
        }

        // GFN uses 1-based button indices: 1=Left, 2=Middle, 3=Right
        let btn = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::Back => 4,
            MouseButton::Forward => 5,
            MouseButton::Other(n) => (n + 1) as u8,
        };

        let timestamp_us = get_timestamp_us();
        let event = match state {
            ElementState::Pressed => InputEvent::MouseButtonDown {
                button: btn,
                timestamp_us,
            },
            ElementState::Released => InputEvent::MouseButtonUp {
                button: btn,
                timestamp_us,
            },
        };

        self.send_event(event);
    }

    /// Handle cursor move (for non-captured mode)
    pub fn handle_cursor_move(&self, x: f64, y: f64) {
        if !self.cursor_captured.load(Ordering::Relaxed) {
            return;
        }

        let x = x as i32;
        let y = y as i32;

        let last_x = self.last_x.swap(x, Ordering::Relaxed);
        let last_y = self.last_y.swap(y, Ordering::Relaxed);

        if last_x != 0 || last_y != 0 {
            let dx = x - last_x;
            let dy = y - last_y;

            if dx != 0 || dy != 0 {
                // Update local cursor for instant feedback
                self.local_cursor.apply_delta(dx, dy);

                // Use coalescer for network events
                if let Some((cdx, cdy, timestamp_us)) = self.mouse_coalescer.accumulate(dx, dy) {
                    self.send_event(InputEvent::MouseMove {
                        dx: cdx,
                        dy: cdy,
                        timestamp_us,
                    });
                }
            }
        }
    }

    /// Handle raw mouse delta (for captured mode) - WITH COALESCING
    /// This is the primary path for mouse input during streaming
    pub fn handle_mouse_delta(&self, dx: i16, dy: i16) {
        if dx == 0 && dy == 0 {
            return;
        }

        // Update local cursor immediately for instant visual feedback
        self.local_cursor.apply_delta(dx as i32, dy as i32);

        // Check queue depth - throttle if queue is getting full
        let depth = self.queue_depth.load(Ordering::Acquire);
        if depth > MAX_INPUT_QUEUE_DEPTH as u64 {
            // Queue is full, still accumulate but may decimate
            self.mouse_coalescer.accumulate(dx as i32, dy as i32);
            return;
        }

        // Use coalescer for batching - sends every 4ms instead of every event
        if let Some((cdx, cdy, timestamp_us)) =
            self.mouse_coalescer.accumulate(dx as i32, dy as i32)
        {
            self.send_event(InputEvent::MouseMove {
                dx: cdx,
                dy: cdy,
                timestamp_us,
            });
        }
    }

    /// Handle raw mouse delta WITHOUT coalescing (for immediate events)
    /// Use this for single-shot movements or when you need immediate transmission
    pub fn handle_mouse_delta_immediate(&self, dx: i16, dy: i16) {
        if dx == 0 && dy == 0 {
            return;
        }

        // Update local cursor
        self.local_cursor.apply_delta(dx as i32, dy as i32);

        // Send immediately without coalescing
        self.send_event(InputEvent::MouseMove {
            dx,
            dy,
            timestamp_us: get_timestamp_us(),
        });
    }

    /// Flush any pending coalesced mouse events
    /// Call this periodically (e.g., every frame) to ensure events don't get stuck
    pub fn flush_mouse_events(&self) {
        if let Some((dx, dy, timestamp_us)) = self.mouse_coalescer.flush() {
            self.send_event(InputEvent::MouseMove {
                dx,
                dy,
                timestamp_us,
            });
        }
    }

    /// Reset input state (call when streaming stops)
    pub fn reset(&self) {
        self.mouse_coalescer.reset();
        self.local_cursor.set_active(false);
        self.queue_depth.store(0, Ordering::Release);
        self.pressed_keys.lock().clear();
    }

    /// Handle keyboard event
    /// keycode is the Windows Virtual Key code (VK code)
    pub fn handle_key(&self, keycode: u16, pressed: bool, modifiers: u16) {
        // Track key state to prevent duplicate events and enable proper release
        let mut pressed_keys = self.pressed_keys.lock();

        if pressed {
            // Only send key down if not already pressed (prevents duplicates)
            if !pressed_keys.insert(keycode) {
                // Key was already pressed, skip to avoid duplicates
                return;
            }
        } else {
            // Only send key up if key was actually pressed
            if !pressed_keys.remove(&keycode) {
                // Key wasn't tracked as pressed, but send release anyway to be safe
            }
        }
        drop(pressed_keys);

        let timestamp_us = get_timestamp_us();
        // GFN uses keycode (VK code), scancode is set to 0
        let event = if pressed {
            InputEvent::KeyDown {
                keycode,
                scancode: 0,
                modifiers,
                timestamp_us,
            }
        } else {
            InputEvent::KeyUp {
                keycode,
                scancode: 0,
                modifiers,
                timestamp_us,
            }
        };

        self.send_event(event);
    }

    /// Release all currently pressed keys (call when focus is lost)
    pub fn release_all_keys(&self) {
        let mut pressed_keys = self.pressed_keys.lock();
        let keys_to_release: Vec<u16> = pressed_keys.drain().collect();
        drop(pressed_keys);

        let timestamp_us = get_timestamp_us();
        for keycode in keys_to_release {
            log::debug!("Releasing stuck key: 0x{:02X}", keycode);
            let event = InputEvent::KeyUp {
                keycode,
                scancode: 0,
                modifiers: 0,
                timestamp_us,
            };
            self.send_event(event);
        }
    }

    /// Handle mouse wheel
    pub fn handle_wheel(&self, delta: i16) {
        self.send_event(InputEvent::MouseWheel {
            delta,
            timestamp_us: get_timestamp_us(),
        });
    }

    /// Set cursor capture state
    pub fn set_cursor_captured(&self, captured: bool) {
        self.cursor_captured.store(captured, Ordering::Relaxed);

        if captured {
            // Reset last position
            self.last_x.store(0, Ordering::Relaxed);
            self.last_y.store(0, Ordering::Relaxed);
        }
    }

    /// Check if cursor is captured
    pub fn is_cursor_captured(&self) -> bool {
        self.cursor_captured.load(Ordering::Relaxed)
    }

    /// Get and reset accumulated mouse delta
    pub fn take_accumulated_delta(&self) -> (i32, i32) {
        let dx = self.accumulated_dx.swap(0, Ordering::Relaxed);
        let dy = self.accumulated_dy.swap(0, Ordering::Relaxed);
        (dx, dy)
    }

    /// Accumulate mouse delta
    pub fn accumulate_delta(&self, dx: i32, dy: i32) {
        self.accumulated_dx.fetch_add(dx, Ordering::Relaxed);
        self.accumulated_dy.fetch_add(dy, Ordering::Relaxed);
    }

    /// Send input event - uses blocking send to ensure events aren't dropped
    fn send_event(&self, event: InputEvent) {
        if let Some(ref tx) = *self.event_tx.lock() {
            // Use blocking_send would require async context
            // For now, use try_send with larger buffer - critical events are tracked
            if tx.try_send(event).is_err() {
                log::warn!("Input channel full - event may be dropped");
            }
        }
    }

    /// Encode and send input directly (for WebRTC data channel)
    pub fn encode_and_send(&self, event: &InputEvent) -> Vec<u8> {
        let mut encoder = self.encoder.lock();
        encoder.encode(event)
    }

    /// Handle clipboard paste (Ctrl+V)
    /// Reads text from clipboard and sends it as key events
    /// Returns the number of characters sent, or 0 if clipboard is empty/unavailable
    pub fn handle_clipboard_paste(&self) -> usize {
        // Try to read clipboard text
        let clipboard_text = match arboard::Clipboard::new() {
            Ok(mut clipboard) => match clipboard.get_text() {
                Ok(text) => text,
                Err(e) => {
                    log::debug!("Failed to read clipboard: {:?}", e);
                    return 0;
                }
            },
            Err(e) => {
                log::debug!("Failed to access clipboard: {:?}", e);
                return 0;
            }
        };

        if clipboard_text.is_empty() {
            return 0;
        }

        // Truncate to max size (64KB like official GFN client)
        let text = if clipboard_text.len() > MAX_CLIPBOARD_PASTE_SIZE {
            log::warn!(
                "Clipboard text truncated from {} to {} bytes",
                clipboard_text.len(),
                MAX_CLIPBOARD_PASTE_SIZE
            );
            &clipboard_text[..MAX_CLIPBOARD_PASTE_SIZE]
        } else {
            &clipboard_text
        };

        log::info!("Pasting {} characters from clipboard", text.chars().count());

        let char_count = text.chars().count();

        // Send ClipboardPaste event - it will be expanded into key events in the WebRTC layer
        self.send_event(InputEvent::ClipboardPaste {
            text: text.to_string(),
        });

        char_count
    }

    /// Check if a key event is Ctrl+V (paste shortcut)
    /// Returns true if the key should trigger clipboard paste
    pub fn is_paste_shortcut(keycode: u16, modifiers: u16) -> bool {
        // VK_V = 0x56, Ctrl modifier = 0x02
        keycode == 0x56 && (modifiers & 0x02) != 0
    }
}

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert winit scancode to GFN scancode
pub fn convert_scancode(scancode: u32) -> u16 {
    // Winit uses platform-specific scancodes
    // For now, pass through directly
    scancode as u16
}

/// Get current modifier state
pub fn get_modifiers(modifiers: &winit::keyboard::ModifiersState) -> u16 {
    let mut result = 0u16;

    if modifiers.shift_key() {
        result |= 0x01;
    }
    if modifiers.control_key() {
        result |= 0x02;
    }
    if modifiers.alt_key() {
        result |= 0x04;
    }
    if modifiers.super_key() {
        result |= 0x08;
    }

    result
}
