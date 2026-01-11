//! GFN Input Protocol Encoder/Decoder
//!
//! Binary protocol for sending input events and receiving output events
//! (force feedback, rumble) over WebRTC data channel.

use bytes::{Buf, BufMut, BytesMut};
use log::debug;
use std::time::Instant;

/// Input event type constants (Client → Server)
pub const INPUT_HEARTBEAT: u32 = 2;
pub const INPUT_KEY_DOWN: u32 = 3; // Type 3 = Key pressed
pub const INPUT_KEY_UP: u32 = 4; // Type 4 = Key released
pub const INPUT_MOUSE_ABS: u32 = 5;
pub const INPUT_MOUSE_REL: u32 = 7;
pub const INPUT_MOUSE_BUTTON_DOWN: u32 = 8;
pub const INPUT_MOUSE_BUTTON_UP: u32 = 9;
pub const INPUT_MOUSE_WHEEL: u32 = 10;
pub const INPUT_GAMEPAD: u32 = 12; // Type 12 = Gamepad state (NOT 6!)

/// Output event type constants (Server → Client)
/// These are for force feedback / haptics from the game server
pub const OUTPUT_RUMBLE: u32 = 13; // Controller rumble/vibration
pub const OUTPUT_FORCE_FEEDBACK: u32 = 14; // Racing wheel force feedback

/// Mouse buttons
pub const MOUSE_BUTTON_LEFT: u8 = 0;
pub const MOUSE_BUTTON_RIGHT: u8 = 1;
pub const MOUSE_BUTTON_MIDDLE: u8 = 2;

/// Input events that can be sent to the server
/// Each event carries its own timestamp_us (microseconds since app start)
/// for accurate timing even when events are queued.
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Keyboard key pressed
    KeyDown {
        keycode: u16,
        scancode: u16,
        modifiers: u16,
        timestamp_us: u64,
    },
    /// Keyboard key released
    KeyUp {
        keycode: u16,
        scancode: u16,
        modifiers: u16,
        timestamp_us: u64,
    },
    /// Mouse moved (relative)
    MouseMove { dx: i16, dy: i16, timestamp_us: u64 },
    /// Mouse button pressed
    MouseButtonDown { button: u8, timestamp_us: u64 },
    /// Mouse button released
    MouseButtonUp { button: u8, timestamp_us: u64 },
    /// Mouse wheel scrolled
    MouseWheel { delta: i16, timestamp_us: u64 },
    /// Heartbeat (keep-alive)
    Heartbeat,
    /// Gamepad state update
    Gamepad {
        controller_id: u8,
        button_flags: u16,
        left_trigger: u8,
        right_trigger: u8,
        left_stick_x: i16,
        left_stick_y: i16,
        right_stick_x: i16,
        right_stick_y: i16,
        flags: u16,
        timestamp_us: u64,
    },
}

/// Encoder for GFN input protocol
pub struct InputEncoder {
    buffer: BytesMut,
    start_time: Instant,
    protocol_version: u8,
}

