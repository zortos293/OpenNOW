//! Windows Raw Input API
//!
//! Provides hardware-level mouse input without OS acceleration.
//! Uses WM_INPUT messages to get raw mouse deltas directly from hardware.
//! Events are coalesced (batched) every 4ms like the official GFN client
//! to prevent server-side buffering while maintaining responsiveness.

use log::{debug, error, info};
use parking_lot::Mutex;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use tokio::sync::mpsc;

use super::{get_timestamp_us, session_elapsed_us, MOUSE_COALESCE_INTERVAL_US};
use crate::webrtc::InputEvent;

// Static state
static RAW_INPUT_REGISTERED: AtomicBool = AtomicBool::new(false);
static RAW_INPUT_ACTIVE: AtomicBool = AtomicBool::new(false);
static ACCUMULATED_DX: AtomicI32 = AtomicI32::new(0);
static ACCUMULATED_DY: AtomicI32 = AtomicI32::new(0);
static MESSAGE_WINDOW: Mutex<Option<isize>> = Mutex::new(None);

// Coalescing state - accumulates events for 4ms batches (like official GFN client)
static COALESCE_DX: AtomicI32 = AtomicI32::new(0);
static COALESCE_DY: AtomicI32 = AtomicI32::new(0);
static COALESCE_LAST_SEND_US: AtomicU64 = AtomicU64::new(0);
static COALESCED_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);

// Local cursor tracking for instant visual feedback (updated on every event)
static LOCAL_CURSOR_X: AtomicI32 = AtomicI32::new(960);
static LOCAL_CURSOR_Y: AtomicI32 = AtomicI32::new(540);
static LOCAL_CURSOR_WIDTH: AtomicI32 = AtomicI32::new(1920);
static LOCAL_CURSOR_HEIGHT: AtomicI32 = AtomicI32::new(1080);

// Direct event sender for immediate mouse events
// Using parking_lot::Mutex for fast, non-blocking access
static EVENT_SENDER: Mutex<Option<mpsc::Sender<InputEvent>>> = Mutex::new(None);

// Win32 types
type HWND = isize;
type WPARAM = usize;
type LPARAM = isize;
type LRESULT = isize;
type HINSTANCE = isize;
type ATOM = u16;

// Window messages
const WM_INPUT: u32 = 0x00FF;
const WM_DESTROY: u32 = 0x0002;

// Raw input constants
const RIDEV_REMOVE: u32 = 0x00000001;
const RID_INPUT: u32 = 0x10000003;
const RIM_TYPEMOUSE: u32 = 0;
const MOUSE_MOVE_RELATIVE: u16 = 0x00;

// HID usage page and usage for mouse
const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
const HID_USAGE_GENERIC_MOUSE: u16 = 0x02;

// Center position for cursor recentering
static CENTER_X: AtomicI32 = AtomicI32::new(0);
static CENTER_Y: AtomicI32 = AtomicI32::new(0);

