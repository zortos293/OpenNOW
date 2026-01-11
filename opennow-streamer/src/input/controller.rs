use gilrs::{Axis, Button, Event, EventType, GamepadId, GilrsBuilder};
use log::{debug, error, info, trace, warn};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::mpsc;

use super::get_timestamp_us;
use crate::webrtc::InputEvent;

/// Pending rumble effect to be applied
#[derive(Debug, Clone)]
pub struct RumbleEffect {
    /// Left motor intensity (0-255, low frequency / strong)
    pub left_motor: u8,
    /// Right motor intensity (0-255, high frequency / weak)
    pub right_motor: u8,
    /// Duration in milliseconds (0 = stop)
    pub duration_ms: u16,
    /// When this effect was queued
    pub queued_at: std::time::Instant,
}

impl RumbleEffect {
    pub fn new(left_motor: u8, right_motor: u8, duration_ms: u16) -> Self {
        Self {
            left_motor,
            right_motor,
            duration_ms,
            queued_at: std::time::Instant::now(),
        }
    }

    /// Check if this is a stop command
    pub fn is_stop(&self) -> bool {
        self.left_motor == 0 && self.right_motor == 0
    }
}

/// XInput button format (confirmed from web client analysis)
/// This is the standard XInput wButtons format used by GFN:
///
/// 0x0001 = DPad Up
/// 0x0002 = DPad Down
/// 0x0004 = DPad Left
/// 0x0008 = DPad Right
/// 0x0010 = Start
/// 0x0020 = Back/Select
/// 0x0040 = L3 (Left Stick Click)
/// 0x0080 = R3 (Right Stick Click)
/// 0x0100 = LB (Left Bumper)
/// 0x0200 = RB (Right Bumper)
/// 0x1000 = A
/// 0x2000 = B
/// 0x4000 = X
/// 0x8000 = Y
const XINPUT_DPAD_UP: u16 = 0x0001;
const XINPUT_DPAD_DOWN: u16 = 0x0002;
const XINPUT_DPAD_LEFT: u16 = 0x0004;
const XINPUT_DPAD_RIGHT: u16 = 0x0008;
const XINPUT_START: u16 = 0x0010;
const XINPUT_BACK: u16 = 0x0020;
const XINPUT_L3: u16 = 0x0040;
const XINPUT_R3: u16 = 0x0080;
const XINPUT_LB: u16 = 0x0100;
const XINPUT_RB: u16 = 0x0200;
const XINPUT_A: u16 = 0x1000;
const XINPUT_B: u16 = 0x2000;
const XINPUT_X: u16 = 0x4000;
const XINPUT_Y: u16 = 0x8000;

/// Deadzone for analog sticks (15% as per GFN docs)
const STICK_DEADZONE: f32 = 0.15;

/// Controller manager to handle gamepad input and rumble feedback
pub struct ControllerManager {
    running: Arc<AtomicBool>,
    event_tx: Mutex<Option<mpsc::Sender<InputEvent>>>,
    /// Pending rumble effects per controller (keyed by controller ID)
    rumble_queue: Arc<Mutex<HashMap<u8, RumbleEffect>>>,
    /// Active rumble effects with expiry times
    active_rumble: Arc<Mutex<HashMap<u8, std::time::Instant>>>,
}

impl ControllerManager {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            event_tx: Mutex::new(None),
            rumble_queue: Arc::new(Mutex::new(HashMap::new())),
            active_rumble: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Set the input event sender
    pub fn set_event_sender(&self, tx: mpsc::Sender<InputEvent>) {
        *self.event_tx.lock() = Some(tx);
    }

