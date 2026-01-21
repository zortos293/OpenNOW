//! Audio Decoder and Player
//!
//! Decode Opus audio and play through cpal.
//! All platforms now use GStreamer for Opus decoding.
//! Optimized for low-latency streaming with jitter buffer.
//! Supports dynamic device switching and sample rate conversion.

use anyhow::{anyhow, Context, Result};
use log::{debug, error, info, warn};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

/// Audio decoder - platform-specific implementation
/// Non-blocking: decoded samples are sent to a channel
pub struct AudioDecoder {
    cmd_tx: mpsc::Sender<AudioCommand>,
    /// For async decoding - samples come out here
    sample_rx: Option<tokio::sync::mpsc::Receiver<Vec<i16>>>,
    sample_rate: u32,
    channels: u32,
}

enum AudioCommand {
    /// Decode audio and send result to channel
    DecodeAsync(Vec<u8>),
    Stop,
}

// ============================================================================
// GStreamer-based Opus decoder (all platforms: macOS, Linux, Windows x64)
// ============================================================================

#[cfg(any(target_os = "macos", target_os = "linux", all(windows, target_arch = "x86_64")))]
impl AudioDecoder {
    /// Create a new Opus audio decoder using GStreamer (Linux/Windows x64)
    /// Returns decoder and a receiver for decoded samples (for async operation)
    pub fn new(sample_rate: u32, channels: u32) -> Result<Self> {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        use gstreamer_app as gst_app;

        info!(
            "Creating Opus audio decoder (GStreamer): {}Hz, {} channels",
            sample_rate, channels
        );

        // Initialize GStreamer (uses bundled runtime on Windows)
        super::init_gstreamer()?;

        // Create channels for thread communication
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
        // Async channel for decoded samples - large buffer to prevent blocking
        let (sample_tx, sample_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(512);

        let sample_rate_clone = sample_rate;
        let channels_clone = channels;

        thread::spawn(move || {
            // Build GStreamer pipeline for Opus decoding
            // Use opusparse to properly frame raw Opus packets from WebRTC
            // The pipeline: appsrc -> opusparse -> opusdec -> audioconvert -> audioresample -> appsink
            let pipeline_str = format!(
                "appsrc name=src format=time do-timestamp=true ! \
                 opusparse ! \
                 opusdec plc=true ! \
                 audioconvert ! \
                 audioresample ! \
                 audio/x-raw,format=S16LE,rate={},channels={} ! \
                 appsink name=sink emit-signals=true sync=false",
                sample_rate_clone, channels_clone
            );

            let pipeline = match gst::parse::launch(&pipeline_str) {
                Ok(p) => p.downcast::<gst::Pipeline>().unwrap(),
                Err(e) => {
                    error!("Failed to create GStreamer audio pipeline: {}", e);
                    error!("Make sure gstopus.dll and gstaudioconvert.dll plugins are present");
                    return;
                }
            };

            let appsrc = pipeline
                .by_name("src")
                .unwrap()
                .downcast::<gst_app::AppSrc>()
                .unwrap();

            let appsink = pipeline
                .by_name("sink")
                .unwrap()
                .downcast::<gst_app::AppSink>()
                .unwrap();

            // Configure appsrc for raw Opus packets
            // channel-mapping-family=0 means RTP mapping (stereo)
            let caps = gst::Caps::builder("audio/x-opus")
                .field("rate", sample_rate_clone as i32)
                .field("channels", channels_clone as i32)
                .field("channel-mapping-family", 0i32)
                .build();
            appsrc.set_caps(Some(&caps));
            appsrc.set_format(gst::Format::Time);

            // Enable live mode for low latency
            appsrc.set_is_live(true);
            appsrc.set_max_bytes(64 * 1024); // 64KB max buffer

            // Set up appsink callback
            let sample_tx_clone = sample_tx.clone();
            use std::sync::atomic::{AtomicU64, Ordering};
            static DECODED_SAMPLE_COUNT: AtomicU64 = AtomicU64::new(0);
            appsink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        if let Ok(sample) = sink.pull_sample() {
                            if let Some(buffer) = sample.buffer() {
                                if let Ok(map) = buffer.map_readable() {
                                    // Convert bytes to i16 samples
                                    let bytes = map.as_slice();
                                    let samples: Vec<i16> = bytes
                                        .chunks_exact(2)
                                        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
                                        .collect();

                                    if !samples.is_empty() {
                                        let count = DECODED_SAMPLE_COUNT
                                            .fetch_add(samples.len() as u64, Ordering::Relaxed);
                                        if count == 0 {
                                            log::info!(
                                                "First audio samples decoded: {} samples",
                                                samples.len()
                                            );
                                        }
                                        let _ = sample_tx_clone.try_send(samples);
                                    }
                                }
                            }
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );

            // Start pipeline
            if let Err(e) = pipeline.set_state(gst::State::Playing) {
                error!("Failed to start GStreamer audio pipeline: {:?}", e);
                return;
            }

            // Check for pipeline errors on bus
            let bus = pipeline.bus().unwrap();
            std::thread::spawn(move || {
                for msg in bus.iter_timed(gst::ClockTime::NONE) {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::Error(err) => {
                            error!("GStreamer audio error: {} ({:?})", err.error(), err.debug());
                        }
                        MessageView::Warning(warn) => {
                            warn!(
                                "GStreamer audio warning: {} ({:?})",
                                warn.error(),
                                warn.debug()
                            );
                        }
                        MessageView::Eos(..) => {
                            debug!("GStreamer audio EOS");
                            break;
                        }
                        _ => {}
                    }
                }
            });

            info!("Opus audio decoder initialized (GStreamer, async mode)");

            let mut packets_pushed = 0u64;
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    AudioCommand::DecodeAsync(data) => {
                        if !data.is_empty() {
                            let data_len = data.len();
                            // Push Opus packet to GStreamer pipeline
                            let buffer = gst::Buffer::from_slice(data);
                            match appsrc.push_buffer(buffer) {
                                Ok(_) => {
                                    packets_pushed += 1;
                                    if packets_pushed == 1 {
                                        info!("First Opus packet pushed to GStreamer pipeline: {} bytes", data_len);
                                    } else if packets_pushed % 1000 == 0 {
                                        debug!("Audio packets pushed: {}", packets_pushed);
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to push audio buffer: {:?}", e);
                                }
                            }
                        }
                    }
                    AudioCommand::Stop => break,
                }
            }

            // Cleanup
            let _ = appsrc.end_of_stream();
            let _ = pipeline.set_state(gst::State::Null);
            debug!("Audio decoder thread stopped");
        });

