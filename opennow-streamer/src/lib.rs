//! OpenNow Streamer Library
//!
//! Core components for the native GeForce NOW streaming client.

#![recursion_limit = "256"]

pub mod app;
pub mod api;
pub mod auth;
pub mod gui;
pub mod input;
pub mod media;
pub mod webrtc;
pub mod utils;
pub mod znow;

pub use app::{App, AppState};