#[repr(C)]
#[derive(Clone, Copy)]
struct RAWINPUTDEVICE {
    usage_page: u16,
    usage: u16,
    flags: u32,
    hwnd_target: HWND,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RAWINPUTHEADER {
    dw_type: u32,
    dw_size: u32,
    h_device: *mut c_void,
    w_param: WPARAM,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RAWMOUSE {
    flags: u16,
    button_flags: u16,
    button_data: u16,
    raw_buttons: u32,
    last_x: i32,
    last_y: i32,
    extra_information: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
union RAWINPUT_DATA {
    mouse: RAWMOUSE,
    keyboard: [u8; 24],
    hid: [u8; 40],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RAWINPUT {
    header: RAWINPUTHEADER,
    data: RAWINPUT_DATA,
}

#[repr(C)]
struct WNDCLASSEXW {
    cb_size: u32,
    style: u32,
    lpfn_wnd_proc: Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: HINSTANCE,
    h_icon: *mut c_void,
    h_cursor: *mut c_void,
    hbr_background: *mut c_void,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
    h_icon_sm: *mut c_void,
}

#[repr(C)]
struct MSG {
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

#[repr(C)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterRawInputDevices(devices: *const RAWINPUTDEVICE, num_devices: u32, size: u32) -> i32;
    fn GetRawInputData(
        raw_input: *mut c_void,
        command: u32,
        data: *mut c_void,
        size: *mut u32,
        header_size: u32,
    ) -> u32;
    fn DefWindowProcW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    fn RegisterClassExW(wc: *const WNDCLASSEXW) -> ATOM;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: HWND,
        menu: *mut c_void,
        instance: HINSTANCE,
        param: *mut c_void,
    ) -> HWND;
    fn DestroyWindow(hwnd: HWND) -> i32;
    fn GetMessageW(msg: *mut MSG, hwnd: HWND, filter_min: u32, filter_max: u32) -> i32;
    fn TranslateMessage(msg: *const MSG) -> i32;
    fn DispatchMessageW(msg: *const MSG) -> LRESULT;
    fn PostMessageW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> i32;
    fn GetModuleHandleW(module_name: *const u16) -> HINSTANCE;
    fn PostQuitMessage(exit_code: i32);
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn GetForegroundWindow() -> isize;
    fn GetClientRect(hwnd: isize, rect: *mut RECT) -> i32;
    fn ClientToScreen(hwnd: isize, point: *mut POINT) -> i32;
}

/// Convert a Rust string to a null-terminated wide string
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Update the center position based on current window
fn update_center() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == 0 {
            return false;
        }
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetClientRect(hwnd, &mut rect) == 0 {
            return false;
        }
        let mut center = POINT {
            x: rect.right / 2,
            y: rect.bottom / 2,
        };
        if ClientToScreen(hwnd, &mut center) == 0 {
            return false;
        }
        CENTER_X.store(center.x, Ordering::SeqCst);
        CENTER_Y.store(center.y, Ordering::SeqCst);
        true
    }
}

/// Recenter the cursor to prevent it from hitting screen edges
#[inline]
fn recenter_cursor() {
    let cx = CENTER_X.load(Ordering::SeqCst);
    let cy = CENTER_Y.load(Ordering::SeqCst);
    if cx != 0 && cy != 0 {
        unsafe {
            SetCursorPos(cx, cy);
        }
    }
}

/// Register for raw mouse input
fn register_raw_mouse(hwnd: HWND) -> bool {
    let device = RAWINPUTDEVICE {
        usage_page: HID_USAGE_PAGE_GENERIC,
        usage: HID_USAGE_GENERIC_MOUSE,
        flags: 0, // Only receive input when window is focused
        hwnd_target: hwnd,
    };

    unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0 }
}

/// Unregister raw mouse input
fn unregister_raw_mouse() -> bool {
    let device = RAWINPUTDEVICE {
        usage_page: HID_USAGE_PAGE_GENERIC,
        usage: HID_USAGE_GENERIC_MOUSE,
        flags: RIDEV_REMOVE,
        hwnd_target: 0,
    };

    unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0 }
}

/// Process a WM_INPUT message and extract mouse delta
fn process_raw_input(lparam: LPARAM) -> Option<(i32, i32)> {
    unsafe {
        // Use a properly aligned buffer for RAWINPUT struct
        #[repr(C, align(8))]
        struct AlignedBuffer {
            data: [u8; 64],
        }

        let mut buffer = AlignedBuffer { data: [0; 64] };
        let mut size: u32 = buffer.data.len() as u32;

        let result = GetRawInputData(
            lparam as *mut c_void,
            RID_INPUT,
            buffer.data.as_mut_ptr() as *mut c_void,
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        );

        if result == u32::MAX || result == 0 {
            return None;
        }

        // Parse the raw input
        let raw = &*(buffer.data.as_ptr() as *const RAWINPUT);

        // Check if it's mouse input
        if raw.header.dw_type != RIM_TYPEMOUSE {
            return None;
        }

        let mouse = &raw.data.mouse;

        // Only process relative mouse movement
        if mouse.flags == MOUSE_MOVE_RELATIVE {
            if mouse.last_x != 0 || mouse.last_y != 0 {
                return Some((mouse.last_x, mouse.last_y));
            }
        }

        None
    }
}