    /// Start the controller input loop
    pub fn start(&self) {
        if self.running.load(Ordering::SeqCst) {
            return;
        }

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();

        let tx_opt = self.event_tx.lock().clone();

        if tx_opt.is_none() {
            warn!("ControllerManager started without event sender!");
            return;
        }
        let tx = tx_opt.unwrap();

        std::thread::spawn(move || {
            info!("Controller input thread starting...");

            // Initialize gilrs WITHOUT built-in axis filtering
            // This gives us raw axis values so our radial deadzone works correctly
            // on all controller types (Xbox, PS5, etc.)
            let mut gilrs = match GilrsBuilder::new()
                .with_default_filters(false) // Disable all default filters
                .set_axis_to_btn(0.5, 0.4) // Only used for D-pad on some controllers
                .build()
            {
                Ok(g) => {
                    info!("gilrs initialized (raw mode - no built-in filtering)");
                    g
                }
                Err(e) => {
                    error!("Failed to initialize gilrs: {}", e);
                    return;
                }
            };

            // Report connected gamepads and detect racing wheels
            // Racing wheels need special axis mapping (wheel rotation, pedals)
            let mut gamepad_count = 0;
            let mut excluded_devices: Vec<GamepadId> = Vec::new();
            let mut wheel_devices: Vec<GamepadId> = Vec::new();

            for (id, gamepad) in gilrs.gamepads() {
                let name = gamepad.name().to_lowercase();

                // Detect and EXCLUDE racing wheels for now
                // TODO: Racing wheel support is disabled until axis mapping is finalized
                let is_logitech = name.contains("logitech");
                let is_wheel = name.contains("g29")
                    || name.contains("g27")
                    || name.contains("g920")
                    || name.contains("g923")
                    || name.contains("g25")
                    || name.contains("driving force")
                    || name.contains("racing wheel")
                    || name.contains("fanatec")
                    || name.contains("thrustmaster")
                    || name.contains("t150")
                    || name.contains("t300")
                    || name.contains("t500")
                    || (is_logitech && (name.contains("steering") || name.contains("pedal")));

                if is_wheel {
                    info!(
                        "Racing wheel excluded (support disabled): '{}' (id={})",
                        gamepad.name(),
                        id
                    );
                    excluded_devices.push(id);
                    continue;
                }

                gamepad_count += 1;
                info!(
                    "Gamepad {} detected: '{}' (UUID: {:?})",
                    id,
                    gamepad.name(),
                    gamepad.uuid()
                );

                // Log supported features
                debug!("  Power info: {:?}", gamepad.power_info());
                debug!("  Is connected: {}", gamepad.is_connected());
            }

            if gamepad_count == 0 && wheel_devices.is_empty() {
                warn!(
                    "No gamepads or wheels detected at startup. Connect a controller to use input."
                );
            } else {
                if gamepad_count > 0 {
                    info!("Found {} gamepad(s)", gamepad_count);
                }
                if !wheel_devices.is_empty() {
                    info!(
                        "Found {} racing wheel(s) - using wheel axis mapping",
                        wheel_devices.len()
                    );
                }
            }

            let mut last_button_flags: u16 = 0;
            let mut event_count: u64 = 0;

            while running.load(Ordering::Relaxed) {
                // Poll events
                while let Some(Event {
                    id, event, time, ..
                }) = gilrs.next_event()
                {
                    // Skip events from excluded devices
                    if excluded_devices.contains(&id) {
                        continue;
                    }

                    let gamepad = gilrs.gamepad(id);
                    let is_wheel_device = wheel_devices.contains(&id);
                    event_count += 1;

                    // Log first few events for debugging
                    if event_count <= 10 {
                        debug!(
                            "Controller event #{}: {:?} from '{}' at {:?}{}",
                            event_count,
                            event,
                            gamepad.name(),
                            time,
                            if is_wheel_device { " [WHEEL]" } else { "" }
                        );
                    }

                    // Use gamepad index as controller ID (0-3)
                    // GamepadId is opaque, but we can use usize conversion
                    let controller_id: u8 = usize::from(id) as u8;

                    match event {
                        EventType::Connected => {
                            // Check if newly connected device is a wheel (excluded)
                            let name = gamepad.name().to_lowercase();
                            let is_logitech = name.contains("logitech");
                            let is_wheel = name.contains("g29")
                                || name.contains("g27")
                                || name.contains("g920")
                                || name.contains("g923")
                                || name.contains("g25")
                                || name.contains("driving force")
                                || name.contains("racing wheel")
                                || name.contains("fanatec")
                                || name.contains("thrustmaster")
                                || name.contains("t150")
                                || name.contains("t300")
                                || name.contains("t500")
                                || (is_logitech
                                    && (name.contains("steering") || name.contains("pedal")));

                            if is_wheel {
                                info!(
                                    "Racing wheel connected (excluded): {} (id={})",
                                    gamepad.name(),
                                    controller_id
                                );
                                excluded_devices.push(id);
                            } else {
                                info!(
                                    "Gamepad connected: {} (id={})",
                                    gamepad.name(),
                                    controller_id
                                );
                            }
                        }
                        EventType::Disconnected => {
                            // Remove from wheel/excluded lists if it was there
                            excluded_devices.retain(|&x| x != id);
                            wheel_devices.retain(|&x| x != id);
                            info!(
                                "Device disconnected: {} (id={})",
                                gamepad.name(),
                                controller_id
                            );
                        }
                        _ => {
                            let (
                                button_flags,
                                left_trigger,
                                right_trigger,
                                left_stick_x,
                                left_stick_y,
                                right_stick_x,
                                right_stick_y,
                            ) = if is_wheel_device {
                                // RACING WHEEL MAPPING (G29/G27/G920/etc)
                                // Debug: Read all axes to understand the mapping
                                let lsx = gamepad.value(Axis::LeftStickX);
                                let lsy = gamepad.value(Axis::LeftStickY);
                                let rsx = gamepad.value(Axis::RightStickX);
                                let rsy = gamepad.value(Axis::RightStickY);
                                let lz = gamepad.value(Axis::LeftZ);
                                let rz = gamepad.value(Axis::RightZ);

                                // Log axis values periodically
                                static DEBUG_COUNTER: std::sync::atomic::AtomicU64 =
                                    std::sync::atomic::AtomicU64::new(0);
                                let count = DEBUG_COUNTER
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if count % 500 == 0 {
                                    info!(
                                        "G29 axes: LSX={:.3} LSY={:.3} RSX={:.3} RSY={:.3} LZ={:.3} RZ={:.3}",
                                        lsx, lsy, rsx, rsy, lz, rz
                                    );
                                }

                                let mut flags: u16 = 0;

                                // D-Pad - enable for G29
                                if gamepad.is_pressed(Button::DPadUp) {
                                    flags |= XINPUT_DPAD_UP;
                                }
                                if gamepad.is_pressed(Button::DPadDown) {
                                    flags |= XINPUT_DPAD_DOWN;
                                }
                                if gamepad.is_pressed(Button::DPadLeft) {
                                    flags |= XINPUT_DPAD_LEFT;
                                }
                                if gamepad.is_pressed(Button::DPadRight) {
                                    flags |= XINPUT_DPAD_RIGHT;
                                }

                                // Face buttons
                                if gamepad.is_pressed(Button::South) {
                                    flags |= XINPUT_A;
                                }
                                if gamepad.is_pressed(Button::East) {
                                    flags |= XINPUT_B;
                                }
                                if gamepad.is_pressed(Button::West) {
                                    flags |= XINPUT_X;
                                }
                                if gamepad.is_pressed(Button::North) {
                                    flags |= XINPUT_Y;
                                }

                                // Paddle shifters -> LB/RB
                                if gamepad.is_pressed(Button::LeftTrigger) {
                                    flags |= XINPUT_LB;
                                }
                                if gamepad.is_pressed(Button::RightTrigger) {
                                    flags |= XINPUT_RB;
                                }

                                // Start/Select
                                if gamepad.is_pressed(Button::Start) {
                                    flags |= XINPUT_START;
                                }
                                if gamepad.is_pressed(Button::Select) {
                                    flags |= XINPUT_BACK;
                                }

                                // Wheel rotation -> Left Stick X
                                let wheel_x = (lsx * 32767.0).clamp(-32768.0, 32767.0) as i16;

                                // G29 Pedals: The axes report 1.0 when released, -1.0 when fully pressed
                                // We need to convert: 1.0 (released) -> 0, -1.0 (pressed) -> 255

                                // Gas pedal (try multiple axes)
                                let gas = {
                                    // Try RightZ first, then RightStickY, then button data
                                    let axis_val = if rz.abs() > 0.01 {
                                        rz
                                    } else if rsy.abs() > 0.01 {
                                        rsy
                                    } else {
                                        1.0
                                    }; // Default to released

                                    // Convert: 1.0 -> 0, -1.0 -> 255
                                    let normalized = ((1.0 - axis_val) / 2.0).clamp(0.0, 1.0);
                                    (normalized * 255.0) as u8
                                };

                                // Brake pedal
                                let brake = {
                                    let axis_val = if lz.abs() > 0.01 {
                                        lz
                                    } else if lsy.abs() > 0.01 {
                                        lsy
                                    } else {
                                        1.0
                                    };
                                    let normalized = ((1.0 - axis_val) / 2.0).clamp(0.0, 1.0);
                                    (normalized * 255.0) as u8
                                };

                                // Clutch (usually on a separate axis)
                                let clutch_y = {
                                    let axis_val = if rsx.abs() > 0.01 { rsx } else { 1.0 };
                                    let normalized = ((1.0 - axis_val) / 2.0).clamp(0.0, 1.0);
                                    (normalized * 32767.0) as i16
                                };

                                // Log when buttons pressed
                                if flags != 0 {
                                    debug!("G29 buttons: 0x{:04X}", flags);
                                }

                                (flags, brake, gas, wheel_x, clutch_y, 0i16, 0i16)
                            } else {
                                // STANDARD GAMEPAD MAPPING
                                let mut flags: u16 = 0;

                                // D-Pad (bits 0-3)
                                if gamepad.is_pressed(Button::DPadUp) {
                                    flags |= XINPUT_DPAD_UP;
                                }
                                if gamepad.is_pressed(Button::DPadDown) {
                                    flags |= XINPUT_DPAD_DOWN;
                                }
                                if gamepad.is_pressed(Button::DPadLeft) {
                                    flags |= XINPUT_DPAD_LEFT;
                                }
                                if gamepad.is_pressed(Button::DPadRight) {
                                    flags |= XINPUT_DPAD_RIGHT;
                                }

                                // Center buttons (bits 4-5)
                                if gamepad.is_pressed(Button::Start) {
                                    flags |= XINPUT_START;
                                }
                                if gamepad.is_pressed(Button::Select) {
                                    flags |= XINPUT_BACK;
                                }

                                // Stick clicks (bits 6-7)
                                if gamepad.is_pressed(Button::LeftThumb) {
                                    flags |= XINPUT_L3;
                                }
                                if gamepad.is_pressed(Button::RightThumb) {
                                    flags |= XINPUT_R3;
                                }

                                // Shoulder buttons / bumpers (bits 8-9)
                                if gamepad.is_pressed(Button::LeftTrigger) {
                                    flags |= XINPUT_LB;
                                }
                                if gamepad.is_pressed(Button::RightTrigger) {
                                    flags |= XINPUT_RB;
                                }

                                // Face buttons (bits 12-15)
                                if gamepad.is_pressed(Button::South) {
                                    flags |= XINPUT_A;
                                }
                                if gamepad.is_pressed(Button::East) {
                                    flags |= XINPUT_B;
                                }
                                if gamepad.is_pressed(Button::West) {
                                    flags |= XINPUT_X;
                                }
                                if gamepad.is_pressed(Button::North) {
                                    flags |= XINPUT_Y;
                                }

                                // Analog triggers (0-255)
                                let get_trigger_value = |button: Button, axis: Axis| -> u8 {
                                    if let Some(data) = gamepad.button_data(button) {
                                        let val = data.value();
                                        if val > 0.01 {
                                            return (val.clamp(0.0, 1.0) * 255.0) as u8;
                                        }
                                    }
                                    let axis_val = gamepad.value(axis);
                                    if axis_val.abs() > 0.01 {
                                        let normalized = if axis_val < -0.5 {
                                            (axis_val + 1.0) / 2.0
                                        } else {
                                            axis_val
                                        };
                                        let result = (normalized.clamp(0.0, 1.0) * 255.0) as u8;
                                        if result > 0 {
                                            return result;
                                        }
                                    }
                                    if gamepad.is_pressed(button) {
                                        return 255u8;
                                    }
                                    0u8
                                };

                                let lt = get_trigger_value(Button::LeftTrigger2, Axis::LeftZ);
                                let rt = get_trigger_value(Button::RightTrigger2, Axis::RightZ);

                                // Analog sticks
                                let lx_val = gamepad.value(Axis::LeftStickX);
                                let ly_val = gamepad.value(Axis::LeftStickY);
                                let rx_val = gamepad.value(Axis::RightStickX);
                                let ry_val = gamepad.value(Axis::RightStickY);

                                // Apply RADIAL deadzone
                                let apply_radial_deadzone = |x: f32, y: f32| -> (f32, f32) {
                                    let magnitude = (x * x + y * y).sqrt();
                                    if magnitude < STICK_DEADZONE {
                                        (0.0, 0.0)
                                    } else {
                                        let scale = (magnitude - STICK_DEADZONE)
                                            / (1.0 - STICK_DEADZONE)
                                            / magnitude;
                                        (x * scale, y * scale)
                                    }
                                };

                                let (lx, ly) = apply_radial_deadzone(lx_val, ly_val);
                                let (rx, ry) = apply_radial_deadzone(rx_val, ry_val);

                                let lsx = (lx * 32767.0).clamp(-32768.0, 32767.0) as i16;
                                let lsy = (ly * 32767.0).clamp(-32768.0, 32767.0) as i16;
                                let rsx = (rx * 32767.0).clamp(-32768.0, 32767.0) as i16;
                                let rsy = (ry * 32767.0).clamp(-32768.0, 32767.0) as i16;

                                (flags, lt, rt, lsx, lsy, rsx, rsy)
                            };

                            // Log button changes
                            if button_flags != last_button_flags {
                                debug!(
                                    "Button state changed: 0x{:04X} -> 0x{:04X}",
                                    last_button_flags, button_flags
                                );
                                last_button_flags = button_flags;
                            }

                            // Log stick movement occasionally
                            if left_stick_x != 0
                                || left_stick_y != 0
                                || right_stick_x != 0
                                || right_stick_y != 0
                            {
                                trace!(
                                    "Sticks: L({}, {}) R({}, {}) Triggers: L={} R={}",
                                    left_stick_x,
                                    left_stick_y,
                                    right_stick_x,
                                    right_stick_y,
                                    left_trigger,
                                    right_trigger
                                );
                            }

                            let event = InputEvent::Gamepad {
                                controller_id,
                                button_flags,
                                left_trigger,
                                right_trigger,
                                left_stick_x,
                                left_stick_y,
                                right_stick_x,
                                right_stick_y,
                                flags: 1, // 1 = controller connected
                                timestamp_us: get_timestamp_us(),
                            };

                            // Send event
                            if let Err(e) = tx.try_send(event) {
                                trace!("Controller event channel full: {:?}", e);
                            }
                        }
                    }
                }

                // Poll sleep - 1ms for 1000Hz polling rate (low latency)
                std::thread::sleep(Duration::from_millis(1));
            }

            info!(
                "Controller input thread stopped (processed {} events)",
                event_count
            );
        });
    }