        Ok(Self {
            cmd_tx,
            sample_rx: Some(sample_rx),
            sample_rate,
            channels,
        })
    }

    /// Take the sample receiver (for passing to audio player thread)
    pub fn take_sample_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<Vec<i16>>> {
        self.sample_rx.take()
    }

    /// Decode an Opus packet asynchronously (non-blocking, fire-and-forget)
    /// Decoded samples are sent to the sample_rx channel
    pub fn decode_async(&self, data: &[u8]) {
        let _ = self.cmd_tx.send(AudioCommand::DecodeAsync(data.to_vec()));
    }

    /// Get sample rate
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Get channel count
    pub fn channels(&self) -> u32 {
        self.channels
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", all(windows, target_arch = "x86_64")))]
impl Drop for AudioDecoder {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(AudioCommand::Stop);
    }
}

// ============================================================================
// Windows ARM64 - No audio decoding (GStreamer not available)
// ============================================================================

#[cfg(all(windows, target_arch = "aarch64"))]
impl AudioDecoder {
    /// Create a stub audio decoder for Windows ARM64
    /// Note: GStreamer ARM64 binaries are not available, so audio is disabled
    pub fn new(sample_rate: u32, channels: u32) -> Result<Self> {
        warn!(
            "Audio decoding not available on Windows ARM64 (GStreamer not available). \
             Audio will be silent. Sample rate: {}Hz, channels: {}",
            sample_rate, channels
        );

        let (cmd_tx, _cmd_rx) = mpsc::channel::<AudioCommand>();
        let (_sample_tx, sample_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(1);

        Ok(Self {
            cmd_tx,
            sample_rx: Some(sample_rx),
            sample_rate,
            channels,
        })
    }

    /// Take the sample receiver (for passing to audio player thread)
    pub fn take_sample_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<Vec<i16>>> {
        self.sample_rx.take()
    }

    /// Decode an Opus packet asynchronously - stub that does nothing on ARM64
    pub fn decode_async(&self, _data: &[u8]) {
        // No-op: audio not supported on Windows ARM64
    }

    /// Get sample rate
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Get channel count
    pub fn channels(&self) -> u32 {
        self.channels
    }
}

#[cfg(all(windows, target_arch = "aarch64"))]
impl Drop for AudioDecoder {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(AudioCommand::Stop);
    }
}

