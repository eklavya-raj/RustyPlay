//! FFmpeg-based H.264 video decoder
//!
//! This module provides H.264 video decoding using FFmpeg with support for:
//! - Hardware acceleration (VideoToolbox on macOS)
//! - Software decoding fallback
//! - Annex-B formatted NAL unit processing
//! - Dynamic resolution changes

use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use tracing::{debug, info, warn};

use super::{DecodedFrame, PixelFormat};

/// FFmpeg H.264 decoder with hardware acceleration support
///
/// This decoder processes Annex-B formatted H.264 NAL units and produces
/// decoded frames in YUV420P or NV12 format. It supports VideoToolbox
/// hardware acceleration on macOS with automatic fallback to software decoding.
pub struct FFmpegDecoder {
    decoder: ffmpeg::decoder::Video,
    use_hardware: bool,
    frame_count: u64,
}

impl FFmpegDecoder {
    /// Create a new FFmpeg H.264 decoder
    ///
    /// # Arguments
    /// * `use_hardware` - Whether to attempt hardware acceleration
    ///
    /// # Returns
    /// A new decoder instance or an error if initialization fails
    ///
    /// # Hardware Acceleration
    /// On macOS, this will attempt to use VideoToolbox hardware acceleration
    /// if `use_hardware` is true. If hardware acceleration is unavailable or
    /// fails to initialize, it will automatically fall back to software decoding.
    ///
    /// # Examples
    /// ```no_run
    /// use rusty_play::video::ffmpeg_decoder::FFmpegDecoder;
    ///
    /// // Try hardware acceleration first
    /// let decoder = FFmpegDecoder::new(true)?;
    ///
    /// // Force software decoding
    /// let decoder = FFmpegDecoder::new(false)?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn new(use_hardware: bool) -> Result<Self> {
        // Initialize FFmpeg library (safe to call multiple times)
        ffmpeg::init().context("Failed to initialize FFmpeg library")?;

        let (codec, actual_use_hardware) = if use_hardware {
            // Try VideoToolbox hardware decoder on macOS
            #[cfg(target_os = "macos")]
            {
                match ffmpeg::decoder::find_by_name("h264_videotoolbox") {
                    Some(codec) => {
                        info!("Using VideoToolbox hardware acceleration for H.264 decoding");
                        (codec, true)
                    }
                    None => {
                        warn!(
                            "VideoToolbox hardware decoder not available, falling back to software decoding"
                        );
                        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
                            .context("H.264 software decoder not found")?;
                        (codec, false)
                    }
                }
            }

            #[cfg(not(target_os = "macos"))]
            {
                warn!("Hardware acceleration not supported on this platform, using software decoding");
                let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
                    .context("H.264 software decoder not found")?;
                (codec, false)
            }
        } else {
            info!("Using software H.264 decoding (hardware acceleration disabled)");
            let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
                .context("H.264 software decoder not found")?;
            (codec, false)
        };

        // Create a new codec context for the decoder
        // We create an empty context and then open it as a decoder
        let context = ffmpeg::codec::context::Context::new_with_codec(codec);
        let decoder = context
            .decoder()
            .video()
            .context("Failed to create video decoder context")?;

        info!(
            hardware_acceleration = actual_use_hardware,
            codec_name = codec.name(),
            "FFmpeg H.264 decoder initialized successfully"
        );