    /// Queue a rumble effect for a controller
    /// The effect will be applied on the next polling cycle
    pub fn queue_rumble(
        &self,
        controller_id: u8,
        left_motor: u8,
        right_motor: u8,
        duration_ms: u16,
    ) {
        let effect = RumbleEffect::new(left_motor, right_motor, duration_ms);

        debug!(
            "Queuing rumble for controller {}: left={}, right={}, duration={}ms",
            controller_id, left_motor, right_motor, duration_ms
        );

        self.rumble_queue.lock().insert(controller_id, effect);
    }

    /// Stop rumble on a specific controller
    pub fn stop_rumble(&self, controller_id: u8) {
        self.queue_rumble(controller_id, 0, 0, 0);
    }

    /// Stop rumble on all controllers
    pub fn stop_all_rumble(&self) {
        let mut queue = self.rumble_queue.lock();
        let mut active = self.active_rumble.lock();

        // Queue stop commands for all active controllers
        for controller_id in active.keys() {
            queue.insert(*controller_id, RumbleEffect::new(0, 0, 0));
        }
        active.clear();

        debug!("Stopped all controller rumble");
    }

    /// Apply pending rumble effects (called from gilrs context)
    /// Note: gilrs rumble support is limited on some platforms
    /// This method is designed to be extended with platform-specific backends
    #[cfg(target_os = "windows")]
    fn apply_rumble_effects(&self, gilrs: &mut gilrs::Gilrs) {
        let mut queue = self.rumble_queue.lock();
        let mut active = self.active_rumble.lock();
        let now = std::time::Instant::now();

        // Check for expired active effects
        active.retain(|controller_id, expiry| {
            if now >= *expiry {
                debug!("Rumble expired for controller {}", controller_id);
                false
            } else {
                true
            }
        });

        // Apply new effects from queue
        for (controller_id, effect) in queue.drain() {
            if effect.is_stop() {
                active.remove(&controller_id);
                // Note: gilrs doesn't have a direct "stop rumble" API
                // We'd need platform-specific code here
                debug!("Stopping rumble for controller {}", controller_id);
            } else {
                // Calculate expiry time
                let expiry = now + Duration::from_millis(effect.duration_ms as u64);
                active.insert(controller_id, expiry);

                // Try to apply via gilrs force feedback (limited support)
                // For full support, we'd need XInput directly on Windows
                debug!(
                    "Applying rumble to controller {}: L={}, R={} for {}ms",
                    controller_id, effect.left_motor, effect.right_motor, effect.duration_ms
                );

                // gilrs force feedback is experimental and not widely supported
                // For now, log the attempt - full implementation would use XInput
                for (id, gamepad) in gilrs.gamepads() {
                    if usize::from(id) as u8 == controller_id {
                        if gamepad.is_ff_supported() {
                            info!(
                                "Controller {} supports force feedback via gilrs",
                                controller_id
                            );
                            // gilrs FF would go here - but it's limited
                        }
                        break;
                    }
                }
            }
        }
    }

