use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;
use ringbuf::{HeapRb, traits::{Consumer, Producer, Split}};
use std::sync::{Arc, Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::codec::{AudioCodec, SessionInfo};
use crate::ntp::ClockSync;
use crate::rtp::JitterBuffer;

use std::ffi::c_void;

#[allow(non_camel_case_types)]
pub type HANDLE_AACDECODER = *mut c_void;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct CStreamInfo {
    pub sample_rate: i32,
    pub frame_size: i32,
    pub num_channels: i32,
    pub p_channel_type: *const i32,
    pub p_channel_indices: *const u8,
}

unsafe extern "C" {
    pub fn aacDecoder_Open(transportFmt: u32, nrOfLayers: u32) -> HANDLE_AACDECODER;
    pub fn aacDecoder_ConfigRaw(
        self_: HANDLE_AACDECODER,
        conf: *mut *const u8,
        length: *const u32,
    ) -> u32;
    pub fn aacDecoder_Fill(
        self_: HANDLE_AACDECODER,
        pBuffer: *mut *const u8,
        bufferSize: *const u32,
        bytesValid: *mut u32,
    ) -> u32;
    pub fn aacDecoder_DecodeFrame(
        self_: HANDLE_AACDECODER,
        pTimeData: *mut i16,
        timeDataSize: i32,
        flags: u32,
    ) -> u32;
    pub fn aacDecoder_Close(self_: HANDLE_AACDECODER);
    pub fn aacDecoder_GetStreamInfo(self_: HANDLE_AACDECODER) -> *const CStreamInfo;
}

pub struct FdkAacDecoder {
    handle: HANDLE_AACDECODER,
}

unsafe impl Send for FdkAacDecoder {}

impl FdkAacDecoder {
    pub fn new(codec_data: &[u8]) -> Result<Self> {
        unsafe {
            let handle = aacDecoder_Open(0, 1); // 0 = TT_MP4_RAW
            if handle.is_null() {
                return Err(anyhow!("Failed to open fdk-aac decoder"));
            }

            let mut config_ptr = codec_data.as_ptr();
            let mut config_len = codec_data.len() as u32;
            let err = aacDecoder_ConfigRaw(handle, &mut config_ptr, &mut config_len);
            if err != 0 {
                aacDecoder_Close(handle);
                return Err(anyhow!("Failed to configure fdk-aac decoder: error code 0x{:x}", err));
            }

            Ok(FdkAacDecoder { handle })
        }
    }

    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<f32>> {
        unsafe {
            let mut bytes_valid = payload.len() as u32;
            let mut data_ptr = payload.as_ptr();
            let mut data_len = payload.len() as u32;

            let err = aacDecoder_Fill(self.handle, &mut data_ptr, &mut data_len, &mut bytes_valid);
            if err != 0 {
                return Err(anyhow!("aacDecoder_Fill failed: error 0x{:x}", err));
            }

            // Output buffer for decoded PCM samples.
            // 8192 is plenty for up to 8 channels * 1024 samples.
            let mut out_buf = vec![0i16; 8192];

            let err = aacDecoder_DecodeFrame(
                self.handle,
                out_buf.as_mut_ptr(),
                out_buf.len() as i32,
                0,
            );

            // IS_OUTPUT_VALID: either AAC_DEC_OK (0) or a decode error (0x4000..=0x4FFF) where output is concealed
            let output_valid = err == 0 || (0x4000..=0x4fff).contains(&err);

            if !output_valid {
                if (0x2000..=0x2fff).contains(&err) {
                    // Fatal initialization error
                    return Err(anyhow!("aacDecoder_DecodeFrame fatal init error: 0x{:x}", err));
                } else {
                    // Transient/sync/unknown non-fatal error (including 0x5 AAC_DEC_UNKNOWN on corrupt/mock data).
                    // Log and return empty vector.
                    debug!("fdk-aac non-fatal decode error: 0x{:x}", err);
                    return Ok(Vec::new());
                }
            }

            // If output is valid (even if err != 0, e.g. concealed decode error), retrieve samples
            let info_ptr = aacDecoder_GetStreamInfo(self.handle);
            if info_ptr.is_null() {
                return Err(anyhow!("Failed to get stream info from fdk-aac"));
            }
            let info = &*info_ptr;

            let num_samples = (info.frame_size * info.num_channels) as usize;
            if num_samples > out_buf.len() {
                return Err(anyhow!(
                    "Decoded sample count {} exceeds buffer capacity",
                    num_samples
                ));
            }

            let mut samples_f32 = Vec::with_capacity(num_samples);
            for &sample in &out_buf[..num_samples] {
                samples_f32.push(sample as f32 / 32768.0);
            }

            Ok(samples_f32)
        }
    }
}

impl Drop for FdkAacDecoder {
    fn drop(&mut self) {
        unsafe {
            aacDecoder_Close(self.handle);
        }
    }
}


/// Size of the ring buffer between decoder and audio output (in f32 samples)
const RING_BUFFER_SIZE: usize = 44100 * 2 * 2; // ~2 seconds of stereo audio at 44.1kHz

/// Audio pipeline state
pub struct AudioPipeline {
    /// Whether the pipeline is currently active
    active: bool,
    /// Sample rate
    sample_rate: u32,
    /// Number of channels
    channels: u16,
}

impl AudioPipeline {
    pub fn new() -> Self {
        AudioPipeline {
            active: false,
            sample_rate: 44100,
            channels: 2,
        }
    }
}

/// Convert a Symphonia AudioBufferRef to f32 samples.
fn convert_audio_buffer(buf: &symphonia::core::audio::AudioBufferRef) -> Vec<f32> {
    use symphonia::core::audio::Signal;
    use symphonia::core::conv::FromSample;

    match buf {
        symphonia::core::audio::AudioBufferRef::F32(b) => {
            // Already f32, just interleave channels
            let channels = b.spec().channels.count();
            let frames = b.frames();
            let mut output = Vec::with_capacity(frames * channels);
            for frame in 0..frames {
                for ch in 0..channels {
                    output.push(b.chan(ch)[frame]);
                }
            }
            output
        }
        symphonia::core::audio::AudioBufferRef::S16(b) => {
            let channels = b.spec().channels.count();
            let frames = b.frames();
            let mut output = Vec::with_capacity(frames * channels);
            for frame in 0..frames {
                for ch in 0..channels {
                    output.push(f32::from_sample(b.chan(ch)[frame]));
                }
            }
            output
        }
        symphonia::core::audio::AudioBufferRef::S32(b) => {
            let channels = b.spec().channels.count();
            let frames = b.frames();
            let mut output = Vec::with_capacity(frames * channels);
            for frame in 0..frames {
                for ch in 0..channels {
                    output.push(f32::from_sample(b.chan(ch)[frame]));
                }
            }
            output
        }
        symphonia::core::audio::AudioBufferRef::U8(b) => {
            let channels = b.spec().channels.count();
            let frames = b.frames();
            let mut output = Vec::with_capacity(frames * channels);
            for frame in 0..frames {
                for ch in 0..channels {
                    output.push(f32::from_sample(b.chan(ch)[frame]));
                }
            }
            output
        }
        _ => {
            warn!("Unsupported audio buffer format");
            Vec::new()
        }
    }
}

/// Run the audio pipeline: consume from jitter buffer, decode, output to speakers.
///
/// This is the main audio processing loop. It:
/// 1. Waits for the jitter buffer to fill to a minimum level
/// 2. Pops packets and decodes them
/// 3. Writes decoded PCM to a ring buffer
/// 4. A CPAL audio callback reads from the ring buffer and plays audio
///
/// The CPAL stream and decode loop run on a dedicated blocking thread
/// because `cpal::Stream` is `!Send`.
pub async fn run_audio_pipeline(
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    session_info: Arc<RwLock<Option<SessionInfo>>>,
    _clock_sync: Arc<RwLock<ClockSync>>,
) -> Result<()> {
    info!("Audio pipeline starting — waiting for session info");

    // Wait for a session to be established (RTSP ANNOUNCE)
    let (codec, sample_rate, channels, _has_aes_key) = loop {
        {
            let info = session_info.read().unwrap();
            if let Some(ref si) = *info {
                let has_key = si.aes_key.is_some() && si.aes_iv.is_some();
                info!(
                    has_aes_key = has_key,
                    key_len = si.aes_key.as_ref().map(|k| k.len()).unwrap_or(0),
                    iv_len = si.aes_iv.as_ref().map(|v| v.len()).unwrap_or(0),
                    "Session info extracted"
                );
                break (si.codec, si.sample_rate, si.channels, has_key);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };

    info!(
        codec = %codec,
        sample_rate = sample_rate,
        channels = channels,
        "Audio pipeline configured"
    );

    // Move the CPAL setup and decode loop to a blocking thread.
    // cpal::Stream is !Send, so it cannot live across .await points
    // in a tokio::spawn task.
    let jb = jitter_buffer.clone();
    let si = session_info.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = run_decode_loop(jb, si, codec, sample_rate, channels) {
            tracing::error!(error = %e, "Audio decode loop exited with error");
        }
    }).await?;

    Ok(())
}

/// Blocking decode loop that runs on a dedicated thread.
///
/// Sets up CPAL output and continuously decodes packets from the jitter buffer.
fn run_decode_loop(
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    session_info: Arc<RwLock<Option<SessionInfo>>>,
    mut codec: AudioCodec,
    mut sample_rate: u32,
    mut channels: u16,
) -> Result<()> {
    // Set up CPAL audio output
    let host = cpal::default_host();
    let device = host.default_output_device()
        .ok_or_else(|| anyhow!("No default audio output device found"))?;

    info!(device = %device.name().unwrap_or_default(), "Using audio output device");

    let config = StreamConfig {
        channels,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Fixed(256),
    };

    // Create a lock-free ring buffer for decoded audio
    let rb = HeapRb::<f32>::new(RING_BUFFER_SIZE);
    let (mut producer, mut consumer) = rb.split();

    // Start the CPAL output stream
    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            // Fill the output buffer from the ring buffer
            for sample in data.iter_mut() {
                *sample = consumer.try_pop().unwrap_or(0.0);
            }
        },
        move |err| {
            tracing::error!(error = %err, "Audio output stream error");
        },
        None,
    )?;

    stream.play()?;
    info!("Audio output stream started");

    // Local cache for AES-CBC decryption keys to avoid locking RwLock for every single packet.
    // Audio RTP packets ALWAYS use AES-CBC, regardless of stream type (AudioOnly or Mirroring).
    // AES-CTR is only used for video mirroring data on a separate socket (see uxplay/lib/mirror_buffer.c).
    let mut cached_aes_key: Option<Vec<u8>> = None;
    let mut cached_aes_iv: Option<Vec<u8>> = None;

    // Initialize ALAC decoder
    let mut alac_decoder = if codec == AudioCodec::Alac {
        use symphonia::core::codecs::{CodecParameters, DecoderOptions, CODEC_TYPE_ALAC};
        let mut params = CodecParameters::new();
        params.codec = CODEC_TYPE_ALAC;
        params.sample_rate = Some(sample_rate);
        
        // ALAC ALACSpecificConfig (24 bytes, from the ALAC spec):
        // Format: [frame_length (4), compatible_version (1), bit_depth (1), pb (1), mb (1), kb (1),
        //          num_channels (1), max_run (2), max_frame_bytes (4), avg_bit_rate (4), sample_rate (4)]
        //
        // These values come from the SDP fmtp line:
        //   "352 0 16 40 10 14 2 255 0 0 44100"
        //     ^0  ^1 ^2 ^3 ^4 ^5 ^6 ^7 ^8 ^9 ^10
        //     frame_len  bits pb mb kb ch  maxrun maxframebytes avgbitrate samplerate
        //
        // NOTE: avg_bit_rate field comes BEFORE sample_rate in the binary layout.
        let sr_bytes = sample_rate.to_be_bytes();
        let magic_cookie = vec![
            0x00, 0x00, 0x01, 0x60,           // frame_length: 352 samples
            0x00,                              // compatible_version: 0
            0x10,                              // bit_depth: 16
            0x28,                              // pb: 40
            0x0a,                              // mb: 10
            0x0e,                              // kb: 14
            0x02,                              // num_channels: 2
            0x00, 0xff,                        // max_run: 255
            0x00, 0x00, 0x00, 0x00,            // max_frame_bytes: 0 (unknown)
            0x00, 0x00, 0x00, 0x00,            // avg_bit_rate: 0
            sr_bytes[0], sr_bytes[1], sr_bytes[2], sr_bytes[3], // sample_rate: from SDP
        ];
        info!(magic_cookie = ?magic_cookie, sample_rate = sample_rate, "ALAC magic cookie");
        params.extra_data = Some(magic_cookie.into_boxed_slice());
        
        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| anyhow!("Failed to create ALAC decoder: {}", e))?;
        Some(decoder)
    } else {
        None
    };

    // Initialize AAC decoder for AAC-LC and AAC-ELD using native FDK-AAC
    let mut aac_decoder = if codec == AudioCodec::AacLc || codec == AudioCodec::AacEld {
        let codec_data = match codec {
            AudioCodec::AacLc => vec![0x12, 0x10],
            AudioCodec::AacEld => vec![0xf8, 0xe8, 0x50, 0x00],
            _ => vec![],
        };
        let decoder = FdkAacDecoder::new(&codec_data)
            .map_err(|e| anyhow!("Failed to create FDK-AAC decoder: {}", e))?;
        Some(decoder)
    } else {
        None
    };

    info!(
        alac_decoder = alac_decoder.is_some(),
        aac_decoder = aac_decoder.is_some(),
        "Decoder initialization status"
    );

    // Main decode loop (blocking)
    let mut pipeline = AudioPipeline::new();
    pipeline.sample_rate = sample_rate;
    pipeline.channels = channels;

    loop {
        // Check for dynamic audio format changes in session_info
        let mut format_changed = false;
        if let Ok(info_guard) = session_info.read() {
            if let Some(ref si) = *info_guard {
                if si.codec != codec || si.sample_rate != sample_rate || si.channels != channels {
                    info!(
                        old_codec = %codec,
                        new_codec = %si.codec,
                        old_sr = sample_rate,
                        new_sr = si.sample_rate,
                        old_ch = channels,
                        new_ch = si.channels,
                        "Audio format changed dynamically"
                    );
                    codec = si.codec;
                    sample_rate = si.sample_rate;
                    channels = si.channels;
                    format_changed = true;
                }
            }
        }

        if format_changed {
            // Re-initialize ALAC decoder
            alac_decoder = if codec == AudioCodec::Alac {
                use symphonia::core::codecs::{CodecParameters, DecoderOptions, CODEC_TYPE_ALAC};
                let mut params = CodecParameters::new();
                params.codec = CODEC_TYPE_ALAC;
                params.sample_rate = Some(sample_rate);
                
                let sr_bytes = sample_rate.to_be_bytes();
                let magic_cookie = vec![
                    0x00, 0x00, 0x01, 0x60, // frame_length: 352 samples
                    0x00,                   // compatible_version: 0
                    0x10,                   // bit_depth: 16
                    0x28,                   // pb: 40
                    0x0a,                   // mb: 10
                    0x0e,                   // kb: 14
                    0x02,                   // num_channels: 2
                    0x00, 0xff,             // max_run: 255
                    0x00, 0x00, 0x00, 0x00, // max_frame_bytes: 0 (unknown)
                    0x00, 0x00, 0x00, 0x00, // avg_bit_rate: 0
                    sr_bytes[0], sr_bytes[1], sr_bytes[2], sr_bytes[3],
                ];
                params.extra_data = Some(magic_cookie.into_boxed_slice());
                
                let decoder = symphonia::default::get_codecs()
                    .make(&params, &DecoderOptions::default())
                    .expect("Failed to create ALAC decoder on format change");
                Some(decoder)
            } else {
                None
            };

            // Re-initialize AAC decoder using native FDK-AAC
            aac_decoder = if codec == AudioCodec::AacLc || codec == AudioCodec::AacEld {
                let codec_data = match codec {
                    AudioCodec::AacLc => vec![0x12, 0x10],
                    AudioCodec::AacEld => vec![0xf8, 0xe8, 0x50, 0x00],
                    _ => vec![],
                };
                let decoder = FdkAacDecoder::new(&codec_data)
                    .expect("Failed to create FDK-AAC decoder on format change");
                Some(decoder)
            } else {
                None
            };

            pipeline.sample_rate = sample_rate;
            pipeline.channels = channels;
            
            // Clear cached keys to force re-reading from session_info
            cached_aes_key = None;
            cached_aes_iv = None;

            info!(
                codec = %codec,
                sample_rate = sample_rate,
                channels = channels,
                "Decoders re-initialized successfully"
            );
        }

        // Check if jitter buffer has enough data
        let packet = {
            let mut jb = jitter_buffer.lock().unwrap();
            if !pipeline.active
                && jb.is_ready() {
                    pipeline.active = true;
                    info!(
                        buf_size = jb.len(),
                        "Jitter buffer ready — starting playout"
                    );
                }
            if pipeline.active {
                jb.pop()
            } else {
                None
            }
        }; // MutexGuard dropped here, before any sleep

        match packet {
            Some(pkt) => {
                info!(
                    seq = pkt.sequence,
                    payload_type = pkt.payload_type,
                    payload_len = pkt.payload.len(),
                    codec = %codec,
                    first_bytes = format!("{:02x} {:02x} {:02x} {:02x}", 
                        pkt.payload.get(0).unwrap_or(&0),
                        pkt.payload.get(1).unwrap_or(&0),
                        pkt.payload.get(2).unwrap_or(&0),
                        pkt.payload.get(3).unwrap_or(&0)),
                    "Received packet from jitter buffer"
                );
                // Only process audio packets (PT=96 typically)
                // Note: 4-byte payloads ARE valid ALAC frames (e.g. silence), do not skip them
                if pkt.payload_type == 96 && !pkt.payload.is_empty() {
                    // Skip empty packet markers (matching uxplay behavior)
                    // Empty packet marker: 0x00 0x68 0x34 0x00
                    if pkt.payload.len() == 4 
                        && pkt.payload[0] == 0x00 
                        && pkt.payload[1] == 0x68 
                        && pkt.payload[2] == 0x34 
                        && pkt.payload[3] == 0x00 {
                        debug!(seq = pkt.sequence, "Skipping empty packet marker");
                        continue;
                    }
                    
                    let mut decrypted_payload = pkt.payload.clone();

                    // Fetch audio AES key/IV from session_info if not yet cached
                    if cached_aes_key.is_none() {
                        if let Ok(info_guard) = session_info.read() {
                            if let Some(ref si) = *info_guard {
                                if si.aes_key.is_some() {
                                    cached_aes_key = si.aes_key.clone();
                                    cached_aes_iv = si.aes_iv.clone();
                                    info!(
                                        has_audio_key = cached_aes_key.is_some(),
                                        has_audio_iv = cached_aes_iv.is_some(),
                                        stream_type = ?si.stream_type,
                                        "Audio AES-CBC keys loaded from session_info"
                                    );
                                } else {
                                    warn!("session_info has no AES key yet — audio cannot be decrypted");
                                }
                            }
                        }
                    }

                    // AES-CBC decryption for audio packets (ALWAYS, regardless of stream type).
                    // This matches uxplay's raop_buffer.c which always uses aes_cbc_decrypt.
                    // AES-CTR is only for video mirroring data on a separate socket (mirror_buffer.c).
                    //
                    // IMPORTANT: The IV must be reset to the original value for EACH packet
                    // (matching uxplay's aes_cbc_reset per packet in raop_buffer.c)
                    //
                    // NOTE: We process ALL payload sizes, including < 16 bytes.
                    // For small payloads (e.g., 4-byte ALAC silence frames):
                    //   - encrypted_len = (4 / 16) * 16 = 0 → no-op
                    //   - All bytes remain as-is ("remainder")
                    // This matches the reference implementation in uxplay/lib/raop_buffer.c:137
                    if let (Some(key), Some(iv)) = (&cached_aes_key, &cached_aes_iv) {
                        use cbc::cipher::KeyIvInit;
                        type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
                        // A new Decryptor is created each time, which resets the IV — correct per-packet behavior
                        if let Ok(decryptor) = Aes128CbcDec::new_from_slices(key, iv) {
                            use cbc::cipher::block_padding::NoPadding;
                            use cbc::cipher::BlockDecryptMut;
                            let decrypt_len = (decrypted_payload.len() / 16) * 16;
                            
                            // Only attempt decryption if there are complete 16-byte blocks
                            if decrypt_len > 0 {
                                let _ = decryptor.decrypt_padded_mut::<NoPadding>(&mut decrypted_payload[..decrypt_len]);
                            }
                            // Remainder bytes (if any) are already in place and don't need copying
                            
                            debug!(
                                seq = pkt.sequence,
                                payload_len = decrypted_payload.len(),
                                decrypt_len = decrypt_len,
                                "AES-CBC decryption performed"
                            );
                        } else {
                            warn!(key_len=key.len(), iv_len=iv.len(), "AES-128-CBC decryptor init failed — bad key/iv length");
                        }
                    }
                    
                    // Validate decrypted ALAC frames (Requirement 9.1)
                    // Log first byte after decryption — uxplay expects 0x20 for valid ALAC
                    if codec == AudioCodec::Alac && !decrypted_payload.is_empty() {
                        let first_byte = decrypted_payload[0];
                        let is_valid_alac = first_byte == 0x20;
                        if !is_valid_alac {
                            warn!(
                                first_byte = format!("0x{:02x}", first_byte),
                                payload_len = decrypted_payload.len(),
                                "Decrypted ALAC frame does NOT start with 0x20 — possible decryption failure"
                            );
                        }
                    }

                    if codec == AudioCodec::Alac {
                        if let Some(ref mut decoder) = alac_decoder {
                            // All payload sizes are valid ALAC frames (4 bytes = silence, larger = audio)
                            let symphonia_packet = symphonia::core::formats::Packet::new_from_slice(
                                0,
                                pkt.sequence as u64,
                                352,
                                &decrypted_payload,
                            );
                            match decoder.decode(&symphonia_packet) {
                                Ok(buf) => {
                                    let samples = convert_audio_buffer(&buf);
                                    if !samples.is_empty() {
                                        let written = producer.push_slice(&samples);
                                        if written < samples.len() {
                                            debug!(
                                                written = written,
                                                total = samples.len(),
                                                "Ring buffer full, dropped samples"
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        seq = pkt.sequence,
                                        payload_len = decrypted_payload.len(),
                                        error = %e,
                                        "ALAC decode failed"
                                    );
                                }
                            }
                        } else {
                            warn!("ALAC decoder is None — codec was not initialized!");
                        }
                    } else if codec == AudioCodec::AacLc || codec == AudioCodec::AacEld {
                        info!("Attempting AAC decode");
                        // Skip AAC-ELD "no data" marker packets (4-byte marker 0x00 0x68 0x34 0x00)
                        if decrypted_payload.len() == 4 && decrypted_payload[0] == 0x00 && decrypted_payload[1] == 0x68 {
                            info!("Skipping AAC-ELD no-data marker packet");
                            continue;
                        }
                        if let Some(ref mut decoder) = aac_decoder {
                            match decoder.decode(&decrypted_payload) {
                                Ok(samples) => {
                                    // Log sample statistics to check if they're reasonable
                                    if !samples.is_empty() {
                                        let max_sample = samples.iter().cloned().fold(f32::NAN, f32::max);
                                        let min_sample = samples.iter().cloned().fold(f32::NAN, f32::min);
                                        debug!(
                                            samples = samples.len(),
                                            min = min_sample,
                                            max = max_sample,
                                            "AAC decode successful"
                                        );
                                        let written = producer.push_slice(&samples);
                                        if written < samples.len() {
                                            debug!(
                                                written = written,
                                                total = samples.len(),
                                                "Ring buffer full, dropped samples"
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    debug!(
                                        seq = pkt.sequence,
                                        error = %e,
                                        "Failed to decode AAC packet"
                                    );
                                }
                            }
                        } else {
                            debug!("AAC decoder is None!");
                        }
                    } else if codec == AudioCodec::Pcm {
                        // PCM is already decoded, just convert to f32
                        let samples: Vec<f32> = decrypted_payload
                            .chunks_exact(2)
                            .map(|chunk| {
                                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                                sample as f32 / 32768.0
                            })
                            .collect();
                        let written = producer.push_slice(&samples);
                        if written < samples.len() {
                            debug!(
                                written = written,
                                total = samples.len(),
                                "Ring buffer full, dropped samples"
                            );
                        }
                    }
                }
            }
            None => {
                // Buffer underrun or not yet ready — brief sleep
                if pipeline.active {
                    debug!("Jitter buffer underrun");
                    pipeline.active = false;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_pipeline_new() {
        let pipeline = AudioPipeline::new();
        assert!(!pipeline.active);
        assert_eq!(pipeline.sample_rate, 44100);
        assert_eq!(pipeline.channels, 2);
    }

    #[test]
    fn test_ring_buffer_basics() {
        let rb = HeapRb::<f32>::new(1024);
        let (mut prod, mut cons) = rb.split();

        let data = vec![1.0f32, 2.0, 3.0, 4.0];
        assert_eq!(prod.push_slice(&data), 4);

        assert_eq!(cons.try_pop(), Some(1.0));
        assert_eq!(cons.try_pop(), Some(2.0));
        assert_eq!(cons.try_pop(), Some(3.0));
        assert_eq!(cons.try_pop(), Some(4.0));
        assert_eq!(cons.try_pop(), None);
    }

    #[test]
    fn test_fdk_aac_decoder_init_success() {
        let dec_lc = FdkAacDecoder::new(&[0x12, 0x10]);
        assert!(dec_lc.is_ok());

        let dec_eld = FdkAacDecoder::new(&[0xf8, 0xe8, 0x50, 0x00]);
        assert!(dec_eld.is_ok());
    }

    #[test]
    fn test_fdk_aac_decoder_init_invalid() {
        let dec = FdkAacDecoder::new(&[0x00, 0x00]);
        assert!(dec.is_err());
    }

    #[test]
    fn test_fdk_aac_decoder_transient_error_handling() {
        let mut dec = FdkAacDecoder::new(&[0xf8, 0xe8, 0x50, 0x00]).unwrap();
        let res = dec.decode(&[0x01, 0x02, 0x03]);
        assert!(res.is_ok());
        assert!(res.unwrap().is_empty());
    }
}

// Bug condition exploration tests
#[cfg(test)]
#[path = "audio_decryption_test.rs"]
mod audio_decryption_test;
