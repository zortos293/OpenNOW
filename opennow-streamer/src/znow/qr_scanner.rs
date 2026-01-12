//! QR Code Scanner
//!
//! Scans video frames for QR codes to detect the znow-runner session code.

use tracing::{info, debug};

/// QR code scanner for video frames
pub struct QrScanner {
    /// Whether scanning is active
    active: bool,
    /// Last detected QR code content
    last_detected: Option<String>,
    /// Frame counter for rate limiting
    frame_count: u64,
    /// Scan every N frames (to reduce CPU usage)
    scan_interval: u64,
}

impl QrScanner {
    pub fn new() -> Self {
        Self {
            active: false,
            last_detected: None,
            frame_count: 0,
            scan_interval: 10, // Scan every 10 frames
        }
    }

    /// Start scanning for QR codes
    pub fn start(&mut self) {
        info!("QR scanner started");
        self.active = true;
        self.last_detected = None;
        self.frame_count = 0;
    }

    /// Stop scanning
    pub fn stop(&mut self) {
        info!("QR scanner stopped");
        self.active = false;
    }

    /// Check if scanner is active
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Get the last detected QR code content
    pub fn last_detected(&self) -> Option<&str> {
        self.last_detected.as_deref()
    }

    /// Scan a video frame for QR codes
    /// Takes Y plane data which may have stride padding
    /// Returns Some(content) if a QR code is detected
    pub fn scan_frame(&mut self, y_plane: &[u8], width: u32, height: u32) -> Option<String> {
        // Call the stride-aware version with stride = width (tightly packed)
        self.scan_frame_with_stride(y_plane, width, height, width)
    }

    /// Scan a video frame for QR codes with explicit stride
    /// stride = bytes per row in y_plane (may be > width due to padding)
    pub fn scan_frame_with_stride(&mut self, y_plane: &[u8], width: u32, height: u32, stride: u32) -> Option<String> {
        if !self.active {
            return None;
        }

        self.frame_count += 1;

        // Rate limit scanning
        if self.frame_count % self.scan_interval != 0 {
            return None;
        }

        debug!("Scanning frame {} for QR code ({}x{}, stride={})", self.frame_count, width, height, stride);

        // Try to decode QR code from the frame
        match self.detect_qr(y_plane, width, height, stride) {
            Some(content) => {
                info!("QR code detected: {}", content);
                self.last_detected = Some(content.clone());
                self.active = false; // Stop scanning after detection
                Some(content)
            }
            None => None,
        }
    }

    /// Detect QR code in Y plane data with stride
    fn detect_qr(&self, y_plane: &[u8], width: u32, height: u32, stride: u32) -> Option<String> {
        let expected_size = (stride * height) as usize;
        if y_plane.len() < expected_size {
            debug!("Y plane too small: {} < {}", y_plane.len(), expected_size);
            return None;
        }

        // Use rqrr for QR detection
        #[cfg(feature = "qr-scanner")]
        {
            use rqrr::PreparedImage;

            // If stride == width, data is tightly packed - use directly
            // Otherwise, we need to copy without stride padding
            let grayscale: Vec<u8> = if stride == width {
                y_plane[..(width * height) as usize].to_vec()
            } else {
                // Copy row by row, skipping stride padding
                let mut packed = Vec::with_capacity((width * height) as usize);
                for row in 0..height {
                    let row_start = (row * stride) as usize;
                    let row_end = row_start + width as usize;
                    packed.extend_from_slice(&y_plane[row_start..row_end]);
                }
                packed
            };

            // Create image from grayscale data
            let img = match image::GrayImage::from_raw(width, height, grayscale) {
                Some(img) => img,
                None => {
                    debug!("Failed to create GrayImage");
                    return None;
                }
            };

            // Prepare image for QR detection
            let mut prepared = PreparedImage::prepare(img);

            // Find and decode QR codes
            let grids = prepared.detect_grids();
            debug!("Found {} potential QR grids", grids.len());

            for grid in grids {
                match grid.decode() {
                    Ok((_, content)) => {
                        return Some(content);
                    }
                    Err(e) => {
                        debug!("Grid decode failed: {:?}", e);
                    }
                }
            }
        }

        // Fallback: simple pattern detection (for testing without rqrr)
        #[cfg(not(feature = "qr-scanner"))]
        {
            // This is a placeholder - in production, rqrr feature should be enabled
            let _ = (y_plane, width, height, stride);
            debug!("QR scanner feature not enabled");
        }

        None
    }

    /// Manually set a detected code (for testing or manual entry)
    pub fn set_detected(&mut self, code: String) {
        self.last_detected = Some(code);
        self.active = false;
    }
}

impl Default for QrScanner {
    fn default() -> Self {
        Self::new()
    }
}