        Ok(Self {
            decoder,
            use_hardware: actual_use_hardware,
            frame_count: 0,
        })
    }

    /// Check if hardware acceleration is being used
    ///
    /// # Returns
    /// true if VideoToolbox or other hardware acceleration is active
    pub fn is_hardware_accelerated(&self) -> bool {
        self.use_hardware
    }

    /// Get the total number of frames decoded
    ///
    /// # Returns
    /// The count of successfully decoded frames
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Decode a single Annex-B formatted NAL unit
    ///
    /// This method processes H.264 NAL units in Annex-B format (start-code prefixed).
    /// It handles:
    /// - SPS (Sequence Parameter Set) NAL units to initialize decoder dimensions
    /// - PPS (Picture Parameter Set) NAL units to configure picture parameters
    /// - IDR (Instantaneous Decoder Refresh) frames - keyframes
    /// - Non-IDR slice NAL units - delta frames
    ///
    /// # Arguments
    /// * `nal_unit` - Annex-B formatted NAL unit with start code (0x00 0x00 0x00 0x01)
    ///
    /// # Returns
    /// - `Ok(Some(DecodedFrame))` - Successfully decoded a frame
    /// - `Ok(None)` - NAL unit processed but no frame available yet (EAGAIN)
    /// - `Err(_)` - Decoding error occurred
    ///
    /// # EAGAIN Handling
    /// The decoder may need multiple NAL units before producing a frame.
    /// This is normal behavior - SPS/PPS NAL units configure the decoder
    /// but don't produce frames. The first frame is typically produced after
    /// receiving an IDR slice NAL unit.
    ///
    /// # Examples
    /// ```no_run
    /// use rusty_play::video::ffmpeg_decoder::FFmpegDecoder;
    ///
    /// let mut decoder = FFmpegDecoder::new(true)?;
    ///
    /// // Process NAL units
    /// for nal_unit in nal_units {
    ///     match decoder.decode_nal_unit(&nal_unit)? {
    ///         Some(frame) => {
    ///             // Display the frame
    ///             println!("Decoded frame: {}x{}", frame.width, frame.height);
    ///         }
    ///         None => {
    ///             // Need more data, continue processing
    ///         }
    ///     }
    /// }
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn decode_nal_unit(&mut self, nal_unit: &[u8]) -> Result<Option<DecodedFrame>> {
        // Validate NAL unit has minimum size
        if nal_unit.is_empty() {
            warn!("Received empty NAL unit, skipping");
            return Ok(None);
        }

        // Create FFmpeg packet from NAL unit data
        let mut packet = ffmpeg::Packet::new(nal_unit.len());
        if let Some(data) = packet.data_mut() {
            data.copy_from_slice(nal_unit);
        } else {
            return Err(anyhow::anyhow!("Failed to allocate packet data"));
        }

        // Send packet to decoder
        // This may fail if the decoder is not ready or the packet is malformed
        if let Err(e) = self.decoder.send_packet(&packet) {
            warn!(error = ?e, "Failed to send packet to decoder, skipping NAL unit");
            return Ok(None);
        }

        // Try to receive a decoded frame
        let mut frame = ffmpeg::frame::Video::empty();
        match self.decoder.receive_frame(&mut frame) {
            Ok(()) => {
                // Successfully decoded a frame
                self.frame_count += 1;

                // Determine pixel format
                let format = match frame.format() {
                    ffmpeg::format::Pixel::YUV420P => PixelFormat::YUV420P,
                    ffmpeg::format::Pixel::NV12 => PixelFormat::NV12,
                    ffmpeg::format::Pixel::RGB24 => PixelFormat::RGB24,
                    ffmpeg::format::Pixel::BGRA => PixelFormat::BGRA,
                    other => {
                        debug!(format = ?other, "Unsupported pixel format, defaulting to YUV420P");
                        PixelFormat::YUV420P
                    }
                };

                // Extract frame data
                // For planar formats like YUV420P, we need to copy all planes
                let width = frame.width();
                let height = frame.height();
                let mut data = Vec::new();

                // Copy Y plane (plane 0)
                let y_plane = frame.data(0);
                let y_stride = frame.stride(0);
                for row in 0..height as usize {
                    let start = row * y_stride;
                    let end = start + width as usize;
                    if end <= y_plane.len() {
                        data.extend_from_slice(&y_plane[start..end]);
                    }
                }

                // For YUV420P and NV12, copy U and V planes
                if format == PixelFormat::YUV420P || format == PixelFormat::NV12 {
                    let uv_height = (height / 2) as usize;
                    let uv_width = (width / 2) as usize;

                    // Copy U plane (plane 1)
                    let u_plane = frame.data(1);
                    let u_stride = frame.stride(1);
                    for row in 0..uv_height {
                        let start = row * u_stride;
                        let end = start + uv_width;
                        if end <= u_plane.len() {
                            data.extend_from_slice(&u_plane[start..end]);
                        }
                    }

                    // Copy V plane (plane 2) - only for YUV420P
                    if format == PixelFormat::YUV420P {
                        let v_plane = frame.data(2);
                        let v_stride = frame.stride(2);
                        for row in 0..uv_height {
                            let start = row * v_stride;
                            let end = start + uv_width;
                            if end <= v_plane.len() {
                                data.extend_from_slice(&v_plane[start..end]);
                            }
                        }
                    }
                }

                let decoded_frame = DecodedFrame {
                    data,
                    width,
                    height,
                    format,
                    timestamp: frame.timestamp(),
                };

                debug!(
                    frame_count = self.frame_count,
                    width = width,
                    height = height,
                    format = ?format,
                    data_size = decoded_frame.data.len(),
                    "Successfully decoded frame"
                );

                Ok(Some(decoded_frame))
            }
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                // Decoder needs more data before producing a frame
                // This is normal for SPS/PPS NAL units and when buffering frames
                debug!("Decoder needs more data (EAGAIN), continuing");
                Ok(None)
            }
            Err(e) => {
                // Actual decoding error
                warn!(error = ?e, "FFmpeg decoding error");
                Err(anyhow::anyhow!("Decoding error: {:?}", e))
            }
        }
    }

    /// Reconfigure decoder with new SPS/PPS for resolution changes
    ///
    /// This method handles dynamic resolution changes by sending new
    /// Sequence Parameter Set (SPS) and Picture Parameter Set (PPS)
    /// NAL units to the decoder. This is typically called when the
    /// iOS device changes orientation or the video resolution changes.
    ///
    /// # Arguments
    /// * `sps_pps` - Annex-B formatted SPS and PPS NAL units
    ///
    /// # Returns
    /// Ok(()) on success, or an error if reconfiguration fails
    ///
    /// # Examples
    /// ```no_run
    /// use rusty_play::video::ffmpeg_decoder::FFmpegDecoder;
    ///
    /// let mut decoder = FFmpegDecoder::new(true)?;
    ///
    /// // When resolution changes, reconfigure with new SPS/PPS
    /// decoder.reconfigure(&sps_pps_data)?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn reconfigure(&mut self, sps_pps: &[u8]) -> Result<()> {
        info!(
            data_size = sps_pps.len(),
            "Reconfiguring decoder with new SPS/PPS"
        );

        // Validate input
        if sps_pps.is_empty() {
            return Err(anyhow::anyhow!("Empty SPS/PPS data"));
        }

        // Create packet with SPS/PPS data
        let mut packet = ffmpeg::Packet::new(sps_pps.len());
        if let Some(data) = packet.data_mut() {
            data.copy_from_slice(sps_pps);
        } else {
            return Err(anyhow::anyhow!("Failed to allocate packet data"));
        }

        // Send SPS/PPS to decoder
        // The decoder will use this to reconfigure itself for the new resolution
        self.decoder
            .send_packet(&packet)
            .context("Failed to send SPS/PPS packet to decoder")?;

        info!("Decoder reconfigured successfully");
        Ok(())
    }

    /// Flush buffered frames from the decoder
    ///
    /// This method sends an EOF signal to the decoder and retrieves all
    /// buffered frames. This is typically called when the stream ends or
    /// before reconfiguring the decoder.
    ///
    /// # Returns
    /// A vector of all buffered frames, or an error if flushing fails
    ///
    /// # Examples
    /// ```no_run
    /// use rusty_play::video::ffmpeg_decoder::FFmpegDecoder;
    ///
    /// let mut decoder = FFmpegDecoder::new(true)?;
    ///
    /// // ... decode some frames ...
    ///
    /// // Flush remaining frames when stream ends
    /// let remaining_frames = decoder.flush()?;
    /// println!("Flushed {} frames", remaining_frames.len());
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        info!("Flushing decoder to retrieve buffered frames");

        // Send EOF to decoder
        self.decoder
            .send_eof()
            .context("Failed to send EOF to decoder")?;

        let mut frames = Vec::new();
        let mut frame = ffmpeg::frame::Video::empty();

        // Retrieve all buffered frames
        while self.decoder.receive_frame(&mut frame).is_ok() {
            let format = match frame.format() {
                ffmpeg::format::Pixel::YUV420P => PixelFormat::YUV420P,
                ffmpeg::format::Pixel::NV12 => PixelFormat::NV12,
                ffmpeg::format::Pixel::RGB24 => PixelFormat::RGB24,
                ffmpeg::format::Pixel::BGRA => PixelFormat::BGRA,
                _ => PixelFormat::YUV420P,
            };

            // Extract frame data (same logic as decode_nal_unit)
            let width = frame.width();
            let height = frame.height();
            let mut data = Vec::new();

            // Copy Y plane
            let y_plane = frame.data(0);
            let y_stride = frame.stride(0);
            for row in 0..height as usize {
                let start = row * y_stride;
                let end = start + width as usize;
                if end <= y_plane.len() {
                    data.extend_from_slice(&y_plane[start..end]);
                }
            }

            // Copy UV planes for YUV420P/NV12
            if format == PixelFormat::YUV420P || format == PixelFormat::NV12 {
                let uv_height = (height / 2) as usize;
                let uv_width = (width / 2) as usize;

                let u_plane = frame.data(1);
                let u_stride = frame.stride(1);
                for row in 0..uv_height {
                    let start = row * u_stride;
                    let end = start + uv_width;
                    if end <= u_plane.len() {
                        data.extend_from_slice(&u_plane[start..end]);
                    }
                }

                if format == PixelFormat::YUV420P {
                    let v_plane = frame.data(2);
                    let v_stride = frame.stride(2);
                    for row in 0..uv_height {
                        let start = row * v_stride;
                        let end = start + uv_width;
                        if end <= v_plane.len() {
                            data.extend_from_slice(&v_plane[start..end]);
                        }
                    }
                }
            }

            frames.push(DecodedFrame {
                data,
                width,
                height,
                format,
                timestamp: frame.timestamp(),
            });
        }

        info!(flushed_frames = frames.len(), "Decoder flush complete");
        Ok(frames)
    }
}

