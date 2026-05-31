//! Video decoding and rendering module
//!
//! This module provides FFmpeg-based H.264 decoding and platform-specific
//! video rendering for AirPlay mirroring. It replaces the GStreamer-based
//! pipeline with a native Rust implementation.

use anyhow::Result;

/// Target display rate for AirPlay screen mirroring (iPhone often sends up to 60 Hz).
pub const DEFAULT_MIRROR_FPS: u32 = 60;

/// CMTime timescale for mirror PTS (nanoseconds — matches UxPlay `video_renderer_render_buffer`).
pub const VIDEO_PTS_TIMESCALE: i32 = 1_000_000_000;

/// Represents a decoded video frame ready for display
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// Raw pixel data (YUV420P, NV12, or RGB)
    pub data: Vec<u8>,
    
    /// Frame width in pixels
    pub width: u32,
    
    /// Frame height in pixels
    pub height: u32,
    
    /// Pixel format of the decoded frame
    pub format: PixelFormat,
    
    /// Presentation timestamp (optional)
    pub timestamp: Option<i64>,
}

/// Supported pixel formats for decoded frames
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// YUV 4:2:0 planar format (most common for H.264)
    YUV420P,
    
    /// NV12 format (semi-planar, preferred by VideoToolbox)
    NV12,
    
    /// RGB 24-bit format
    RGB24,
    
    /// BGRA 32-bit format (native to macOS)
    BGRA,
}

/// Video stream configuration
#[derive(Debug, Clone)]
pub struct VideoConfig {
    /// Video width in pixels
    pub width: u32,
    
    /// Video height in pixels
    pub height: u32,
    
    /// Target frame rate (fps)
    pub frame_rate: f32,
    
    /// Whether to use hardware acceleration
    pub use_hardware: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            frame_rate: 60.0,
            use_hardware: true,
        }
    }
}

/// Platform-agnostic video renderer trait
///
/// This trait defines the interface for video rendering backends.
/// Platform-specific implementations (macOS, Linux, Windows) implement
/// this trait to provide native video display capabilities.
pub trait VideoRenderer: Send {
    /// Create a new renderer with the specified initial dimensions
    ///
    /// # Arguments
    /// * `width` - Initial window width in pixels
    /// * `height` - Initial window height in pixels
    ///
    /// # Returns
    /// A new renderer instance or an error if initialization fails
    fn new(width: u32, height: u32) -> Result<Self>
    where
        Self: Sized;
    
    /// Display a decoded frame
    ///
    /// # Arguments
    /// * `frame` - The decoded frame to display
    ///
    /// # Returns
    /// Ok(()) on success, or an error if frame presentation fails
    fn display_frame(&mut self, frame: DecodedFrame) -> Result<()>;

    /// Set expected stream/display frame rate (drives CMTime spacing on macOS).
    fn set_display_fps(&mut self, _fps: u32) {}

    /// Resize the display window
    ///
    /// # Arguments
    /// * `width` - New window width in pixels
    /// * `height` - New window height in pixels
    ///
    /// # Returns
    /// Ok(()) on success, or an error if resize fails
    fn resize(&mut self, width: u32, height: u32) -> Result<()>;
    
    /// Check if the window is still open
    ///
    /// # Returns
    /// true if the window is open, false if it has been closed
    fn is_window_open(&self) -> bool;
    
    /// Shutdown and clean up resources
    ///
    /// This method should release all allocated resources including
    /// window handles, GPU contexts, and buffer pools.
    ///
    /// # Returns
    /// Ok(()) on success, or an error if cleanup fails
    fn shutdown(&mut self) -> Result<()>;
}

// FFmpeg decoder module
pub mod ffmpeg_decoder;

// Platform-specific renderer modules
#[cfg(target_os = "macos")]
pub mod macos_renderer;

// Video pipeline coordinator
pub mod pipeline;

// Re-export FFmpeg decoder
pub use ffmpeg_decoder::FFmpegDecoder;

// Re-export platform-specific renderer
#[cfg(target_os = "macos")]
pub use macos_renderer::MacOSRenderer;

// Re-export video pipeline
pub use pipeline::VideoPipeline;
