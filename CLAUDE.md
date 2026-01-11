# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

OpenNOW is an open-source GeForce NOW client written in Native Rust. The main application is in `opennow-streamer/` - a high-performance streaming client using wgpu for rendering and WebRTC for game streaming.

## Build Commands

```bash
# Development build (faster compilation)
cd opennow-streamer
cargo build

# Release build (optimized, use for testing performance)
cargo build --release

# Run development build
cargo run

# Format code before committing
cargo fmt

# Lint
cargo clippy
```

### Platform-Specific Notes

- **Windows**: Uses D3D11VA for zero-copy video decoding, GStreamer for audio
- **macOS**: Uses VideoToolbox for hardware decoding, FFmpeg for audio
- **Linux**: Uses Vulkan Video for decoding (Intel Arc, NVIDIA RTX, AMD RDNA2+), GStreamer for audio and Raspberry Pi V4L2 fallback

## Architecture

### Core Modules (in `opennow-streamer/src/`)

| Module | Purpose |
|--------|---------|
| `main.rs` | Application entry point, winit event loop, input routing |
| `app/` | Application state management, settings, caching, session handling |
| `gui/` | wgpu renderer, egui UI, game library screens, stats overlay |
| `webrtc/` | WebRTC peer connection, signaling, SDP handling, data channels |
| `media/` | Video decoders (D3D11VA, VideoToolbox, Vulkan, GStreamer), audio (Opus via GStreamer/cpal), RTP parsing |
| `input/` | Platform-specific input capture (Windows Raw Input, macOS CGEventTap, Linux evdev), gamepad via gilrs, racing wheel FFB |
| `api/` | GFN API integration: CloudMatch session management, game catalog GraphQL, queue times |
| `auth/` | OAuth authentication flow |

### Data Flow

1. **Authentication**: `auth/` handles OAuth → tokens stored in `app/cache.rs`
2. **Game Launch**: `api/cloudmatch.rs` creates session → polls until ready
3. **Streaming**: `webrtc/signaling.rs` connects → `webrtc/peer.rs` establishes WebRTC → video/audio tracks received
4. **Video Pipeline**: RTP packets (`media/rtp.rs`) → hardware decoder (`media/video.rs` dispatches to platform decoder) → wgpu texture (`gui/renderer.rs`)
5. **Input**: Platform input module captures → `input/mod.rs` formats to GFN protocol → `webrtc/datachannel.rs` sends

### Key Types

- `App` (`app/mod.rs`): Central application state, handles all `UiAction` events
- `Renderer` (`gui/renderer.rs`): wgpu surface, egui integration, video frame display
- `UiAction` (`app/types.rs`): All UI events (login, game launch, settings changes)
- `Settings` (`app/config.rs`): Persistent user settings (resolution, codec, window size)
- `VideoDecoder` trait (`media/mod.rs`): Platform-agnostic decoder interface

### WebRTC Flow

The client uses a forked webrtc-rs (`webrtc-rs-gfn`) with GFN-specific SSRC handling:

1. `signaling.rs`: WebSocket connection to GFN signaling server
2. `sdp.rs`: Custom SDP generation matching GFN's expected format
3. `peer.rs`: WebRTC peer connection setup, track handling
4. `datachannel.rs`: Input protocol (Type 12 XInput format for controllers)

### Platform Abstractions

Input and video decoding have platform-specific implementations selected at compile time:

```rust
// Video decoder selection (media/video.rs)
#[cfg(target_os = "windows")]
use super::dxva_decoder::DxvaDecoder;

#[cfg(target_os = "macos")]
use super::videotoolbox::VideoToolboxDecoder;

#[cfg(target_os = "linux")]
use super::gstreamer_decoder::GStreamerDecoder;  // or VulkanDecoder
```

## Code Patterns

### Adding a New Setting

1. Add field to `Settings` struct in `app/config.rs`
2. Add default value in `impl Default for Settings`
3. Handle in `UiAction::UpdateSetting` match arm in `app/mod.rs`
4. Add UI control in `gui/renderer.rs` render_settings functions

### Adding a New UI Action

1. Add variant to `UiAction` enum in `app/types.rs`
2. Handle in `App::handle_action()` in `app/mod.rs`
3. Trigger from UI in `gui/renderer.rs` by pushing to `actions` vec

### Platform-Specific Code

Use `#[cfg(target_os = "...")]` attributes:
```rust
#[cfg(target_os = "windows")]
fn windows_only() { ... }

#[cfg(target_os = "macos")]
fn macos_only() { ... }

#[cfg(target_os = "linux")]
fn linux_only() { ... }
```

## Important Dependencies

- **wgpu 28**: Using patched version from git for External Texture support
- **egui 0.33**: Using forked version (`zortos293/egui` branch `wgpu-28`) for wgpu 28 compatibility
- **webrtc-rs**: Using forked version (`zortos293/webrtc-rs-gfn`) for GFN SSRC handling
- **GStreamer**: Used for audio on Windows/Linux, V4L2 decoding on Raspberry Pi

## Debugging

- Logs use `log` crate: `info!()`, `warn!()`, `error!()`
- Tracy profiler support: build with `--features tracy`
- Stats overlay: Press F3 during streaming
- WebRTC stats available in stats panel