/// Audio player using cpal with optimized lock-free-ish ring buffer
/// Supports sample rate conversion, channel upmixing, and dynamic device switching
pub struct AudioPlayer {
    /// Input sample rate (from decoder, typically 48000Hz)
    input_sample_rate: u32,
    /// Output sample rate (device native rate)
    output_sample_rate: u32,
    /// Input channel count (from decoder, typically 2 for stereo)
    input_channels: u32,
    /// Output channel count (device channels, may be 8 for 7.1 headsets)
    output_channels: u32,
    buffer: Arc<AudioRingBuffer>,
    stream: Arc<Mutex<Option<cpal::Stream>>>,
    /// Flag to indicate stream needs recreation (device change)
    needs_restart: Arc<AtomicBool>,
    /// Current device name for change detection
    current_device_name: Arc<Mutex<String>>,
    /// Resampler state for rate conversion and channel upmixing
    resampler: Arc<Mutex<AudioResampler>>,
}

/// High-quality audio resampler using Catmull-Rom spline interpolation
/// This provides much better quality than linear interpolation, especially for 2x upsampling
/// Also handles channel upmixing (e.g., stereo to 7.1 surround)
struct AudioResampler {
    input_rate: u32,
    output_rate: u32,
    input_channels: u32,
    output_channels: u32,
    /// Fractional sample position for interpolation
    phase: f64,
    /// History buffer for 4-point interpolation (per input channel)
    /// Stores [s_minus1, s0, s1, s2] for each channel
    history: Vec<[i16; 4]>,
}

/// Lock-free ring buffer for audio samples
/// Uses atomic indices for read/write positions to minimize lock contention
pub struct AudioRingBuffer {
    samples: Mutex<Vec<i16>>,
    read_pos: AtomicUsize,
    write_pos: AtomicUsize,
    capacity: usize,
}

impl AudioRingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            samples: Mutex::new(vec![0i16; capacity]),
            read_pos: AtomicUsize::new(0),
            write_pos: AtomicUsize::new(0),
            capacity,
        }
    }

    fn available(&self) -> usize {
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);
        if write >= read {
            write - read
        } else {
            self.capacity - read + write
        }
    }

    fn free_space(&self) -> usize {
        self.capacity - 1 - self.available()
    }

    /// Write samples to buffer (called from decoder thread)
    fn write(&self, data: &[i16]) {
        let mut samples = self.samples.lock();
        let mut write_pos = self.write_pos.load(Ordering::Acquire);
        let read_pos = self.read_pos.load(Ordering::Acquire);

        for &sample in data {
            let next_pos = (write_pos + 1) % self.capacity;
            // Don't overwrite unread data
            if next_pos != read_pos {
                samples[write_pos] = sample;
                write_pos = next_pos;
            } else {
                // Buffer full - drop remaining samples
                break;
            }
        }

        self.write_pos.store(write_pos, Ordering::Release);
    }

    /// Read samples from buffer (called from audio callback - must be fast!)
    fn read(&self, out: &mut [i16]) {
        let samples = self.samples.lock();
        let write_pos = self.write_pos.load(Ordering::Acquire);
        let mut read_pos = self.read_pos.load(Ordering::Acquire);

        for sample in out.iter_mut() {
            if read_pos == write_pos {
                *sample = 0; // Underrun - output silence
            } else {
                *sample = samples[read_pos];
                read_pos = (read_pos + 1) % self.capacity;
            }
        }

        self.read_pos.store(read_pos, Ordering::Release);
    }
}

impl AudioResampler {
    fn new(input_rate: u32, output_rate: u32, input_channels: u32, output_channels: u32) -> Self {
        info!(
            "Audio resampler: {}Hz {}ch -> {}Hz {}ch",
            input_rate, input_channels, output_rate, output_channels
        );
        Self {
            input_rate,
            output_rate,
            input_channels,
            output_channels,
            phase: 0.0,
            // Initialize history with zeros for each input channel
            history: vec![[0i16; 4]; input_channels as usize],
        }
    }