impl InputEncoder {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(256),
            start_time: Instant::now(),
            protocol_version: 2,
        }
    }

    /// Set protocol version (received from handshake)
    pub fn set_protocol_version(&mut self, version: u8) {
        self.protocol_version = version;
    }

    /// Get timestamp in microseconds since start
    fn timestamp_us(&self) -> u64 {
        self.start_time.elapsed().as_micros() as u64
    }

    /// Encode an input event to binary format
    /// Uses the timestamp embedded in each event (captured at creation time)
    pub fn encode(&mut self, event: &InputEvent) -> Vec<u8> {
        self.buffer.clear();

        match event {
            InputEvent::KeyDown {
                keycode,
                scancode,
                modifiers,
                timestamp_us,
            } => {
                // Type 3 (Key Down): 18 bytes
                // [type 4B LE][keycode 2B BE][modifiers 2B BE][scancode 2B BE][timestamp 8B BE]
                self.buffer.put_u32_le(INPUT_KEY_DOWN);
                self.buffer.put_u16(*keycode);
                self.buffer.put_u16(*modifiers);
                self.buffer.put_u16(*scancode);
                self.buffer.put_u64(*timestamp_us);
            }

            InputEvent::KeyUp {
                keycode,
                scancode,
                modifiers,
                timestamp_us,
            } => {
                self.buffer.put_u32_le(INPUT_KEY_UP);
                self.buffer.put_u16(*keycode);
                self.buffer.put_u16(*modifiers);
                self.buffer.put_u16(*scancode);
                self.buffer.put_u64(*timestamp_us);
            }

            InputEvent::MouseMove {
                dx,
                dy,
                timestamp_us,
            } => {
                // Type 7 (Mouse Relative): 22 bytes
                // [type 4B LE][dx 2B BE][dy 2B BE][reserved 6B][timestamp 8B BE]
                self.buffer.put_u32_le(INPUT_MOUSE_REL);
                self.buffer.put_i16(*dx);
                self.buffer.put_i16(*dy);
                self.buffer.put_u16(0); // Reserved
                self.buffer.put_u32(0); // Reserved
                self.buffer.put_u64(*timestamp_us);
            }

            InputEvent::MouseButtonDown {
                button,
                timestamp_us,
            } => {
                // Type 8 (Mouse Button Down): 18 bytes
                // [type 4B LE][button 1B][pad 1B][reserved 4B][timestamp 8B BE]
                self.buffer.put_u32_le(INPUT_MOUSE_BUTTON_DOWN);
                self.buffer.put_u8(*button);
                self.buffer.put_u8(0); // Padding
                self.buffer.put_u32(0); // Reserved
                self.buffer.put_u64(*timestamp_us);
            }

            InputEvent::MouseButtonUp {
                button,
                timestamp_us,
            } => {
                self.buffer.put_u32_le(INPUT_MOUSE_BUTTON_UP);
                self.buffer.put_u8(*button);
                self.buffer.put_u8(0);
                self.buffer.put_u32(0);
                self.buffer.put_u64(*timestamp_us);
            }

            InputEvent::MouseWheel {
                delta,
                timestamp_us,
            } => {
                // Type 10 (Mouse Wheel): 22 bytes
                // [type 4B LE][horiz 2B BE][vert 2B BE][reserved 6B][timestamp 8B BE]
                self.buffer.put_u32_le(INPUT_MOUSE_WHEEL);
                self.buffer.put_i16(0); // Horizontal (unused)
                self.buffer.put_i16(*delta); // Vertical (positive = scroll up)
                self.buffer.put_u16(0); // Reserved
                self.buffer.put_u32(0); // Reserved
                self.buffer.put_u64(*timestamp_us);
            }

            InputEvent::Heartbeat => {
                // Type 2 (Heartbeat): 4 bytes
                self.buffer.put_u32_le(INPUT_HEARTBEAT);
            }

            InputEvent::Gamepad {
                controller_id,
                button_flags,
                left_trigger,
                right_trigger,
                left_stick_x,
                left_stick_y,
                right_stick_x,
                right_stick_y,
                flags,
                timestamp_us,
            } => {
                // Type 12 (Gamepad): 38 bytes total - from web client analysis
                // Web client uses ALL LITTLE ENDIAN (DataView getUint16(true) = LE)
                //
                // Structure (from vendor_beautified.js fd() decoder):
                // [0x00] Type:      4B LE (event type = 12)
                // [0x04] Padding:   2B LE (reserved)
                // [0x06] Index:     2B LE (gamepad index 0-3)
                // [0x08] Bitmap:    2B LE (device type bitmap / flags)
                // [0x0A] Padding:   2B LE (reserved)
                // [0x0C] Buttons:   2B LE (button state bitmask)
                // [0x0E] Trigger:   2B LE (packed: low=LT, high=RT, 0-255 each)
                // [0x10] Axes[0]:   2B LE signed (Left X)
                // [0x12] Axes[1]:   2B LE signed (Left Y)
                // [0x14] Axes[2]:   2B LE signed (Right X)
                // [0x16] Axes[3]:   2B LE signed (Right Y)
                // [0x18] Padding:   2B LE (reserved)
                // [0x1A] Padding:   2B LE (reserved)
                // [0x1C] Padding:   2B LE (reserved)
                // [0x1E] Timestamp: 8B LE (capture timestamp in microseconds)
                // Total: 38 bytes

                self.buffer.put_u32_le(INPUT_GAMEPAD); // 0x00: Type = 12 (LE)
                self.buffer.put_u16_le(0); // 0x04: Padding
                self.buffer.put_u16_le(*controller_id as u16); // 0x06: Index (LE)
                self.buffer.put_u16_le(*flags); // 0x08: Bitmap/flags (LE)
                self.buffer.put_u16_le(0); // 0x0A: Padding
                self.buffer.put_u16_le(*button_flags); // 0x0C: Buttons (LE)
                                                       // Pack triggers: low byte = LT, high byte = RT
                let packed_triggers = (*left_trigger as u16) | ((*right_trigger as u16) << 8);
                self.buffer.put_u16_le(packed_triggers); // 0x0E: Triggers packed (LE)
                self.buffer.put_i16_le(*left_stick_x); // 0x10: Left X (LE)
                self.buffer.put_i16_le(*left_stick_y); // 0x12: Left Y (LE)
                self.buffer.put_i16_le(*right_stick_x); // 0x14: Right X (LE)
                self.buffer.put_i16_le(*right_stick_y); // 0x16: Right Y (LE)
                self.buffer.put_u16_le(0); // 0x18: Padding
                self.buffer.put_u16_le(0); // 0x1A: Padding
                self.buffer.put_u16_le(0); // 0x1C: Padding
                self.buffer.put_u64_le(*timestamp_us); // 0x1E: Timestamp (LE)
            }
        }

        // Protocol v3+ requires single event wrapper
        // Official client uses: [0x22][payload] for single events
        if self.protocol_version > 2 {
            let payload = self.buffer.to_vec();
            let mut final_buf = BytesMut::with_capacity(1 + payload.len());

            // Single event wrapper marker (34 = 0x22)
            final_buf.put_u8(0x22);
            // Payload (already contains timestamp)
            final_buf.extend_from_slice(&payload);

            final_buf.to_vec()
        } else {
            self.buffer.to_vec()
        }
    }

    /// Encode handshake response
    pub fn encode_handshake_response(major: u8, minor: u8, flags: u8) -> Vec<u8> {
        vec![0x0e, major, minor, flags]
    }
}