/// Flush coalesced mouse events - sends accumulated deltas if any
#[inline]
fn flush_coalesced_events() {
    let dx = COALESCE_DX.swap(0, Ordering::AcqRel);
    let dy = COALESCE_DY.swap(0, Ordering::AcqRel);

    if dx != 0 || dy != 0 {
        let timestamp_us = get_timestamp_us();
        let now_us = session_elapsed_us();
        COALESCE_LAST_SEND_US.store(now_us, Ordering::Release);

        let guard = EVENT_SENDER.lock();
        if let Some(ref sender) = *guard {
            let _ = sender.try_send(InputEvent::MouseMove {
                dx: dx as i16,
                dy: dy as i16,
                timestamp_us,
            });
        }
    }
}

/// Window procedure for the message-only window
/// Implements event coalescing: accumulates mouse deltas and sends every 4ms
/// This matches official GFN client behavior and prevents server-side buffering
unsafe extern "system" fn raw_input_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_INPUT => {
            if RAW_INPUT_ACTIVE.load(Ordering::SeqCst) {
                if let Some((dx, dy)) = process_raw_input(lparam) {
                    // 1. Update local cursor IMMEDIATELY for instant visual feedback
                    // This happens on every event regardless of coalescing
                    let width = LOCAL_CURSOR_WIDTH.load(Ordering::Acquire);
                    let height = LOCAL_CURSOR_HEIGHT.load(Ordering::Acquire);
                    let old_x = LOCAL_CURSOR_X.load(Ordering::Acquire);
                    let old_y = LOCAL_CURSOR_Y.load(Ordering::Acquire);
                    LOCAL_CURSOR_X.store((old_x + dx).clamp(0, width), Ordering::Release);
                    LOCAL_CURSOR_Y.store((old_y + dy).clamp(0, height), Ordering::Release);

                    // 2. Accumulate delta for coalescing
                    COALESCE_DX.fetch_add(dx, Ordering::Relaxed);
                    COALESCE_DY.fetch_add(dy, Ordering::Relaxed);
                    COALESCED_EVENT_COUNT.fetch_add(1, Ordering::Relaxed);

                    // 3. Check if enough time has passed to send batch (4ms default)
                    let now_us = session_elapsed_us();
                    let last_us = COALESCE_LAST_SEND_US.load(Ordering::Acquire);

                    if now_us.saturating_sub(last_us) >= MOUSE_COALESCE_INTERVAL_US {
                        flush_coalesced_events();
                    }
                }
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Start raw input capture
pub fn start_raw_input() -> Result<(), String> {
    // No cursor recentering - cursor is hidden during streaming
    // Recentering causes jitter and feedback loops

    // If already registered AND active, just return success
    if RAW_INPUT_REGISTERED.load(Ordering::SeqCst) {
        if RAW_INPUT_ACTIVE.load(Ordering::SeqCst) {
            info!("Raw input already active");
            return Ok(());
        }
        // Re-activating existing registration
        // Reset accumulated state before reactivating to ensure clean state
        ACCUMULATED_DX.store(0, Ordering::SeqCst);
        ACCUMULATED_DY.store(0, Ordering::SeqCst);
        COALESCE_DX.store(0, Ordering::SeqCst);
        COALESCE_DY.store(0, Ordering::SeqCst);
        COALESCE_LAST_SEND_US.store(0, Ordering::SeqCst);

        RAW_INPUT_ACTIVE.store(true, Ordering::SeqCst);
        info!("Raw input resumed with clean state");
        return Ok(());
    }

    // Reset all state before starting fresh
    ACCUMULATED_DX.store(0, Ordering::SeqCst);
    ACCUMULATED_DY.store(0, Ordering::SeqCst);
    COALESCE_DX.store(0, Ordering::SeqCst);
    COALESCE_DY.store(0, Ordering::SeqCst);
    COALESCE_LAST_SEND_US.store(0, Ordering::SeqCst);
    COALESCED_EVENT_COUNT.store(0, Ordering::SeqCst);

    // Spawn a thread to handle the message loop
    std::thread::spawn(|| {
        unsafe {
            let class_name = to_wide("OpenNOW_RawInput_Streamer");
            let h_instance = GetModuleHandleW(std::ptr::null());

            // Register window class
            let wc = WNDCLASSEXW {
                cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfn_wnd_proc: Some(raw_input_wnd_proc),
                cb_cls_extra: 0,
                cb_wnd_extra: 0,
                h_instance,
                h_icon: std::ptr::null_mut(),
                h_cursor: std::ptr::null_mut(),
                hbr_background: std::ptr::null_mut(),
                lpsz_menu_name: std::ptr::null(),
                lpsz_class_name: class_name.as_ptr(),
                h_icon_sm: std::ptr::null_mut(),
            };

            if RegisterClassExW(&wc) == 0 {
                error!("Failed to register raw input window class");
                return;
            }

            // Create message-only window (HWND_MESSAGE = -3)
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                -3isize, // HWND_MESSAGE
                std::ptr::null_mut(),
                h_instance,
                std::ptr::null_mut(),
            );

            if hwnd == 0 {
                error!("Failed to create raw input window");
                return;
            }

            *MESSAGE_WINDOW.lock() = Some(hwnd);

            // Register for raw mouse input
            if !register_raw_mouse(hwnd) {
                error!("Failed to register raw mouse input");
                DestroyWindow(hwnd);
                return;
            }

            RAW_INPUT_REGISTERED.store(true, Ordering::SeqCst);
            RAW_INPUT_ACTIVE.store(true, Ordering::SeqCst);
            info!("Raw input started - receiving hardware mouse deltas (no acceleration)");

            // Message loop
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // Cleanup
            RAW_INPUT_REGISTERED.store(false, Ordering::SeqCst);
            RAW_INPUT_ACTIVE.store(false, Ordering::SeqCst);
            *MESSAGE_WINDOW.lock() = None;
            info!("Raw input thread stopped");
        }
    });

    // Wait for the thread to start
    std::thread::sleep(std::time::Duration::from_millis(50));

    if RAW_INPUT_REGISTERED.load(Ordering::SeqCst) {
        Ok(())
    } else {
        Err("Failed to start raw input".to_string())
    }
}

/// Pause raw input capture
pub fn pause_raw_input() {
    RAW_INPUT_ACTIVE.store(false, Ordering::SeqCst);
    ACCUMULATED_DX.store(0, Ordering::SeqCst);
    ACCUMULATED_DY.store(0, Ordering::SeqCst);
    debug!("Raw input paused");
}

/// Resume raw input capture
pub fn resume_raw_input() {
    if RAW_INPUT_REGISTERED.load(Ordering::SeqCst) {
        // Update center when resuming (window may have moved)
        update_center();
        ACCUMULATED_DX.store(0, Ordering::SeqCst);
        ACCUMULATED_DY.store(0, Ordering::SeqCst);
        RAW_INPUT_ACTIVE.store(true, Ordering::SeqCst);
        debug!("Raw input resumed");
    }
}

/// Stop raw input completely
pub fn stop_raw_input() {
    // First, deactivate to stop processing new events immediately
    RAW_INPUT_ACTIVE.store(false, Ordering::SeqCst);

    // Clear the event sender FIRST to prevent any in-flight events from being sent
    // to a potentially stale channel
    clear_raw_input_sender();

    // Reset all accumulated state to prevent stale deltas in next session
    ACCUMULATED_DX.store(0, Ordering::SeqCst);
    ACCUMULATED_DY.store(0, Ordering::SeqCst);
    COALESCE_DX.store(0, Ordering::SeqCst);
    COALESCE_DY.store(0, Ordering::SeqCst);
    COALESCE_LAST_SEND_US.store(0, Ordering::SeqCst);

    // Reset local cursor to center for next session
    let width = LOCAL_CURSOR_WIDTH.load(Ordering::Acquire);
    let height = LOCAL_CURSOR_HEIGHT.load(Ordering::Acquire);
    LOCAL_CURSOR_X.store(width / 2, Ordering::SeqCst);
    LOCAL_CURSOR_Y.store(height / 2, Ordering::SeqCst);

    unregister_raw_mouse();

    let guard = MESSAGE_WINDOW.lock();
    if let Some(hwnd) = *guard {
        unsafe {
            PostMessageW(hwnd, WM_DESTROY, 0, 0);
        }
    }
    drop(guard);

    // Wait for the thread to actually exit (up to 1000ms with better synchronization)
    // This prevents race conditions when starting a new session immediately
    let start = std::time::Instant::now();
    while RAW_INPUT_REGISTERED.load(Ordering::SeqCst) {
        if start.elapsed() > std::time::Duration::from_millis(1000) {
            error!("Raw input thread did not exit in time, forcing reset");
            RAW_INPUT_REGISTERED.store(false, Ordering::SeqCst);
            *MESSAGE_WINDOW.lock() = None;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Additional safety: ensure we're fully stopped before returning
    // Sleep briefly to let any pending window messages drain
    std::thread::sleep(std::time::Duration::from_millis(50));

    info!("Raw input stopped and fully cleaned up");
}

/// Get accumulated mouse deltas and reset
pub fn get_raw_mouse_delta() -> (i32, i32) {
    let dx = ACCUMULATED_DX.swap(0, Ordering::SeqCst);
    let dy = ACCUMULATED_DY.swap(0, Ordering::SeqCst);
    (dx, dy)
}

/// Check if raw input is active
pub fn is_raw_input_active() -> bool {
    RAW_INPUT_ACTIVE.load(Ordering::SeqCst)
}

/// Update center position (call when window moves/resizes)
pub fn update_raw_input_center() {
    update_center();
}

/// Set the event sender for direct mouse event delivery
/// This allows raw input to send events directly to the streaming loop
/// for minimal latency instead of polling accumulated deltas
pub fn set_raw_input_sender(sender: mpsc::Sender<InputEvent>) {
    let mut guard = EVENT_SENDER.lock();
    *guard = Some(sender);
    info!("Raw input direct sender configured");
}

/// Clear the event sender
pub fn clear_raw_input_sender() {
    let mut guard = EVENT_SENDER.lock();
    *guard = None;
}

/// Set local cursor dimensions (call when stream starts or resolution changes)
pub fn set_local_cursor_dimensions(width: u32, height: u32) {
    LOCAL_CURSOR_WIDTH.store(width as i32, Ordering::Release);
    LOCAL_CURSOR_HEIGHT.store(height as i32, Ordering::Release);
    // Center cursor when dimensions change
    LOCAL_CURSOR_X.store(width as i32 / 2, Ordering::Release);
    LOCAL_CURSOR_Y.store(height as i32 / 2, Ordering::Release);
    info!("Local cursor dimensions set to {}x{}", width, height);
}

/// Get local cursor position (for rendering)
/// Returns (x, y) in stream coordinates
pub fn get_local_cursor_position() -> (i32, i32) {
    (
        LOCAL_CURSOR_X.load(Ordering::Acquire),
        LOCAL_CURSOR_Y.load(Ordering::Acquire),
    )
}

/// Get local cursor position normalized (0.0-1.0)
pub fn get_local_cursor_normalized() -> (f32, f32) {
    let x = LOCAL_CURSOR_X.load(Ordering::Acquire) as f32;
    let y = LOCAL_CURSOR_Y.load(Ordering::Acquire) as f32;
    let w = LOCAL_CURSOR_WIDTH.load(Ordering::Acquire) as f32;
    let h = LOCAL_CURSOR_HEIGHT.load(Ordering::Acquire) as f32;
    (x / w.max(1.0), y / h.max(1.0))
}

/// Flush any pending coalesced mouse events
/// Call this before button events to ensure proper ordering
pub fn flush_pending_mouse_events() {
    flush_coalesced_events();
}

/// Get count of coalesced events (for stats)
pub fn get_coalesced_event_count() -> u64 {
    COALESCED_EVENT_COUNT.load(Ordering::Relaxed)
}

/// Reset coalescing state (call when streaming stops)
pub fn reset_coalescing() {
    COALESCE_DX.store(0, Ordering::Release);
    COALESCE_DY.store(0, Ordering::Release);
    COALESCE_LAST_SEND_US.store(0, Ordering::Release);
    COALESCED_EVENT_COUNT.store(0, Ordering::Release);
    // Center cursor based on actual dimensions, not hardcoded values
    let width = LOCAL_CURSOR_WIDTH.load(Ordering::Acquire);
    let height = LOCAL_CURSOR_HEIGHT.load(Ordering::Acquire);
    LOCAL_CURSOR_X.store(width / 2, Ordering::Release);
    LOCAL_CURSOR_Y.store(height / 2, Ordering::Release);
}