    /// Resample audio using Catmull-Rom spline interpolation (4-point)
    /// This provides much better quality than linear interpolation
    /// The Catmull-Rom spline passes through all control points and provides
    /// smooth C1 continuous curves, ideal for audio resampling
    /// Also handles channel upmixing (stereo -> multi-channel)
    fn resample(&mut self, input: &[i16]) -> Vec<i16> {
        let in_ch = self.input_channels as usize;
        let out_ch = self.output_channels as usize;
        let input_frames = input.len() / in_ch;

        if input_frames == 0 {
            return Vec::new();
        }

        // Calculate output frame count based on sample rate ratio
        let ratio = self.input_rate as f64 / self.output_rate as f64;
        let output_frames = if self.input_rate == self.output_rate {
            input_frames
        } else {
            ((input_frames as f64) / ratio).ceil() as usize
        };

        let mut output = Vec::with_capacity(output_frames * out_ch);

        // Ensure history is properly sized for input channels
        if self.history.len() != in_ch {
            self.history = vec![[0i16; 4]; in_ch];
        }

        for _ in 0..output_frames {
            let input_idx = self.phase as usize;
            let frac = self.phase - input_idx as f64;

            // First, get the resampled stereo frame
            let mut stereo_frame = [0i16; 2];

            for ch in 0..in_ch.min(2) {
                // Get 4 samples for Catmull-Rom interpolation: s[-1], s[0], s[1], s[2]
                let get_sample = |frame_idx: isize| -> i16 {
                    if frame_idx < 0 {
                        // Use history for samples before current buffer
                        let hist_idx = (4 + frame_idx) as usize;
                        self.history[ch][hist_idx.min(3)]
                    } else if (frame_idx as usize) < input_frames {
                        input[frame_idx as usize * in_ch + ch]
                    } else {
                        // Clamp to last sample
                        if input_frames > 0 {
                            input[(input_frames - 1) * in_ch + ch]
                        } else {
                            self.history[ch][3] // Use last known sample
                        }
                    }
                };

                let s0 = get_sample(input_idx as isize - 1) as f64;
                let s1 = get_sample(input_idx as isize) as f64;
                let s2 = get_sample(input_idx as isize + 1) as f64;
                let s3 = get_sample(input_idx as isize + 2) as f64;

                // Catmull-Rom spline interpolation formula
                // This provides C1 continuity (smooth first derivative)
                let t = frac;
                let t2 = t * t;
                let t3 = t2 * t;

                let interpolated = 0.5
                    * ((2.0 * s1)
                        + (-s0 + s2) * t
                        + (2.0 * s0 - 5.0 * s1 + 4.0 * s2 - s3) * t2
                        + (-s0 + 3.0 * s1 - 3.0 * s2 + s3) * t3);

                stereo_frame[ch] = interpolated.clamp(-32768.0, 32767.0) as i16;
            }

            // Now upmix stereo to output channel count
            // Standard channel mapping for common configurations:
            // 2ch: FL, FR
            // 6ch (5.1): FL, FR, FC, LFE, BL, BR
            // 8ch (7.1): FL, FR, FC, LFE, BL, BR, SL, SR
            match out_ch {
                1 => {
                    // Mono: mix L+R
                    let mono = ((stereo_frame[0] as i32 + stereo_frame[1] as i32) / 2) as i16;
                    output.push(mono);
                }
                2 => {
                    // Stereo: swap L/R channels (GFN sends them inverted)
                    output.push(stereo_frame[1]); // Right -> Left
                    output.push(stereo_frame[0]); // Left -> Right
                }
                _ => {
                    // Multi-channel (5.1, 7.1, etc.)
                    // Standard layout: FL, FR, FC, LFE, BL, BR, [SL, SR for 7.1+]
                    // Swap L/R channels (GFN sends them inverted)
                    let left = stereo_frame[1];
                    let right = stereo_frame[0];

                    for ch_idx in 0..out_ch {
                        let sample = match ch_idx {
                            0 => left,  // Front Left
                            1 => right, // Front Right
                            2 => {
                                // Center - mix of L+R at reduced level
                                ((left as i32 + right as i32) / 3) as i16
                            }
                            3 => 0, // LFE - no bass routing for now
                            4 => {
                                // Back/Rear Left - copy of front left at reduced level
                                (left as i32 * 2 / 3) as i16
                            }
                            5 => {
                                // Back/Rear Right - copy of front right at reduced level
                                (right as i32 * 2 / 3) as i16
                            }
                            6 => {
                                // Side Left (7.1) - copy of front left at reduced level
                                (left as i32 / 2) as i16
                            }
                            7 => {
                                // Side Right (7.1) - copy of front right at reduced level
                                (right as i32 / 2) as i16
                            }
                            _ => 0, // Any additional channels: silence
                        };
                        output.push(sample);
                    }
                }
            }

            self.phase += ratio;
        }

        // Update history with last 4 samples from this buffer for next call
        if input_frames >= 4 {
            for ch in 0..in_ch {
                for i in 0..4 {
                    self.history[ch][i] = input[(input_frames - 4 + i) * in_ch + ch];
                }
            }
        } else if input_frames > 0 {
            // Shift history and add new samples
            for ch in 0..in_ch {
                let shift = 4 - input_frames;
                for i in 0..shift {
                    self.history[ch][i] = self.history[ch][i + input_frames];
                }
                for i in 0..input_frames {
                    self.history[ch][shift + i] = input[i * in_ch + ch];
                }
            }
        }

        // Keep fractional phase, reset integer part
        self.phase = self.phase.fract();

        output
    }

