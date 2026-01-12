//! ZNow Module
//!
//! Provides portable app launching functionality via GeForce NOW sessions.
//! Connects to a relay server to communicate with znow-runner executable
//! running inside the GFN VM.

pub mod api;
pub mod qr_scanner;
pub mod relay;

pub use api::ZNowApiClient;
pub use qr_scanner::QrScanner;
pub use relay::ZNowRelayClient;