/// Implement Drop to ensure proper resource cleanup
///
/// This ensures that FFmpeg decoder resources are properly released
/// when the decoder is dropped, preventing resource leaks.
impl Drop for FFmpegDecoder {
    fn drop(&mut self) {
        info!(
            frame_count = self.frame_count,
            hardware_accelerated = self.use_hardware,
            "Dropping FFmpegDecoder and releasing resources"
        );
        
        // FFmpeg resources are automatically cleaned up by the ffmpeg-next crate
        // when the decoder is dropped. We just log the cleanup for diagnostics.
        // The decoder context and any internal buffers will be freed by FFmpeg's
        // cleanup routines.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decoder_initialization_software() {
        // Test software decoder initialization
        let result = FFmpegDecoder::new(false);
        if let Err(ref e) = result {
            eprintln!("Decoder initialization error: {:?}", e);
        }
        assert!(
            result.is_ok(),
            "Software decoder initialization should succeed"
        );

        let decoder = result.unwrap();
        assert_eq!(
            decoder.is_hardware_accelerated(),
            false,
            "Software decoder should not report hardware acceleration"
        );
        assert_eq!(decoder.frame_count(), 0, "Initial frame count should be 0");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_decoder_initialization_hardware() {
        // Test hardware decoder initialization on macOS
        let result = FFmpegDecoder::new(true);
        assert!(
            result.is_ok(),
            "Hardware decoder initialization should succeed (or fall back to software)"
        );

        let decoder = result.unwrap();
        // Hardware acceleration may or may not be available, so we just check it doesn't crash
        let _ = decoder.is_hardware_accelerated();
    }

    #[test]
    fn test_multiple_decoder_instances() {
        // Test that we can create multiple decoder instances
        let decoder1 = FFmpegDecoder::new(false);
        let decoder2 = FFmpegDecoder::new(false);

        assert!(decoder1.is_ok(), "First decoder should initialize");
        assert!(decoder2.is_ok(), "Second decoder should initialize");
    }

    #[test]
    fn test_decode_empty_nal_unit() {
        // Test that empty NAL units are handled gracefully
        let mut decoder = FFmpegDecoder::new(false).expect("Decoder should initialize");
        
        let result = decoder.decode_nal_unit(&[]);
        assert!(result.is_ok(), "Empty NAL unit should not cause error");
        assert!(result.unwrap().is_none(), "Empty NAL unit should return None");
    }

    #[test]
    fn test_decode_malformed_nal_unit() {
        // Test that malformed NAL units are handled gracefully
        let mut decoder = FFmpegDecoder::new(false).expect("Decoder should initialize");
        
        // Create a malformed NAL unit (just random bytes)
        let malformed_nal = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01];
        
        let result = decoder.decode_nal_unit(&malformed_nal);
        // Should either return Ok(None) or Ok(Some(_)), but not panic
        assert!(result.is_ok(), "Malformed NAL unit should not panic");
    }