    /// Update output rate and channels (for device change)
    fn set_output_config(&mut self, output_rate: u32, output_channels: u32) {
        if self.output_rate != output_rate || self.output_channels != output_channels {
            self.output_rate = output_rate;
            self.output_channels = output_channels;
            self.phase = 0.0;
            // Reset history on config change
            for hist in &mut self.history {
                *hist = [0i16; 4];
            }
            info!(
                "Resampler updated: {}Hz {}ch -> {}Hz {}ch",
                self.input_rate, self.input_channels, output_rate, output_channels
            );
        }
    }
}

impl AudioPlayer {
    /// Create a new audio player
    pub fn new(sample_rate: u32, channels: u32) -> Result<Self> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use cpal::SampleFormat;

        info!(
            "Creating audio player: {}Hz, {} channels",
            sample_rate, channels
        );

        let host = cpal::default_host();

        let device = host
            .default_output_device()
            .context("No audio output device found")?;

        info!("Using audio device: {}", device.name().unwrap_or_default());

        // Query supported configurations
        let supported_configs: Vec<_> = device
            .supported_output_configs()
            .map(|configs| configs.collect())
            .unwrap_or_default();

        if supported_configs.is_empty() {
            return Err(anyhow!("No supported audio configurations found"));
        }

        // Log available configurations for debugging
        for cfg in &supported_configs {
            debug!(
                "Supported config: {:?} channels, {:?}-{:?} Hz, format {:?}",
                cfg.channels(),
                cfg.min_sample_rate().0,
                cfg.max_sample_rate().0,
                cfg.sample_format()
            );
        }

        // Find best matching configuration
        // Prefer: f32 format (most compatible), matching channels, matching sample rate
        let target_rate = cpal::SampleRate(sample_rate);
        let target_channels = channels as u16;

        // Try to find a config that supports our sample rate and channel count
        let mut best_config = None;
        let mut best_score = 0i32;

        for cfg in &supported_configs {
            let mut score = 0i32;

            // Prefer f32 format (most widely supported)
            if cfg.sample_format() == SampleFormat::F32 {
                score += 100;
            } else if cfg.sample_format() == SampleFormat::I16 {
                score += 50;
            }

            // Prefer matching channel count
            if cfg.channels() == target_channels {
                score += 50;
            } else if cfg.channels() >= target_channels {
                score += 25;
            }

            // Check if sample rate is in range
            if target_rate >= cfg.min_sample_rate() && target_rate <= cfg.max_sample_rate() {
                score += 100;
            } else if cfg.max_sample_rate().0 >= 44100 {
                score += 25; // At least supports reasonable rates
            }

            if score > best_score {
                best_score = score;
                best_config = Some(cfg.clone());
            }
        }

        let supported_range =
            best_config.ok_or_else(|| anyhow!("No suitable audio configuration found"))?;

        // Determine actual sample rate to use
        let actual_rate = if target_rate >= supported_range.min_sample_rate()
            && target_rate <= supported_range.max_sample_rate()
        {
            target_rate
        } else if cpal::SampleRate(48000) >= supported_range.min_sample_rate()
            && cpal::SampleRate(48000) <= supported_range.max_sample_rate()
        {
            cpal::SampleRate(48000)
        } else if cpal::SampleRate(44100) >= supported_range.min_sample_rate()
            && cpal::SampleRate(44100) <= supported_range.max_sample_rate()
        {
            cpal::SampleRate(44100)
        } else {
            supported_range.max_sample_rate()
        };

