//! RustyPlay - AirPlay Receiver Library
//!
//! This library provides the core functionality for the RustyPlay AirPlay receiver,
//! including video/audio decoding, network protocols, and rendering.

pub mod codec;
pub mod mirror;
pub mod video;

// Re-export commonly used types
pub use codec::{SessionInfo, AudioCodec, StreamType, derive_video_keys};
