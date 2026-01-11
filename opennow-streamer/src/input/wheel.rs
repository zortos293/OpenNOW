//! Racing Wheel Input Handler
//!
//! Supports racing wheels via Windows.Gaming.Input RacingWheel API.
//! Provides proper axis separation for wheel, throttle, brake, clutch, and handbrake.
//! Includes force feedback support for immersive racing experiences.
//!
//! For GFN compatibility, wheel input is mapped to the gamepad Type 12 format:
//! - Wheel rotation → Left Stick X (-32768 to 32767)
//! - Throttle → Right Trigger (0-255)
//! - Brake → Left Trigger (0-255)
//! - Clutch → Left Stick Y (mapped, or button)
//! - Handbrake → Button flag
//! - Wheel buttons → Standard button flags

use log::{debug, error, info, trace, warn};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::get_timestamp_us;
use crate::webrtc::InputEvent;

/// Force feedback effect types
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FfbEffectType {
    /// Constant force in one direction
    Constant = 0,
    /// Spring effect (centering force)
    Spring = 1,
    /// Damper effect (resistance to motion)
    Damper = 2,
    /// Friction effect
    Friction = 3,
}

impl From<u8> for FfbEffectType {
    fn from(value: u8) -> Self {
        match value {
            0 => FfbEffectType::Constant,
            1 => FfbEffectType::Spring,
            2 => FfbEffectType::Damper,
            3 => FfbEffectType::Friction,
            _ => FfbEffectType::Constant,
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use std::collections::HashMap;
    use windows::Foundation::TimeSpan;
    use windows::Gaming::Input::ForceFeedback::{
        ConstantForceEffect, ForceFeedbackLoadEffectResult, ForceFeedbackMotor,
    };
    use windows::Gaming::Input::RacingWheel;
    use windows_numerics::Vector3;

    /// Racing wheel state
    #[derive(Debug, Clone, Default)]
    pub struct WheelState {
        /// Wheel rotation (-1.0 to 1.0, negative = left, positive = right)
        pub wheel: f64,
        /// Throttle pedal (0.0 to 1.0)
        pub throttle: f64,
        /// Brake pedal (0.0 to 1.0)
        pub brake: f64,
        /// Clutch pedal (0.0 to 1.0)
        pub clutch: f64,
        /// Handbrake (0.0 to 1.0)
        pub handbrake: f64,
        /// Button state (RacingWheelButtons flags)
        pub buttons: u32,
        /// Pattern shifter gear (-1 = reverse, 0 = neutral, 1-10 = gears)
        pub gear: i32,
    }

    /// Force feedback motor state for a wheel
    struct FfbState {
        motor: ForceFeedbackMotor,
        constant_effect: Option<ConstantForceEffect>,
        effect_loaded: bool,
    }

    /// Racing wheel manager using Windows.Gaming.Input
    pub struct WheelManagerImpl {
        running: Arc<AtomicBool>,
        event_tx: Mutex<Option<mpsc::Sender<InputEvent>>>,
        wheels: Mutex<Vec<RacingWheel>>,
        /// Force feedback state per wheel (indexed by wheel index)
        ffb_states: Mutex<HashMap<usize, FfbState>>,
    }

    impl WheelManagerImpl {
        pub fn new() -> Self {
            Self {
                running: Arc::new(AtomicBool::new(false)),
                event_tx: Mutex::new(None),
                wheels: Mutex::new(Vec::new()),
                ffb_states: Mutex::new(HashMap::new()),
            }
        }

        /// Set the input event sender
        pub fn set_event_sender(&self, tx: mpsc::Sender<InputEvent>) {
            *self.event_tx.lock() = Some(tx);
        }

        /// Detect connected racing wheels
        pub fn detect_wheels(&self) -> usize {
            match RacingWheel::RacingWheels() {
                Ok(wheels_view) => {
                    let count = wheels_view.Size().unwrap_or(0) as usize;
                    let mut wheels = self.wheels.lock();
                    wheels.clear();

                    for i in 0..count {
                        if let Ok(wheel) = wheels_view.GetAt(i as u32) {
                            info!("Racing wheel {} detected", i);
                            wheels.push(wheel);
                        }
                    }

                    if count > 0 {
                        info!("Found {} racing wheel(s)", count);
                    }
                    count
                }
                Err(e) => {
                    debug!("No racing wheels found: {:?}", e);
                    0
                }
            }
        }

        /// Start the wheel input polling loop
        pub fn start(&self) {
            if self.running.load(Ordering::SeqCst) {
                return;
            }

            // Detect wheels first
            let wheel_count = self.detect_wheels();
            if wheel_count == 0 {
                info!("No racing wheels detected - wheel input disabled");
                return;
            }

            self.running.store(true, Ordering::SeqCst);
            let running = self.running.clone();

            let tx_opt = self.event_tx.lock().clone();
            if tx_opt.is_none() {
                warn!("WheelManager started without event sender!");
                return;
            }
            let tx = tx_opt.unwrap();

            // Clone wheels for the thread
            let wheels: Vec<RacingWheel> = self.wheels.lock().clone();

            std::thread::spawn(move || {
                info!(
                    "Racing wheel input thread starting with {} wheel(s)...",
                    wheels.len()
                );

                let mut last_states: Vec<WheelState> = vec![WheelState::default(); wheels.len()];
                let mut event_count: u64 = 0;

                while running.load(Ordering::Relaxed) {
                    for (idx, wheel) in wheels.iter().enumerate() {
                        // Read current wheel state
                        if let Ok(reading) = wheel.GetCurrentReading() {
                            let state = WheelState {
                                wheel: reading.Wheel,
                                throttle: reading.Throttle,
                                brake: reading.Brake,
                                clutch: reading.Clutch,
                                handbrake: reading.Handbrake,
                                buttons: reading.Buttons.0 as u32,
                                gear: 0, // Pattern shifter handled separately
                            };

                            // Check if state changed (with small deadzone for analog values)
                            let last = &last_states[idx];
                            let changed = (state.wheel - last.wheel).abs() > 0.001
                                || (state.throttle - last.throttle).abs() > 0.01
                                || (state.brake - last.brake).abs() > 0.01
                                || (state.clutch - last.clutch).abs() > 0.01
                                || (state.handbrake - last.handbrake).abs() > 0.01
                                || state.buttons != last.buttons;

                            if changed {
                                event_count += 1;

                                // Log first few events
                                if event_count <= 5 {
                                    debug!(
                                        "Wheel {}: rotation={:.2}, throttle={:.2}, brake={:.2}, buttons=0x{:08X}",
                                        idx, state.wheel, state.throttle, state.brake, state.buttons
                                    );
                                }

                                // Map wheel state to gamepad format for GFN compatibility
                                // This allows racing games to work without dedicated wheel protocol
                                let event = Self::map_to_gamepad_event(idx as u8, &state);

                                if let Err(e) = tx.try_send(event) {
                                    trace!("Wheel event channel full: {:?}", e);
                                }

                                last_states[idx] = state;
                            }
                        }
                    }

                    // Poll at 1000Hz for low latency
                    std::thread::sleep(Duration::from_millis(1));
                }

                info!(
                    "Racing wheel input thread stopped (processed {} events)",
                    event_count
                );
            });
        }

        /// Map wheel state to gamepad InputEvent for GFN compatibility
        fn map_to_gamepad_event(wheel_idx: u8, state: &WheelState) -> InputEvent {
            // Map wheel rotation to left stick X
            // Wheel: -1.0 (full left) to 1.0 (full right) -> -32768 to 32767
            let left_stick_x = (state.wheel * 32767.0).clamp(-32768.0, 32767.0) as i16;

            // Map throttle to right trigger (0-255)
            let right_trigger = (state.throttle * 255.0).clamp(0.0, 255.0) as u8;

            // Map brake to left trigger (0-255)
            let left_trigger = (state.brake * 255.0).clamp(0.0, 255.0) as u8;

            // Map clutch to left stick Y (some games use this)
            // Clutch: 0.0 (released) to 1.0 (pressed) -> 0 to 32767
            let left_stick_y = (state.clutch * 32767.0).clamp(0.0, 32767.0) as i16;

            // Map handbrake to right stick Y
            let right_stick_y = (state.handbrake * 32767.0).clamp(0.0, 32767.0) as i16;

            // Map wheel buttons to XInput button flags
            let button_flags = Self::map_wheel_buttons(state.buttons);

            InputEvent::Gamepad {
                controller_id: wheel_idx,
                button_flags,
                left_trigger,
                right_trigger,
                left_stick_x,
                left_stick_y,
                right_stick_x: 0,
                right_stick_y,
                flags: 1, // Connected flag
                timestamp_us: get_timestamp_us(),
            }
        }

        /// Map RacingWheelButtons to XInput button flags
        fn map_wheel_buttons(wheel_buttons: u32) -> u16 {
            let mut flags: u16 = 0;

            // RacingWheelButtons enum values (from Windows.Gaming.Input):
            // None = 0
            // PreviousGear = 1
            // NextGear = 2
            // DPadUp = 4
            // DPadDown = 8
            // DPadLeft = 16
            // DPadRight = 32
            // Button1-16 = 64 onwards

            // D-Pad mapping (direct match to XInput)
            if wheel_buttons & 4 != 0 {
                flags |= 0x0001;
            } // DPadUp
            if wheel_buttons & 8 != 0 {
                flags |= 0x0002;
            } // DPadDown
            if wheel_buttons & 16 != 0 {
                flags |= 0x0004;
            } // DPadLeft
            if wheel_buttons & 32 != 0 {
                flags |= 0x0008;
            } // DPadRight

            // Gear shift buttons to bumpers
            if wheel_buttons & 1 != 0 {
                flags |= 0x0100;
            } // PreviousGear -> LB
            if wheel_buttons & 2 != 0 {
                flags |= 0x0200;
            } // NextGear -> RB

            // Wheel-specific buttons to face buttons
            // Button1 (usually main action) -> A
            if wheel_buttons & 64 != 0 {
                flags |= 0x1000;
            }
            // Button2 -> B
            if wheel_buttons & 128 != 0 {
                flags |= 0x2000;
            }
            // Button3 -> X
            if wheel_buttons & 256 != 0 {
                flags |= 0x4000;
            }
            // Button4 -> Y
            if wheel_buttons & 512 != 0 {
                flags |= 0x8000;
            }

            // Button5-6 to Start/Back
            if wheel_buttons & 1024 != 0 {
                flags |= 0x0010;
            } // Start
            if wheel_buttons & 2048 != 0 {
                flags |= 0x0020;
            } // Back

            flags
        }

        /// Initialize force feedback for a wheel
        /// Must be called after detect_wheels() to set up FFB motors
        pub fn init_force_feedback(&self, wheel_idx: usize) -> bool {
            let wheels = self.wheels.lock();
            if wheel_idx >= wheels.len() {
                warn!("Cannot init FFB: wheel index {} out of range", wheel_idx);
                return false;
            }

            let wheel = &wheels[wheel_idx];

            // Check if wheel has force feedback motor
            match wheel.WheelMotor() {
                Ok(motor) => {
                    info!("Wheel {} has force feedback motor", wheel_idx);

                    // Check supported axes
                    if let Ok(axes) = motor.SupportedAxes() {
                        info!("FFB supported axes: {:?}", axes);
                    }

                    // Create constant force effect
                    match ConstantForceEffect::new() {
                        Ok(effect) => {
                            info!("Created ConstantForceEffect for wheel {}", wheel_idx);

                            let ffb_state = FfbState {
                                motor,
                                constant_effect: Some(effect),
                                effect_loaded: false,
                            };

                            self.ffb_states.lock().insert(wheel_idx, ffb_state);
                            true
                        }
                        Err(e) => {
                            error!("Failed to create ConstantForceEffect: {:?}", e);
                            false
                        }
                    }
                }
                Err(e) => {
                    info!(
                        "Wheel {} does not support force feedback: {:?}",
                        wheel_idx, e
                    );
                    false
                }
            }
        }

        /// Apply force feedback effect to a wheel
        /// magnitude: -1.0 (full left) to 1.0 (full right)
        /// duration_ms: effect duration in milliseconds
        pub fn apply_force_feedback(
            &self,
            wheel_idx: usize,
            effect_type: super::FfbEffectType,
            magnitude: f64,
            duration_ms: u16,
        ) {
            let mut ffb_states = self.ffb_states.lock();

            let Some(ffb_state) = ffb_states.get_mut(&wheel_idx) else {
                // Try to initialize FFB if not already done
                drop(ffb_states);
                if self.init_force_feedback(wheel_idx) {
                    // Retry after initialization
                    let mut ffb_states = self.ffb_states.lock();
                    if let Some(ffb_state) = ffb_states.get_mut(&wheel_idx) {
                        self.apply_ffb_internal(ffb_state, effect_type, magnitude, duration_ms);
                    }
                }
                return;
            };

            self.apply_ffb_internal(ffb_state, effect_type, magnitude, duration_ms);
        }

        /// Internal helper to apply FFB effect
        fn apply_ffb_internal(
            &self,
            ffb_state: &mut FfbState,
            effect_type: super::FfbEffectType,
            magnitude: f64,
            duration_ms: u16,
        ) {
            // Currently only support constant force effect
            if effect_type != super::FfbEffectType::Constant {
                debug!(
                    "Effect type {:?} not yet implemented, using constant force",
                    effect_type
                );
            }

            let Some(ref effect) = ffb_state.constant_effect else {
                warn!("No constant effect available");
                return;
            };

            // Clamp magnitude to valid range
            let mag = magnitude.clamp(-1.0, 1.0);

            // Direction vector: X axis for steering wheel
            // Positive X = force to the right, Negative X = force to the left
            let direction = Vector3 {
                X: mag as f32,
                Y: 0.0,
                Z: 0.0,
            };

            // Duration in 100-nanosecond units (TimeSpan)
            let duration = TimeSpan {
                Duration: (duration_ms as i64) * 10_000, // ms to 100ns
            };

            // Set effect parameters
            if let Err(e) = effect.SetParameters(direction, duration) {
                error!("Failed to set FFB parameters: {:?}", e);
                return;
            }

            // Load effect if not already loaded
            if !ffb_state.effect_loaded {
                // LoadEffectAsync returns an async operation
                // We'll wait briefly for it to complete
                match ffb_state.motor.LoadEffectAsync(effect) {
                    Ok(async_op) => {
                        // Wait a short time for the async operation to complete
                        std::thread::sleep(std::time::Duration::from_millis(10));

                        // Try to get results - if still pending, we'll retry next time
                        match async_op.GetResults() {
                            Ok(result) => match result {
                                ForceFeedbackLoadEffectResult::Succeeded => {
                                    info!("FFB effect loaded successfully");
                                    ffb_state.effect_loaded = true;
                                }
                                ForceFeedbackLoadEffectResult::EffectStorageFull => {
                                    warn!("FFB effect storage full");
                                }
                                ForceFeedbackLoadEffectResult::EffectNotSupported => {
                                    warn!("FFB effect not supported by device");
                                }
                                _ => {
                                    warn!(
                                        "FFB effect load returned unexpected result: {:?}",
                                        result
                                    );
                                }
                            },
                            Err(e) => {
                                // May fail if still pending - will retry next time
                                debug!("FFB load pending or failed: {:?}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to start FFB effect load: {:?}", e);
                        return;
                    }
                }
            }

            // Start the effect
            if ffb_state.effect_loaded {
                if let Err(e) = effect.Start() {
                    error!("Failed to start FFB effect: {:?}", e);
                }
            }
        }

        /// Stop all force feedback effects on a wheel
        pub fn stop_force_feedback(&self, wheel_idx: usize) {
            let ffb_states = self.ffb_states.lock();

            if let Some(ffb_state) = ffb_states.get(&wheel_idx) {
                if let Some(ref effect) = ffb_state.constant_effect {
                    if let Err(e) = effect.Stop() {
                        debug!("Failed to stop FFB effect: {:?}", e);
                    }
                }

                // Also stop all effects on the motor
                if let Err(e) = ffb_state.motor.StopAllEffects() {
                    debug!("Failed to stop all FFB effects: {:?}", e);
                }
            }
        }

        /// Stop all force feedback on all wheels
        pub fn stop_all_force_feedback(&self) {
            let ffb_states = self.ffb_states.lock();

            for (idx, ffb_state) in ffb_states.iter() {
                if let Some(ref effect) = ffb_state.constant_effect {
                    let _ = effect.Stop();
                }
                let _ = ffb_state.motor.StopAllEffects();
                debug!("Stopped FFB on wheel {}", idx);
            }
        }

        /// Check if a wheel supports force feedback
        pub fn has_force_feedback(&self, wheel_idx: usize) -> bool {
            self.ffb_states.lock().contains_key(&wheel_idx)
        }

        /// Stop the wheel input loop
        pub fn stop(&self) {
            // Stop all force feedback before stopping
            self.stop_all_force_feedback();
            self.running.store(false, Ordering::SeqCst);
        }

        /// Check if any wheels are connected
        pub fn has_wheels(&self) -> bool {
            !self.wheels.lock().is_empty()
        }

        /// Get the number of connected wheels
        pub fn wheel_count(&self) -> usize {
            self.wheels.lock().len()
        }
    }

    impl Default for WheelManagerImpl {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod fallback_impl {
    use super::*;

    /// Fallback wheel manager for non-Windows platforms
    /// Racing wheels are handled by gilrs as generic gamepads
    pub struct WheelManagerImpl {
        _running: Arc<AtomicBool>,
    }

    impl WheelManagerImpl {
        pub fn new() -> Self {
            Self {
                _running: Arc::new(AtomicBool::new(false)),
            }
        }

        pub fn set_event_sender(&self, _tx: mpsc::Sender<InputEvent>) {
            // No-op on non-Windows
        }

        pub fn detect_wheels(&self) -> usize {
            info!("Racing wheel detection not available on this platform - using gilrs fallback");
            0
        }

        pub fn start(&self) {
            info!("Racing wheel support uses gilrs fallback on this platform");
        }

        pub fn stop(&self) {}

        pub fn has_wheels(&self) -> bool {
            false
        }

        pub fn wheel_count(&self) -> usize {
            0
        }

        // Force feedback stubs for non-Windows platforms
        pub fn init_force_feedback(&self, _wheel_idx: usize) -> bool {
            info!("Force feedback not available on this platform");
            false
        }

        pub fn apply_force_feedback(
            &self,
            _wheel_idx: usize,
            _effect_type: super::FfbEffectType,
            _magnitude: f64,
            _duration_ms: u16,
        ) {
            // No-op on non-Windows
        }

        pub fn stop_force_feedback(&self, _wheel_idx: usize) {
            // No-op on non-Windows
        }

        pub fn stop_all_force_feedback(&self) {
            // No-op on non-Windows
        }

        pub fn has_force_feedback(&self, _wheel_idx: usize) -> bool {
            false
        }
    }

    impl Default for WheelManagerImpl {
        fn default() -> Self {
            Self::new()
        }
    }
}

/// G29 force feedback support using the g29 crate (HID-based)
/// This works when the G29 is in PS3 mode and provides direct FFB control
mod g29_ffb {
    use g29::interface::G29Interface;
    use log::{debug, info};
    use parking_lot::Mutex;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// G29 Force Feedback Manager
    /// Uses the g29 crate for HID-based force feedback control
    pub struct G29FfbManager {
        g29: Mutex<Option<G29Interface>>,
        initialized: AtomicBool,
        connected: AtomicBool,
    }

    impl G29FfbManager {
        pub fn new() -> Self {
            Self {
                g29: Mutex::new(None),
                initialized: AtomicBool::new(false),
                connected: AtomicBool::new(false),
            }
        }

        /// Try to initialize G29 connection
        /// Returns true if G29 was found and initialized
        pub fn init(&self) -> bool {
            if self.initialized.load(Ordering::Relaxed) {
                return self.connected.load(Ordering::Relaxed);
            }

            self.initialized.store(true, Ordering::Relaxed);

            info!("Attempting to connect to Logitech G29 via HID...");
            info!("Note: G29 must be in PS3 mode (switch on wheel) for HID FFB to work");

            // G29Interface::new() panics on failure, so we catch it
            let result = catch_unwind(AssertUnwindSafe(|| G29Interface::new()));

            match result {
                Ok(g29_device) => {
                    info!("Logitech G29 connected via HID!");

                    // Note: reset() takes 10 seconds and calibrates the wheel
                    // We skip it here to avoid blocking - games typically do their own calibration

                    *self.g29.lock() = Some(g29_device);
                    self.connected.store(true, Ordering::Relaxed);
                    true
                }
                Err(_) => {
                    info!("G29 not found via HID (may be in PS4 mode or not connected)");
                    self.connected.store(false, Ordering::Relaxed);
                    false
                }
            }
        }

        /// Check if G29 is connected
        pub fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Relaxed)
        }

        /// Apply constant force feedback
        /// magnitude: -1.0 (full left) to 1.0 (full right)
        pub fn apply_constant_force(&self, magnitude: f64) {
            let g29_lock = self.g29.lock();
            if let Some(ref g29_device) = *g29_lock {
                // g29 crate uses 0.0-1.0 range
                // Clamp to valid range and convert
                let mag = magnitude.clamp(-1.0, 1.0);

                // The g29 crate's force_feedback_constant takes strength 0-1
                // For now we use absolute value - direction handling may need improvement
                let strength = mag.abs() as f32;

                // Catch any panics from the device communication
                let result = catch_unwind(AssertUnwindSafe(|| {
                    g29_device.force_feedback_constant(strength);
                }));

                if result.is_err() {
                    debug!("Failed to apply G29 FFB");
                }
            }
        }

        /// Set autocenter strength
        /// strength: 0.0 (off) to 1.0 (full)
        /// rate: 0.0 (slow) to 1.0 (fast)
        #[allow(dead_code)]
        pub fn set_autocenter(&self, strength: f32, rate: f32) {
            let g29_lock = self.g29.lock();
            if let Some(ref g29_device) = *g29_lock {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    g29_device.set_autocenter(strength.clamp(0.0, 1.0), rate.clamp(0.0, 1.0));
                }));

                if result.is_err() {
                    debug!("Failed to set G29 autocenter");
                }
            }
        }

        /// Stop all force feedback
        pub fn stop(&self) {
            let g29_lock = self.g29.lock();
            if let Some(ref g29_device) = *g29_lock {
                // Turn off force feedback
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    g29_device.force_feedback_constant(0.0);
                }));
            }
        }
    }

    impl Default for G29FfbManager {
        fn default() -> Self {
            Self::new()
        }
    }
}

// Re-export the appropriate implementation
#[cfg(target_os = "windows")]
pub use windows_impl::WheelManagerImpl as WheelManager;

#[cfg(not(target_os = "windows"))]
pub use fallback_impl::WheelManagerImpl as WheelManager;

// Export G29 FFB manager for direct use
pub use g29_ffb::G29FfbManager;
