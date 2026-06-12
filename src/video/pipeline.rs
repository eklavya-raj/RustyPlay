//! Video pipeline coordinator
//!
//! This module coordinates between the FFmpeg decoder and platform-specific renderer
//! to provide a complete video playback pipeline.

use anyhow::Result;
use std::time::Instant;

use super::{ffmpeg_decoder::FFmpegDecoder, DecodedFrame, VideoRenderer, DEFAULT_MIRROR_FPS};

#[cfg(target_os = "macos")]
use super::macos_renderer::MacOSRenderer;

/// Video pipeline that coordinates decoding and rendering
///
/// This struct manages the complete video playback pipeline, coordinating
/// between the FFmpeg decoder and the platform-specific renderer.
pub struct VideoPipeline {
    decoder: FFmpegDecoder,
    renderer: Box<dyn VideoRenderer>,
    current_width: u32,
    current_height: u32,
    frame_count: u64,
    last_frame_time: Option<Instant>,
    /// Last presentation timestamp in nanoseconds (strictly increasing, fixed frame interval).
    last_pts_ns: i64,
    /// Target display / negotiation frame rate.
    display_fps: u32,
}

impl VideoPipeline {
    /// Create a new video pipeline
    pub fn new(use_hardware_decoding: bool) -> Result<Self> {
        tracing::info!(
            hardware_decoding = use_hardware_decoding,
            "Initializing video pipeline"
        );

        let decoder = FFmpegDecoder::new(use_hardware_decoding)?;
        tracing::info!("FFmpegDecoder initialized successfully");

        // Use default dimensions - will be updated when first video frame arrives with actual device dimensions
        const DEFAULT_WIDTH: u32 = 1920;
        const DEFAULT_HEIGHT: u32 = 1080;
        
        #[cfg(target_os = "macos")]
        let mut renderer: Box<dyn VideoRenderer> = Box::new(MacOSRenderer::new(DEFAULT_WIDTH, DEFAULT_HEIGHT)?);
        renderer.set_display_fps(DEFAULT_MIRROR_FPS);
        #[cfg(target_os = "macos")]
        tracing::info!(
            display_fps = DEFAULT_MIRROR_FPS,
            window_width = DEFAULT_WIDTH,
            window_height = DEFAULT_HEIGHT,
            "MacOSRenderer created successfully with default dimensions"
        );

        #[cfg(not(target_os = "macos"))]
        compile_error!("Video rendering is currently only supported on macOS");

        tracing::info!("Video pipeline initialized successfully");

        Ok(Self {
            decoder,
            renderer,
            current_width: DEFAULT_WIDTH,
            current_height: DEFAULT_HEIGHT,
            frame_count: 0,
            last_frame_time: None,
            last_pts_ns: -1,
            display_fps: DEFAULT_MIRROR_FPS,
        })
    }

    /// Generate PTS using actual sender NTP timestamp converted to nanoseconds.
    /// This aligns video frame presentation with the sender's transmission timing,
    /// eliminating hiccups caused by fixed frame rate assumptions.
    fn pts_for_frame(&mut self, ntp_timestamp: u64) -> i64 {
        // NTP timestamp is in units of 1/2^32 seconds. Convert to nanoseconds.
        // ntp_timestamp = (seconds << 32) | fraction
        // To avoid overflow, only use upper bits for seconds; fraction precision not critical for 60Hz.
        let ntp_ns = (ntp_timestamp as i64).saturating_mul(1_000_000_000).saturating_div(1i64 << 32);
        
        // Ensure monotonically increasing to avoid display layer reordering
        if ntp_ns <= self.last_pts_ns {
            // Sender timestamp didn't advance; use fixed frame interval as fallback
            let frame_ns = (1_000_000_000i64) / self.display_fps.max(1) as i64;
            self.last_pts_ns = self.last_pts_ns.saturating_add(frame_ns);
        } else {
            self.last_pts_ns = ntp_ns;
        }
        self.last_pts_ns
    }

    /// Decode and display one AVCC access unit from the mirroring stream.
    pub fn process_avcc_packet(&mut self, avcc_packet: &[u8], ntp_timestamp: u64) -> Result<()> {
        if !self.renderer.is_window_open() {
            anyhow::bail!("Video window closed by user");
        }

        let mut frames = self.decoder.decode_avcc_packet(avcc_packet)?;
        for mut frame in frames.drain(..) {
            let pts = self.pts_for_frame(ntp_timestamp);
            frame.timestamp = Some(pts);

            self.frame_count += 1;
            self.last_frame_time = Some(Instant::now());
            tracing::debug!(
                frame_count = self.frame_count,
                width = frame.width,
                height = frame.height,
                pts_ns = pts,
                "Displayed frame"
            );
            self.renderer.display_frame(frame)?;
        }

        Ok(())
    }

    /// Process codec configuration (SPS/PPS) and handle resolution changes
    pub fn process_codec_config(&mut self, sps_pps: &[u8], width: f32, height: f32) -> Result<()> {
        let new_width = width as u32;
        let new_height = height as u32;

        tracing::info!(
            width = new_width,
            height = new_height,
            "Processing codec configuration"
        );

        self.last_pts_ns = -1;

        self.decoder.reconfigure(sps_pps)?;

        if new_width != self.current_width || new_height != self.current_height {
            tracing::info!(
                old_width = self.current_width,
                old_height = self.current_height,
                new_width = new_width,
                new_height = new_height,
                "Resolution change detected"
            );

            self.renderer.resize(new_width, new_height)?;
            self.current_width = new_width;
            self.current_height = new_height;
        }

        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<()> {
        tracing::info!(
            frame_count = self.frame_count,
            "Shutting down video pipeline"
        );
        self.renderer.shutdown()?;
        Ok(())
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    pub fn is_window_open(&self) -> bool {
        self.renderer.is_window_open()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_initialization() {
        let result = VideoPipeline::new(false);
        if result.is_ok() {
            let pipeline = result.unwrap();
            assert_eq!(pipeline.frame_count, 0);
        }
    }

    #[test]
    fn test_pts_fixed_interval() {
        let mut pipeline = VideoPipeline {
            decoder: FFmpegDecoder::new(false).unwrap(),
            renderer: Box::new(MockRenderer),
            current_width: 0,
            current_height: 0,
            frame_count: 0,
            last_frame_time: None,
            last_pts_ns: -1,
            display_fps: 60,
        };
        let step = 1_000_000_000 / 60;
        assert_eq!(pipeline.pts_for_frame(0), 0);
        assert_eq!(pipeline.pts_for_frame(999), step);
        assert_eq!(pipeline.pts_for_frame(999), step * 2);
    }

    struct MockRenderer;

    impl VideoRenderer for MockRenderer {
        fn new(_w: u32, _h: u32) -> Result<Self> {
            Ok(Self)
        }
        fn display_frame(&mut self, _frame: DecodedFrame) -> Result<()> {
            Ok(())
        }
        fn resize(&mut self, _w: u32, _h: u32) -> Result<()> {
            Ok(())
        }
        fn is_window_open(&self) -> bool {
            true
        }
        fn shutdown(&mut self) -> Result<()> {
            Ok(())
        }
    }
}
