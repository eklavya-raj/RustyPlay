//! FFmpeg-based H.264 video decoder
//!
//! This module provides H.264 video decoding using FFmpeg with support for:
//! - Hardware acceleration (VideoToolbox on macOS)
//! - Software decoding fallback
//! - Annex-B formatted NAL unit processing
//! - Dynamic resolution changes via extradata

use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi::*;
use std::ffi::CStr;
use std::ptr;
use tracing::{debug, info, warn};

use super::{DecodedFrame, PixelFormat};

/// FFmpeg H.264 decoder with hardware acceleration support
pub struct FFmpegDecoder {
    /// The opened video decoder (owns the AVCodecContext internally).
    /// Set to Some when open, None when not yet opened or after reconfig.
    video: Option<ffmpeg::decoder::Video>,
    use_hardware: bool,
    frame_count: u64,
    codec: ffmpeg::Codec,
    /// Reused across `receive_frame` calls (avoids per-frame allocation).
    decode_frame: ffmpeg::frame::Video,
}

// Safety: ffmpeg::decoder::Video is Send (it wraps AVCodecContext which is just a pointer)
unsafe impl Send for FFmpegDecoder {}

impl FFmpegDecoder {
    /// Create a new FFmpeg H.264 decoder
    pub fn new(use_hardware: bool) -> Result<Self> {
        ffmpeg::init().context("Failed to initialize FFmpeg library")?;

        let (codec, actual_use_hardware) = if use_hardware {
            #[cfg(target_os = "macos")]
            {
                match ffmpeg::decoder::find_by_name("h264_videotoolbox") {
                    Some(codec) => {
                        info!("Using VideoToolbox hardware acceleration for H.264 decoding");
                        (codec, true)
                    }
                    None => {
                        warn!("VideoToolbox unavailable, falling back to software decoding");
                        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
                            .context("H.264 software decoder not found")?;
                        (codec, false)
                    }
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                (ffmpeg::decoder::find(ffmpeg::codec::Id::H264).context("H.264 decoder not found")?, false)
            }
        } else {
            info!("Using software H.264 decoding");
            (ffmpeg::decoder::find(ffmpeg::codec::Id::H264).context("H.264 software decoder not found")?, false)
        };

        info!(
            hardware_acceleration = actual_use_hardware,
            codec_name = codec.name(),
            "FFmpeg H.264 decoder initialized"
        );

        Ok(Self {
            video: None,
            use_hardware: actual_use_hardware,
            frame_count: 0,
            codec,
            decode_frame: ffmpeg::frame::Video::empty(),
        })
    }

    pub fn is_hardware_accelerated(&self) -> bool {
        self.use_hardware
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Open a new decoder with optional avcC-format extradata.
    /// Replaces the current `video` if one was open.
    unsafe fn open_video_decoder(&mut self, extradata: Option<&[u8]>) -> Result<()> {
        // Create a new unopened codec context
        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(self.codec);

        // Set extradata via raw pointer before opening (must be avcC format for H.264)
        if let Some(data) = extradata {
            if !data.is_empty() {
                let raw_ctx = ctx.as_mut_ptr();
                // Allocate and set extradata
                let ed_ptr = av_malloc(data.len()) as *mut u8;
                if ed_ptr.is_null() {
                    anyhow::bail!("Failed to allocate extradata buffer");
                }
                ptr::copy_nonoverlapping(data.as_ptr(), ed_ptr, data.len());
                (*raw_ctx).extradata = ed_ptr;
                (*raw_ctx).extradata_size = data.len() as std::os::raw::c_int;
            }
        }

        // Open the decoder via safe API. `decoder()` consumes `ctx`, `open_as()` opens it,
        // `video()` gives us a Video decoder.
        let video = ctx
            .decoder()
            .open_as(self.codec)
            .context("Failed to open H.264 decoder")?
            .video()
            .context("Failed to get video decoder")?;

        info!(
            hardware_acceleration = self.use_hardware,
            extradata_size = extradata.map(|d| d.len()).unwrap_or(0),
            "H.264 decoder opened"
        );

        self.video = Some(video);
        Ok(())
    }

    /// Get the underlying AVCodecContext pointer for FFI calls.
    fn as_mut_ptr(&mut self) -> *mut AVCodecContext {
        // We can get the raw pointer through the Video -> Opened -> Decoder -> Context chain
        // by using the Context::as_mut_ptr() via the Deref chain.
        // But we can't directly access the inner pointer. Instead, let's just use the
        // safe APIs: video.send_packet(), video.receive_frame().
        // For extradata, we set it before opening, so no need to access raw ptr at runtime.
        ptr::null_mut()
    }

    fn ffmpeg_error_string(errnum: i32) -> String {
        let mut buf = [0i8; 2048];
        unsafe {
            av_strerror(errnum, buf.as_mut_ptr(), buf.len() as usize);
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        }
    }

    /// Decode one AVCC access unit (4-byte big-endian length-prefixed NALs).
    ///
    /// The decoder must be configured with avcC extradata (via `reconfigure`) before
    /// this is called for mirroring streams.
    pub fn decode_avcc_packet(&mut self, avcc_data: &[u8]) -> Result<Vec<DecodedFrame>> {
        if avcc_data.is_empty() {
            warn!("Received empty AVCC packet, skipping");
            return Ok(Vec::new());
        }

        if self.video.is_none() {
            unsafe { self.open_video_decoder(None)?; }
        }

        let video = self.video.as_mut().unwrap();

        let mut packet = ffmpeg::Packet::new(avcc_data.len());
        if let Some(data) = packet.data_mut() {
            data.copy_from_slice(avcc_data);
        } else {
            return Err(anyhow::anyhow!("Failed to allocate packet data"));
        }

        if let Err(e) = video.send_packet(&packet) {
            warn!(error = ?e, packet_size = avcc_data.len(), "Failed to send packet to decoder");
        }

        let mut decoded = Vec::with_capacity(2);
        loop {
            match video.receive_frame(&mut self.decode_frame) {
                Ok(()) => {
                    self.frame_count += 1;
                    if let Some(decoded_frame) = Self::frame_to_decoded(&self.decode_frame) {
                        decoded.push(decoded_frame);
                    }
                }
                Err(ffmpeg::Error::Other { errno }) if errno == 35 /* EAGAIN */ => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => {
                    warn!(error = ?e, "FFmpeg decoding error");
                    return Err(anyhow::anyhow!("Decoding error: {:?}", e));
                }
            }
        }

        Ok(decoded)
    }

    fn frame_to_decoded(frame: &ffmpeg::frame::Video) -> Option<DecodedFrame> {
        let width = frame.width();
        let height = frame.height();
        if width == 0 || height == 0 {
            return None;
        }

        let format = Self::pixel_format_from_ffmpeg(frame.format());
        let w = width as usize;
        let h = height as usize;
        let uv_width = w / 2;
        let uv_height = h / 2;
        let y_size = w * h;
        let chroma_size = if format == PixelFormat::NV12 {
            uv_width * uv_height * 2
        } else {
            uv_width * uv_height * 2
        };
        let mut data = Vec::with_capacity(y_size + chroma_size);

        let y_stride = frame.stride(0) as usize;
        let y_plane = frame.data(0);
        for row in 0..h {
            let start = row * y_stride;
            let end = start + w;
            if end <= y_plane.len() {
                data.extend_from_slice(&y_plane[start..end]);
            }
        }

        if format == PixelFormat::YUV420P || format == PixelFormat::NV12 {
            let uv_stride = frame.stride(1) as usize;
            let uv_data = frame.data(1);
            let chroma_row_size = if format == PixelFormat::NV12 {
                uv_width * 2
            } else {
                uv_width
            };
            for row in 0..uv_height {
                let start = row * uv_stride;
                let end = start + chroma_row_size;
                if end <= uv_data.len() {
                    data.extend_from_slice(&uv_data[start..end]);
                }
            }

            if format == PixelFormat::YUV420P {
                let v_stride = frame.stride(2) as usize;
                let v_data = frame.data(2);
                for row in 0..uv_height {
                    let start = row * v_stride;
                    let end = start + uv_width;
                    if end <= v_data.len() {
                        data.extend_from_slice(&v_data[start..end]);
                    }
                }
            }
        }

        Some(DecodedFrame {
            data,
            width,
            height,
            format,
            timestamp: frame.timestamp(),
        })
    }

    fn pixel_format_from_ffmpeg(pix: ffmpeg::format::Pixel) -> PixelFormat {
        match pix {
            ffmpeg::format::Pixel::YUV420P => PixelFormat::YUV420P,
            ffmpeg::format::Pixel::NV12 => PixelFormat::NV12,
            ffmpeg::format::Pixel::RGB24 => PixelFormat::RGB24,
            ffmpeg::format::Pixel::BGRA => PixelFormat::BGRA,
            _ => {
                debug!(format = ?pix, "Unrecognized pixel format, defaulting to YUV420P");
                PixelFormat::YUV420P
            }
        }
    }

    /// Reconfigure decoder with new SPS/PPS for resolution changes
    ///
    /// Builds avcC extradata from Annex-B formatted SPS/PPS data, then
    /// closes and reopens the decoder with the new extradata.
    ///
    /// # Arguments
    /// * `annexb_data` - Annex-B formatted SPS and PPS NAL units
    pub fn reconfigure(&mut self, annexb_data: &[u8]) -> Result<()> {
        info!(
            data_size = annexb_data.len(),
            "Reconfiguring decoder with new SPS/PPS"
        );

        if annexb_data.is_empty() {
            return Err(anyhow::anyhow!("Empty SPS/PPS data"));
        }

        // Parse Annex-B to extract SPS/PPS
        let mut offset = 0;
        let mut sps_list: Vec<Vec<u8>> = Vec::new();
        let mut pps_list: Vec<Vec<u8>> = Vec::new();

        while offset < annexb_data.len() {
            // Detect start code (0x00000001 or 0x000001)
            let start_code_len = if offset + 4 <= annexb_data.len()
                && annexb_data[offset] == 0
                && annexb_data[offset + 1] == 0
                && annexb_data[offset + 2] == 0
                && annexb_data[offset + 3] == 1
            {
                4
            } else if offset + 3 <= annexb_data.len()
                && annexb_data[offset] == 0
                && annexb_data[offset + 1] == 0
                && annexb_data[offset + 2] == 1
            {
                3
            } else {
                offset += 1;
                continue;
            };
            offset += start_code_len;

            let nal_start = offset;
            while offset < annexb_data.len() {
                if (offset + 4 <= annexb_data.len()
                    && annexb_data[offset] == 0
                    && annexb_data[offset + 1] == 0
                    && annexb_data[offset + 2] == 0
                    && annexb_data[offset + 3] == 1)
                    || (offset + 3 <= annexb_data.len()
                        && annexb_data[offset] == 0
                        && annexb_data[offset + 1] == 0
                        && annexb_data[offset + 2] == 1)
                {
                    break;
                }
                offset += 1;
            }

            if offset > nal_start {
                let nal = annexb_data[nal_start..offset].to_vec();
                if !nal.is_empty() {
                    match nal[0] & 0x1f {
                        7 => sps_list.push(nal),
                        8 => pps_list.push(nal),
                        _ => {}
                    }
                }
            }
        }

        if sps_list.is_empty() {
            return Err(anyhow::anyhow!("No SPS found in Annex-B data"));
        }

        // Build avcC extradata
        let mut extradata = Vec::new();
        extradata.push(1); // version
        let sps = &sps_list[0];
        extradata.push(sps.get(1).copied().unwrap_or(100)); // profile
        extradata.push(sps.get(2).copied().unwrap_or(0));   // compat
        extradata.push(sps.get(3).copied().unwrap_or(31));  // level
        extradata.push(0xFF); // lengthSizeMinusOne
        extradata.push(0xE0 | (sps_list.len().min(31) as u8)); // numSPS
        for sps in &sps_list {
            extradata.extend_from_slice(&(sps.len().min(65535) as u16).to_be_bytes());
            extradata.extend_from_slice(sps);
        }
        extradata.push(pps_list.len().min(255) as u8); // numPPS
        for pps in &pps_list {
            extradata.extend_from_slice(&(pps.len().min(65535) as u16).to_be_bytes());
            extradata.extend_from_slice(pps);
        }

        info!(
            num_sps = sps_list.len(),
            num_pps = pps_list.len(),
            extradata_size = extradata.len(),
            "Reopening decoder with new extradata"
        );

        // Drop the current video decoder (closes it) and open a new one with extradata
        self.video = None;
        unsafe {
            self.open_video_decoder(Some(&extradata))?;
        }

        info!("Decoder reconfigured successfully");
        Ok(())
    }

    /// Flush buffered frames from the decoder
    pub fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        info!("Flushing decoder");

        let video = match self.video.as_mut() {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };

        // Send EOF to signal flush
        if let Err(e) = video.send_eof() {
            warn!("Failed to send EOF: {:?}", e);
            return Ok(Vec::new());
        }

        let mut frames = Vec::new();
        let mut frame = ffmpeg::frame::Video::empty();
        while video.receive_frame(&mut frame).is_ok() {
            if let Some(decoded_frame) = Self::frame_to_decoded(&frame) {
                frames.push(decoded_frame);
            }
            frame = ffmpeg::frame::Video::empty();
        }

        info!(flushed = frames.len(), "Flush complete");
        Ok(frames)
    }
}

/// Implement Drop
impl Drop for FFmpegDecoder {
    fn drop(&mut self) {
        info!(
            frame_count = self.frame_count,
            hardware = self.use_hardware,
            "Dropping FFmpegDecoder"
        );
        // video is dropped here, which calls avcodec_free_context via the Context drop chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decoder_initialization_software() {
        let result = FFmpegDecoder::new(false);
        assert!(result.is_ok(), "Decoder should initialize");
        let d = result.unwrap();
        assert!(!d.is_hardware_accelerated());
        assert_eq!(d.frame_count(), 0);
    }

    #[test]
    fn test_decode_empty_avcc_packet() {
        let mut d = FFmpegDecoder::new(false).unwrap();
        let r = d.decode_avcc_packet(&[]);
        assert!(r.is_ok());
        assert!(r.unwrap().is_empty());
    }

    #[test]
    fn test_reconfigure_with_empty() {
        let mut d = FFmpegDecoder::new(false).unwrap();
        assert!(d.reconfigure(&[]).is_err());
    }
}