    /// Apply pending rumble effects (non-Windows fallback)
    #[cfg(not(target_os = "windows"))]
    fn apply_rumble_effects(&self, gilrs: &mut gilrs::Gilrs) {
        let mut queue = self.rumble_queue.lock();
        let mut active = self.active_rumble.lock();
        let now = std::time::Instant::now();

        // Check for expired active effects
        active.retain(|controller_id, expiry| {
            if now >= *expiry {
                debug!("Rumble expired for controller {}", controller_id);
                false
            } else {
                true
            }
        });

        // Apply new effects from queue
        for (controller_id, effect) in queue.drain() {
            if effect.is_stop() {
                active.remove(&controller_id);
            } else {
                let expiry = now + Duration::from_millis(effect.duration_ms as u64);
                active.insert(controller_id, expiry);

                // Try gilrs force feedback on Linux (evdev based)
                for (id, gamepad) in gilrs.gamepads() {
                    if usize::from(id) as u8 == controller_id {
                        if gamepad.is_ff_supported() {
                            info!("Controller {} supports force feedback", controller_id);
                            // Linux FF via evdev would go here
                        }
                        break;
                    }
                }
            }
        }
    }

    /// Check if any rumble is currently active
    pub fn is_rumble_active(&self) -> bool {
        !self.active_rumble.lock().is_empty()
    }

    /// Stop the controller input loop
    pub fn stop(&self) {
        self.stop_all_rumble();
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Default for ControllerManager {
    fn default() -> Self {
        Self::new()
    }
}