impl Default for InputEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Output events received from the server (force feedback / haptics)
#[derive(Debug, Clone)]
pub enum OutputEvent {
    /// Controller rumble/vibration
    /// Sent by server when game triggers haptic feedback
    Rumble {
        /// Controller index (0-3)
        controller_id: u8,
        /// Left motor intensity (0-255, low frequency / strong)
        left_motor: u8,
        /// Right motor intensity (0-255, high frequency / weak)
        right_motor: u8,
        /// Duration in milliseconds (0 = stop, 65535 = indefinite)
        duration_ms: u16,
    },
    /// Racing wheel force feedback
    /// Sent by server for steering wheel effects
    ForceFeedback {
        /// Wheel index (usually 0)
        wheel_id: u8,
        /// Effect type (0=constant, 1=spring, 2=damper, 3=friction)
        effect_type: u8,
        /// Force magnitude (-1.0 to 1.0 mapped to -32768 to 32767)
        magnitude: i16,
        /// Duration in milliseconds
        duration_ms: u16,
        /// Additional parameters based on effect type
        param1: i16,
        param2: i16,
    },
    /// Unknown output event
    Unknown { event_type: u32, data: Vec<u8> },
}

/// Decoder for output events from server
pub struct OutputDecoder {
    protocol_version: u8,
}

impl OutputDecoder {
    pub fn new() -> Self {
        Self {
            protocol_version: 2,
        }
    }

    /// Set protocol version (received from handshake)
    pub fn set_protocol_version(&mut self, version: u8) {
        self.protocol_version = version;
    }

    /// Decode an output event from binary data
    /// Returns None if data is not a recognized output event
    pub fn decode(&self, data: &[u8]) -> Option<OutputEvent> {
        if data.is_empty() {
            return None;
        }

        let mut buf = data;

        // Protocol v3+ has wrapper byte
        if self.protocol_version > 2 && !buf.is_empty() && buf[0] == 0x22 {
            buf = &buf[1..];
        }

        // Need at least 4 bytes for event type
        if buf.len() < 4 {
            return None;
        }

        // Read event type (4 bytes LE)
        let event_type = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let payload = &buf[4..];

        match event_type {
            OUTPUT_RUMBLE => self.decode_rumble(payload),
            OUTPUT_FORCE_FEEDBACK => self.decode_force_feedback(payload),
            _ => {
                // Only return Some for actual output events (rumble/FFB)
                // Return None for everything else to allow other handlers to process
                // (e.g., handshake messages which start with 0x0e or 0x020e)
                None
            }
        }
    }