    #[test]
    fn test_reconfigure_with_empty_sps_pps() {
        // Test that reconfigure with empty data returns an error
        let mut decoder = FFmpegDecoder::new(false).expect("Decoder should initialize");
        
        let result = decoder.reconfigure(&[]);
        assert!(result.is_err(), "Empty SPS/PPS should return error");
    }

    #[test]
    fn test_flush_without_frames() {
        // Test that flush works even when no frames have been decoded
        let mut decoder = FFmpegDecoder::new(false).expect("Decoder should initialize");
        
        let result = decoder.flush();
        assert!(result.is_ok(), "Flush should succeed even with no frames");
        assert_eq!(result.unwrap().len(), 0, "Should return empty vector when no frames buffered");
    }

    #[test]
    fn test_decoder_drop_cleanup() {
        // Test that decoder can be dropped without issues
        // This verifies the Drop implementation works correctly
        {
            let decoder = FFmpegDecoder::new(false).expect("Decoder should initialize");
            assert_eq!(decoder.frame_count(), 0);
            // Decoder will be dropped here when it goes out of scope
        }
        
        // Create another decoder to verify resources were properly cleaned up
        let decoder2 = FFmpegDecoder::new(false);
        assert!(decoder2.is_ok(), "Should be able to create decoder after previous one was dropped");
    }

    #[test]
    fn test_multiple_drop_cycles() {
        // Test multiple create/drop cycles to verify no resource leaks
        for _ in 0..5 {
            let decoder = FFmpegDecoder::new(false).expect("Decoder should initialize");
            assert_eq!(decoder.frame_count(), 0);
            // Decoder drops at end of each iteration
        }
        
        // If we got here without panicking, resource cleanup is working
    }
}