        let actual_channels = supported_range.channels();
        let sample_format = supported_range.sample_format();

        info!(
            "Using audio config: {}Hz, {} channels, format {:?}",
            actual_rate.0, actual_channels, sample_format
        );

        // Buffer for ~150ms of audio (handles network jitter)
        // 48000Hz * 2ch * 0.15s = 14400 samples
        // Larger buffer prevents underruns from network jitter
        let buffer_size = (actual_rate.0 as usize) * (actual_channels as usize) * 150 / 1000;
        let buffer = Arc::new(AudioRingBuffer::new(buffer_size));

        info!(
            "Audio buffer size: {} samples (~{}ms)",
            buffer_size,
            buffer_size * 1000 / (actual_rate.0 as usize * actual_channels as usize)
        );

        let config = supported_range.with_sample_rate(actual_rate).into();

        let buffer_clone = buffer.clone();

        // Build stream based on sample format
        // The callback reads from the ring buffer - optimized for low latency
        let stream = match sample_format {
            SampleFormat::F32 => {
                let buffer_f32 = buffer_clone.clone();
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _| {
                            // Read i16 samples in bulk and convert to f32
                            let mut i16_buf = vec![0i16; data.len()];
                            buffer_f32.read(&mut i16_buf);
                            for (out, &sample) in data.iter_mut().zip(i16_buf.iter()) {
                                *out = sample as f32 / 32768.0;
                            }
                        },
                        |err| {
                            error!("Audio stream error: {}", err);
                        },
                        None,
                    )
                    .context("Failed to create f32 audio stream")?
            }
            SampleFormat::I16 => {
                let buffer_i16 = buffer_clone.clone();
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _| {
                            buffer_i16.read(data);
                        },
                        |err| {
                            error!("Audio stream error: {}", err);
                        },
                        None,
                    )
                    .context("Failed to create i16 audio stream")?
            }
            _ => {
                // Fallback: try f32 anyway
                let buffer_fallback = buffer_clone.clone();
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _| {
                            let mut i16_buf = vec![0i16; data.len()];
                            buffer_fallback.read(&mut i16_buf);
                            for (out, &sample) in data.iter_mut().zip(i16_buf.iter()) {
                                *out = sample as f32 / 32768.0;
                            }
                        },
                        |err| {
                            error!("Audio stream error: {}", err);
                        },
                        None,
                    )
                    .context("Failed to create audio stream with fallback format")?
            }
        };

        stream.play().context("Failed to start audio playback")?;

        let device_name = device.name().unwrap_or_default();
        info!("Audio player started successfully on '{}'", device_name);

        // Create resampler for input_rate -> output_rate conversion and channel upmixing
        // Input: decoder's sample_rate (48000) and channels (2 for stereo Opus)
        // Output: device's actual_rate and actual_channels (may be 8 for 7.1 headsets)
        let resampler =
            AudioResampler::new(sample_rate, actual_rate.0, channels, actual_channels as u32);

        if sample_rate != actual_rate.0 || channels != actual_channels as u32 {
            info!(
                "Audio conversion enabled: {}Hz {}ch -> {}Hz {}ch",
                sample_rate, channels, actual_rate.0, actual_channels
            );
        }

        Ok(Self {
            input_sample_rate: sample_rate,
            output_sample_rate: actual_rate.0,
            input_channels: channels,
            output_channels: actual_channels as u32,
            buffer,
            stream: Arc::new(Mutex::new(Some(stream))),
            needs_restart: Arc::new(AtomicBool::new(false)),
            current_device_name: Arc::new(Mutex::new(device_name)),
            resampler: Arc::new(Mutex::new(resampler)),
        })
    }

    /// Push audio samples to the player (with automatic resampling and channel upmixing)
    pub fn push_samples(&self, samples: &[i16]) {
        // Check if device changed and we need to restart
        self.check_device_change();

        // Resample and upmix (48000Hz stereo -> device rate and channels)
        let resampled = {
            let mut resampler = self.resampler.lock();
            resampler.resample(samples)
        };

        self.buffer.write(&resampled);
    }

    /// Get buffer fill level
    pub fn buffer_available(&self) -> usize {
        self.buffer.available()
    }

    /// Get output sample rate (device rate)
    pub fn sample_rate(&self) -> u32 {
        self.output_sample_rate
    }

    /// Get output channel count (device channels)
    pub fn channels(&self) -> u32 {
        self.output_channels
    }

    /// Check if the default audio device changed and restart stream if needed
    fn check_device_change(&self) {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let current_device = match host.default_output_device() {
            Some(d) => d,
            None => return,
        };

        let new_name = current_device.name().unwrap_or_default();
        let current_name = self.current_device_name.lock().clone();

        if new_name != current_name && !new_name.is_empty() {
            warn!("Audio device changed: '{}' -> '{}'", current_name, new_name);

            // Update device name
            *self.current_device_name.lock() = new_name.clone();

            // Recreate the audio stream on the new device
            if let Err(e) = self.recreate_stream(&current_device) {
                error!("Failed to switch audio device: {}", e);
            } else {
                info!("Audio switched to '{}'", new_name);
            }
        }
    }

    /// Recreate the audio stream on a new device
    fn recreate_stream(&self, device: &cpal::Device) -> Result<()> {
        use cpal::traits::{DeviceTrait, StreamTrait};
        use cpal::SampleFormat;

        // Stop old stream
        *self.stream.lock() = None;

        // Query supported configurations
        let supported_configs: Vec<_> = device
            .supported_output_configs()
            .map(|configs| configs.collect())
            .unwrap_or_default();

        if supported_configs.is_empty() {
            return Err(anyhow!("No supported audio configurations on new device"));
        }

        // Find best config (prefer F32, stereo-compatible)
        let mut best_config = None;
        let mut best_score = 0i32;

        for cfg in &supported_configs {
            let mut score = 0i32;
            if cfg.sample_format() == SampleFormat::F32 {
                score += 100;
            }
            // Prefer stereo, but accept any channel count (we'll upmix)
            if cfg.channels() == 2 {
                score += 50;
            } else if cfg.channels() >= 2 {
                score += 25;
            }
            if cfg.max_sample_rate().0 >= 44100 {
                score += 25;
            }

            if score > best_score {
                best_score = score;
                best_config = Some(cfg.clone());
            }
        }

        let supported_range =
            best_config.ok_or_else(|| anyhow!("No suitable audio config on new device"))?;

        // Pick sample rate
        let actual_rate = if cpal::SampleRate(48000) >= supported_range.min_sample_rate()
            && cpal::SampleRate(48000) <= supported_range.max_sample_rate()
        {
            cpal::SampleRate(48000)
        } else if cpal::SampleRate(44100) >= supported_range.min_sample_rate()
            && cpal::SampleRate(44100) <= supported_range.max_sample_rate()
        {
            cpal::SampleRate(44100)
        } else {
            supported_range.max_sample_rate()
        };

        let actual_channels = supported_range.channels();
        let sample_format = supported_range.sample_format();
        let config = supported_range.with_sample_rate(actual_rate).into();
        let buffer = self.buffer.clone();

        info!(
            "New device config: {}Hz, {} channels, {:?}",
            actual_rate.0, actual_channels, sample_format
        );

        // Update resampler for new output rate and channels
        self.resampler
            .lock()
            .set_output_config(actual_rate.0, actual_channels as u32);

        // Build new stream
        let stream = match sample_format {
            SampleFormat::F32 => {
                let buf = buffer.clone();
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _| {
                            let mut i16_buf = vec![0i16; data.len()];
                            buf.read(&mut i16_buf);
                            for (out, &sample) in data.iter_mut().zip(i16_buf.iter()) {
                                *out = sample as f32 / 32768.0;
                            }
                        },
                        |err| error!("Audio stream error: {}", err),
                        None,
                    )
                    .context("Failed to create audio stream")?
            }
            SampleFormat::I16 => {
                let buf = buffer.clone();
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _| {
                            buf.read(data);
                        },
                        |err| error!("Audio stream error: {}", err),
                        None,
                    )
                    .context("Failed to create audio stream")?
            }
            _ => {
                let buf = buffer.clone();
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _| {
                            let mut i16_buf = vec![0i16; data.len()];
                            buf.read(&mut i16_buf);
                            for (out, &sample) in data.iter_mut().zip(i16_buf.iter()) {
                                *out = sample as f32 / 32768.0;
                            }
                        },
                        |err| error!("Audio stream error: {}", err),
                        None,
                    )
                    .context("Failed to create audio stream")?
            }
        };

        stream
            .play()
            .context("Failed to start audio on new device")?;
        *self.stream.lock() = Some(stream);

        Ok(())
    }
}