    /// Decode rumble event
    /// Expected format (after type):
    /// [0x00] Controller ID: 1B
    /// [0x01] Left motor:    1B (0-255)
    /// [0x02] Right motor:   1B (0-255)
    /// [0x03] Padding:       1B
    /// [0x04] Duration:      2B LE (milliseconds)
    fn decode_rumble(&self, payload: &[u8]) -> Option<OutputEvent> {
        if payload.len() < 6 {
            debug!("Rumble payload too short: {} bytes", payload.len());
            return None;
        }

        let controller_id = payload[0];
        let left_motor = payload[1];
        let right_motor = payload[2];
        // payload[3] is padding
        let duration_ms = u16::from_le_bytes([payload[4], payload[5]]);

        debug!(
            "Decoded rumble: controller={}, left={}, right={}, duration={}ms",
            controller_id, left_motor, right_motor, duration_ms
        );

        Some(OutputEvent::Rumble {
            controller_id,
            left_motor,
            right_motor,
            duration_ms,
        })
    }

    /// Decode force feedback event
    /// Expected format (after type):
    /// [0x00] Wheel ID:      1B
    /// [0x01] Effect type:   1B (0=constant, 1=spring, 2=damper, 3=friction)
    /// [0x02] Magnitude:     2B LE signed (-32768 to 32767)
    /// [0x04] Duration:      2B LE (milliseconds)
    /// [0x06] Param1:        2B LE signed (effect-specific)
    /// [0x08] Param2:        2B LE signed (effect-specific)
    fn decode_force_feedback(&self, payload: &[u8]) -> Option<OutputEvent> {
        if payload.len() < 10 {
            debug!("FFB payload too short: {} bytes", payload.len());
            return None;
        }

        let wheel_id = payload[0];
        let effect_type = payload[1];
        let magnitude = i16::from_le_bytes([payload[2], payload[3]]);
        let duration_ms = u16::from_le_bytes([payload[4], payload[5]]);
        let param1 = i16::from_le_bytes([payload[6], payload[7]]);
        let param2 = i16::from_le_bytes([payload[8], payload[9]]);

        debug!(
            "Decoded FFB: wheel={}, type={}, magnitude={}, duration={}ms, p1={}, p2={}",
            wheel_id, effect_type, magnitude, duration_ms, param1, param2
        );

        Some(OutputEvent::ForceFeedback {
            wheel_id,
            effect_type,
            magnitude,
            duration_ms,
            param1,
            param2,
        })
    }
}

impl Default for OutputDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mouse_move_encoding() {
        let mut encoder = InputEncoder::new();
        let event = InputEvent::MouseMove {
            dx: -1,
            dy: 5,
            timestamp_us: 12345,
        };
        let encoded = encoder.encode(&event);

        assert_eq!(encoded.len(), 22);
        // Type 7 in LE
        assert_eq!(&encoded[0..4], &[0x07, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn test_heartbeat_encoding() {
        let mut encoder = InputEncoder::new();
        let event = InputEvent::Heartbeat;
        let encoded = encoder.encode(&event);

        assert_eq!(encoded.len(), 4);
        assert_eq!(&encoded[0..4], &[0x02, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn test_rumble_decoding() {
        let decoder = OutputDecoder::new();

        // Type 13 (rumble) + payload
        let data: Vec<u8> = vec![
            0x0D, 0x00, 0x00, 0x00, // Type 13 LE
            0x00, // Controller ID
            0xFF, // Left motor (max)
            0x80, // Right motor (half)
            0x00, // Padding
            0xE8, 0x03, // Duration 1000ms LE
        ];

        let event = decoder.decode(&data).unwrap();
        match event {
            OutputEvent::Rumble {
                controller_id,
                left_motor,
                right_motor,
                duration_ms,
            } => {
                assert_eq!(controller_id, 0);
                assert_eq!(left_motor, 255);
                assert_eq!(right_motor, 128);
                assert_eq!(duration_ms, 1000);
            }
            _ => panic!("Expected Rumble event"),
        }
    }

    #[test]
    fn test_ffb_decoding() {
        let decoder = OutputDecoder::new();

        // Type 14 (FFB) + payload
        let data: Vec<u8> = vec![
            0x0E, 0x00, 0x00, 0x00, // Type 14 LE
            0x00, // Wheel ID
            0x00, // Effect type (constant)
            0x00, 0x40, // Magnitude 16384 (0.5 force)
            0xF4, 0x01, // Duration 500ms LE
            0x00, 0x00, // Param1
            0x00, 0x00, // Param2
        ];

        let event = decoder.decode(&data).unwrap();
        match event {
            OutputEvent::ForceFeedback {
                wheel_id,
                effect_type,
                magnitude,
                duration_ms,
                ..
            } => {
                assert_eq!(wheel_id, 0);
                assert_eq!(effect_type, 0);
                assert_eq!(magnitude, 16384);
                assert_eq!(duration_ms, 500);
            }
            _ => panic!("Expected ForceFeedback event"),
        }
    }
}
