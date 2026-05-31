use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig, SupportedStreamConfigRange};
use ringbuf::{HeapRb, traits::{Consumer, Observer, Producer, Split}};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};

use crate::codec::{alac_magic_cookie_from_fmtp, AudioCodec, SessionInfo};
use crate::ntp::{ClockSync, AUDIO_LATENCY_SAMPLES};
use crate::rtp::JitterBuffer;

use std::ffi::c_void;

#[cfg(target_os = "macos")]
use coreaudio::audio_unit::{AudioUnit, IOType, SampleFormat as CoreAudioSampleFormat};
#[cfg(target_os = "macos")]
use coreaudio::audio_unit::render_callback::{self, data};

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

            // Only play clean frames — concealed/error output from FDK sounds like crackling.
            if err != 0 {
                if (0x2000..=0x2fff).contains(&err) {
                    return Err(anyhow!("aacDecoder_DecodeFrame fatal init error: 0x{:x}", err));
                }
                debug!("fdk-aac decode error (skipped): 0x{:x}", err);
                return Ok(Vec::new());
            }

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
const RING_BUFFER_SIZE: usize = 44100 * 2; // ~1 s stereo @ 44.1 kHz (enough for jitter; saves ~350 KiB)

/// AirPlay empty-audio marker (4 bytes).
const EMPTY_AUDIO_MARKER: [u8; 4] = [0x00, 0x68, 0x34, 0x00];

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// Cached AES-CBC key/IV; decryptor built per packet (IV reset — matches uxplay).
struct AesCbcSession {
    key: [u8; 16],
    iv: [u8; 16],
}

impl AesCbcSession {
    fn from_slices(key: &[u8], iv: &[u8]) -> Option<Self> {
        Some(Self {
            key: aes_key16(key)?,
            iv: aes_iv16(iv)?,
        })
    }

    #[inline]
    fn decrypt_in_place(&self, buf: &mut [u8]) {
        let decrypt_len = (buf.len() / 16) * 16;
        if decrypt_len == 0 {
            return;
        }
        if let Ok(dec) = Aes128CbcDec::new_from_slices(&self.key, &self.iv) {
            let _ = dec.decrypt_padded_mut::<NoPadding>(&mut buf[..decrypt_len]);
        }
    }
}

/// Target decoded PCM queue depth (~200 ms at the *device* sample rate).
fn target_pcm_samples(sample_rate: u32, channels: u16) -> usize {
    (sample_rate as usize * channels as usize) / 5
}

/// Pick the best CPAL output config. macOS built-in output is often 48 kHz while AirPlay is 44.1 kHz.
fn pick_output_config(
    device: &cpal::Device,
    channels: u16,
    stream_rate: u32,
) -> Result<(StreamConfig, u32)> {
    let mut f32_ranges: Vec<SupportedStreamConfigRange> = device
        .supported_output_configs()?
        .filter(|r| r.sample_format() == SampleFormat::F32 && r.channels() == channels)
        .collect();

    if f32_ranges.is_empty() {
        let default = device.default_output_config()?;
        let rate = default.sample_rate().0;
        return Ok((
            StreamConfig {
                channels: default.channels(),
                sample_rate: default.sample_rate(),
                buffer_size: cpal::BufferSize::Default,
            },
            rate,
        ));
    }

    // Prefer opening the device at the stream's native rate (no resampling).
    for range in &f32_ranges {
        if range.min_sample_rate().0 <= stream_rate && stream_rate <= range.max_sample_rate().0 {
            let config = range.with_sample_rate(cpal::SampleRate(stream_rate)).config();
            info!(
                stream_rate = stream_rate,
                device_rate = stream_rate,
                "Audio output: native stream sample rate"
            );
            return Ok((config, stream_rate));
        }
    }

    // Otherwise use the device default (typically 48000 on Mac) and resample in software.
    let default = device.default_output_config()?;
    let rate = default.sample_rate().0;
    let config = f32_ranges
        .iter()
        .find(|r| {
            r.min_sample_rate().0 <= rate
                && rate <= r.max_sample_rate().0
                && r.channels() == channels
        })
        .map(|r| r.with_sample_rate(cpal::SampleRate(rate)).config())
        .unwrap_or_else(|| StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(rate),
            buffer_size: cpal::BufferSize::Default,
        });

    if rate != stream_rate {
        info!(
            stream_rate = stream_rate,
            device_rate = rate,
            "Audio output: resampling in software (common on macOS 48 kHz speakers)"
        );
    }

    Ok((config, rate))
}

/// Linear interpolation resampler for interleaved PCM (44.1 kHz → 48 kHz, etc.).
fn resample_interleaved(
    input: &[f32],
    channels: usize,
    in_rate: u32,
    out_rate: u32,
    phase: &mut f64,
) -> Vec<f32> {
    if in_rate == out_rate || input.is_empty() || channels == 0 {
        return input.to_vec();
    }
    let in_frames = input.len() / channels;
    if in_frames < 2 {
        return input.to_vec();
    }

    let step = in_rate as f64 / out_rate as f64;
    let mut out = Vec::new();
    let mut pos = *phase;

    while pos < (in_frames - 1) as f64 {
        let i0 = pos as usize;
        let i1 = i0 + 1;
        let frac = pos - i0 as f64;
        for ch in 0..channels {
            let s0 = input[i0 * channels + ch] as f64;
            let s1 = input[i1 * channels + ch] as f64;
            out.push((s0 + (s1 - s0) * frac) as f32);
        }
        pos += step;
    }

    *phase = pos - (in_frames - 1) as f64;
    out
}

/// uxplay accepts these first payload bytes after decryption (raop_buffer.c).
fn is_valid_decrypted_payload(codec: AudioCodec, data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    match codec {
        AudioCodec::Alac => data[0] == 0x20,
        AudioCodec::AacLc | AudioCodec::AacEld => matches!(
            data[0],
            0x20 | 0x80 | 0x81 | 0x82 | 0x8c | 0x8d | 0x8e
        ),
        AudioCodec::Pcm => true,
    }
}

fn aes_key16(key: &[u8]) -> Option<[u8; 16]> {
    if key.len() >= 16 {
        let mut k = [0u8; 16];
        k.copy_from_slice(&key[..16]);
        Some(k)
    } else {
        None
    }
}

fn aes_iv16(iv: &[u8]) -> Option<[u8; 16]> {
    aes_key16(iv)
}

fn refresh_session_keys(
    session_info: &Arc<RwLock<Option<SessionInfo>>>,
    cached_aes: &mut Option<AesCbcSession>,
) {
    if cached_aes.is_some() {
        return;
    }
    if let Ok(guard) = session_info.read() {
        if let Some(si) = guard.as_ref() {
            if let (Some(key), Some(iv)) = (&si.aes_key, &si.aes_iv) {
                if let Some(session) = AesCbcSession::from_slices(key, iv) {
                    info!(
                        stream_type = ?si.stream_type,
                        "Audio AES-CBC keys loaded from session_info"
                    );
                    *cached_aes = Some(session);
                }
            }
        }
    }
}

fn make_alac_decoder(
    sample_rate: u32,
    channels: u16,
    fmtp: Option<&str>,
) -> Result<Box<dyn symphonia::core::codecs::Decoder>> {
    use symphonia::core::codecs::{CodecParameters, DecoderOptions, CODEC_TYPE_ALAC};
    let mut params = CodecParameters::new();
    params.codec = CODEC_TYPE_ALAC;
    params.sample_rate = Some(sample_rate);
    let magic_cookie = alac_magic_cookie_from_fmtp(fmtp, sample_rate, channels);
    debug!(magic_cookie = ?magic_cookie, fmtp = ?fmtp, "ALAC decoder magic cookie");
    params.extra_data = Some(magic_cookie.into_boxed_slice());
    symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map(|d| d as Box<dyn symphonia::core::codecs::Decoder>)
        .map_err(|e| anyhow!("Failed to create ALAC decoder: {}", e))
}

/// Audio pipeline state
pub struct AudioPipeline {
    /// Playout has started (prefill complete); never cleared on transient underrun.
    playout_started: bool,
    /// Sample rate
    sample_rate: u32,
    /// Number of channels
    channels: u16,
}

impl AudioPipeline {
    pub fn new() -> Self {
        AudioPipeline {
            playout_started: false,
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
    clock_sync: Arc<RwLock<ClockSync>>,
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
    let cs = clock_sync.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = run_decode_loop(jb, si, cs, codec, sample_rate, channels) {
            tracing::error!(error = %e, "Audio decode loop exited with error");
        }
    }).await?;

    Ok(())
}

/// Fade-in duration at stream start to avoid clicks from an empty PCM ring.
const STARTUP_FADE_MS: u64 = 200;

/// Sleep until this RTP timestamp's scheduled playout instant.
///
/// Latency is encoded in the anchor (`RECORD` → ref+250 ms, `SYNC` → ref=now).
fn wait_for_rtp_playout(clock_sync: &Arc<RwLock<ClockSync>>, rtp_timestamp: u32) {
    let playout = clock_sync
        .read()
        .ok()
        .and_then(|sync| sync.rtp_to_playout_time(rtp_timestamp));
    if let Some(when) = playout {
        let now = Instant::now();
        if when > now {
            std::thread::sleep(when - now);
        }
    }
}

/// Block decode when the PCM ring is too far ahead of the device callback.
fn wait_for_pcm_headroom(
    producer: &impl ringbuf::traits::Observer,
    device_rate: u32,
    channels: u16,
) {
    let target = target_pcm_samples(device_rate, channels);
    let max_queued = target * 2;
    let mut spins = 0;
    while producer.occupied_len() >= max_queued {
        spins += 1;
        if spins > 2000 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Blocking decode loop that runs on a dedicated thread.
///
/// Sets up CPAL output and continuously decodes packets from the jitter buffer.
fn run_decode_loop(
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    session_info: Arc<RwLock<Option<SessionInfo>>>,
    clock_sync: Arc<RwLock<ClockSync>>,
    mut codec: AudioCodec,
    mut sample_rate: u32,
    mut channels: u16,
) -> Result<()> {
    let mut output_rate = sample_rate;
    let mut needs_resample = false;
    let mut resample_phase = 0.0f64;

    // Lock-free SPSC ring between decode thread and audio callback.
    let rb = HeapRb::<f32>::new(RING_BUFFER_SIZE);
    let (mut producer, mut consumer) = rb.split();
    let drain_pcm = Arc::new(AtomicBool::new(false));
    let drain_cb = drain_pcm.clone();
    let fade_samples_total = Arc::new(AtomicUsize::new(0));
    let fade_samples_left = Arc::new(AtomicUsize::new(0));
    let fade_total_cb = fade_samples_total.clone();
    let fade_left_cb = fade_samples_left.clone();
    let ch = channels as usize;

    #[cfg(not(target_os = "macos"))]
    let mut hold = vec![0.0f32; ch];

    #[cfg(target_os = "macos")]
    let mut _audio_unit: Option<AudioUnit> = None;

    #[cfg(target_os = "macos")]
    {
        let host = cpal::default_host();
        let device = host.default_output_device()
            .ok_or_else(|| anyhow!("No default audio output device found"))?;

        info!(device = %device.name().unwrap_or_default(), "Using audio output device via CoreAudio");

        let mut audio_unit = AudioUnit::new(IOType::DefaultOutput)?;
        let stream_format = audio_unit.input_stream_format()?;
        if stream_format.sample_format != CoreAudioSampleFormat::F32 {
            return Err(anyhow!("CoreAudio output stream format is not f32"));
        }

        output_rate = stream_format.sample_rate as u32;
        needs_resample = output_rate != sample_rate;
        resample_phase = 0.0f64;

        let consumer = Arc::new(Mutex::new(consumer));
        let hold = Arc::new(Mutex::new(vec![0.0f32; ch]));

        type Args = render_callback::Args<data::NonInterleaved<f32>>;
        audio_unit.set_render_callback(move |args: Args| {
            let render_callback::Args { num_frames, mut data, .. } = args;
            if drain_cb.swap(false, Ordering::AcqRel) {
                if let Ok(mut consumer_lock) = consumer.lock() {
                    while consumer_lock.try_pop().is_some() {}
                }
                if let Ok(mut hold_buf) = hold.lock() {
                    hold_buf.fill(0.0);
                }
                fade_left_cb.store(0, Ordering::Release);
                fade_total_cb.store(0, Ordering::Release);
            }

            let mut hold_buf = hold.lock().unwrap_or_else(|e| e.into_inner());
            // Pre-fetch consumer lock once per callback instead of per-frame to reduce lock contention.
            let mut consumer_lock_opt = consumer.try_lock().ok();
            
            for frame_idx in 0..num_frames {
                for (channel_idx, channel_data) in data.channels_mut().enumerate() {
                    let sample = if let Some(ref mut consumer_lock) = consumer_lock_opt {
                        consumer_lock.try_pop().unwrap_or(hold_buf[channel_idx])
                    } else {
                        hold_buf[channel_idx]
                    };
                    channel_data[frame_idx] = sample;
                    hold_buf[channel_idx] = sample;
                }
                let left = fade_left_cb.load(Ordering::Relaxed);
                let total = fade_total_cb.load(Ordering::Relaxed);
                if left > 0 && total > 0 {
                    let done = total - left;
                    for channel_data in data.channels_mut() {
                        channel_data[frame_idx] *= done as f32 / total as f32;
                    }
                    fade_left_cb.fetch_sub(1, Ordering::Relaxed);
                }
            }
            // Release consumer lock explicitly before callback exits
            drop(consumer_lock_opt);
            Ok(())
        })?;

        audio_unit.start()?;
        info!("CoreAudio output stream started");
        _audio_unit = Some(audio_unit);
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Set up CPAL audio output
        let host = cpal::default_host();
        let device = host.default_output_device()
            .ok_or_else(|| anyhow!("No default audio output device found"))?;

        info!(device = %device.name().unwrap_or_default(), "Using audio output device");

        let (output_config, rate) = pick_output_config(&device, channels, sample_rate)?;
        output_rate = rate;
        needs_resample = output_rate != sample_rate;

        let stream = device.build_output_stream(
            &output_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                if drain_cb.swap(false, Ordering::AcqRel) {
                    while consumer.try_pop().is_some() {}
                    hold.fill(0.0);
                    fade_left_cb.store(0, Ordering::Release);
                    fade_total_cb.store(0, Ordering::Release);
                }
                for frame in data.chunks_mut(ch) {
                    for (i, sample) in frame.iter_mut().enumerate() {
                        if let Some(v) = consumer.try_pop() {
                            *sample = v;
                            hold[i] = v;
                        } else {
                            // Hold last sample on underrun (zeros cause audible clicks).
                            *sample = hold[i];
                        }
                        let left = fade_left_cb.load(Ordering::Relaxed);
                        let total = fade_total_cb.load(Ordering::Relaxed);
                        if left > 0 && total > 0 {
                            let done = total - left;
                            *sample *= done as f32 / total as f32;
                            fade_left_cb.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                }
            },
            move |err| {
                tracing::error!(error = %err, "Audio output stream error");
            },
            None,
        )?;

        stream.play()?;
        info!("Audio output stream started");
    }

    // Local cache for AES-CBC decryption keys to avoid locking RwLock for every single packet.
    // Audio RTP packets ALWAYS use AES-CBC, regardless of stream type (AudioOnly or Mirroring).
    // AES-CTR is only used for video mirroring data on a separate socket (see uxplay/lib/mirror_buffer.c).
    let mut cached_aes: Option<AesCbcSession> = None;

    let session_fmtp = session_info
        .read()
        .ok()
        .and_then(|g| g.as_ref().and_then(|si| si.fmtp.clone()));

    let mut alac_decoder = if codec == AudioCodec::Alac {
        Some(make_alac_decoder(
            sample_rate,
            channels,
            session_fmtp.as_deref(),
        )?)
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
    let mut last_flush_generation = 0u64;

    loop {
        refresh_session_keys(&session_info, &mut cached_aes);

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
            let fmtp = session_info
                .read()
                .ok()
                .and_then(|g| g.as_ref().and_then(|si| si.fmtp.clone()));
            alac_decoder = if codec == AudioCodec::Alac {
                Some(make_alac_decoder(sample_rate, channels, fmtp.as_deref()).expect(
                    "Failed to create ALAC decoder on format change",
                ))
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
            drain_pcm.store(true, Ordering::Release);
            resample_phase = 0.0;

            // Clear cached keys to force re-reading from session_info
            cached_aes = None;

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

            // RTSP RECORD/FLUSH/TEARDOWN cleared the buffer — wait for prefill again.
            let flush_gen = jb.flush_generation();
            if flush_gen != last_flush_generation {
                last_flush_generation = flush_gen;
                pipeline.playout_started = false;
                drain_pcm.store(true, Ordering::Release);
                fade_samples_left.store(0, Ordering::Release);
                fade_samples_total.store(0, Ordering::Release);
                resample_phase = 0.0;
                if let Ok(mut sync) = clock_sync.write() {
                    sync.reset_playout_anchor();
                }
                debug!(
                    flush_generation = flush_gen,
                    "Jitter buffer flushed — drained PCM and waiting to re-prefill"
                );
            }

            if !pipeline.playout_started && jb.is_ready() && cached_aes.is_some() {
                jb.sync_expected_to_front();
                if let Some(rtp_ts) = jb.front_rtp_timestamp() {
                    if let Ok(mut sync) = clock_sync.write() {
                        if !sync.has_rtp_anchor() {
                            sync.set_rtp_reference(rtp_ts);
                        }
                    }
                }
                let fade_len =
                    (output_rate as u64 * STARTUP_FADE_MS * ch as u64 / 1000) as usize;
                fade_samples_total.store(fade_len, Ordering::Release);
                fade_samples_left.store(fade_len, Ordering::Release);
                pipeline.playout_started = true;
                info!(
                    buf_size = jb.len(),
                    min_fill = jb.min_fill(),
                    rtp_pacing = clock_sync.read().map(|s| s.has_rtp_anchor()).unwrap_or(false),
                    sync_count = clock_sync.read().map(|s| s.sync_count()).unwrap_or(0),
                    audio_latency_samples = AUDIO_LATENCY_SAMPLES,
                    startup_fade_samples = fade_len,
                    "Jitter buffer ready — starting playout"
                );
            }

            if pipeline.playout_started {
                jb.pop_next()
            } else {
                None
            }
        }; // MutexGuard dropped here, before any sleep

        match packet {
            Some(pkt) => {
                wait_for_pcm_headroom(&producer, output_rate, channels);
                wait_for_rtp_playout(&clock_sync, pkt.timestamp);

                if pkt.payload_type == 96 && !pkt.payload.is_empty() {
                    if pkt.payload.as_slice() == EMPTY_AUDIO_MARKER {
                        continue;
                    }

                    let mut decrypted_payload = pkt.payload;
                    if let Some(aes) = &cached_aes {
                        aes.decrypt_in_place(&mut decrypted_payload);
                    }

                    if !is_valid_decrypted_payload(codec, &decrypted_payload) {
                        debug!(
                            seq = pkt.sequence,
                            first_byte = decrypted_payload.first().map(|b| format!("0x{:02x}", b)),
                            len = decrypted_payload.len(),
                            codec = %codec,
                            "Skipping packet with invalid decrypted payload"
                        );
                        continue;
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
                                        let pcm = if needs_resample {
                                            resample_interleaved(
                                                &samples,
                                                ch,
                                                sample_rate,
                                                output_rate,
                                                &mut resample_phase,
                                            )
                                        } else {
                                            samples
                                        };
                                        let total = pcm.len();
                                        let written =
                                            push_pcm_samples(&mut producer, &pcm, output_rate, channels);
                                        if written < total {
                                            debug!(
                                                written = written,
                                                total = total,
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
                        // Skip AAC-ELD "no data" marker packets (4-byte marker 0x00 0x68 0x34 0x00)
                        if decrypted_payload.len() == 4 && decrypted_payload[0] == 0x00 && decrypted_payload[1] == 0x68 {
                            debug!(seq = pkt.sequence, "Skipping AAC-ELD no-data marker packet");
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
                                        let pcm = if needs_resample {
                                            resample_interleaved(
                                                &samples,
                                                ch,
                                                sample_rate,
                                                output_rate,
                                                &mut resample_phase,
                                            )
                                        } else {
                                            samples
                                        };
                                        let total = pcm.len();
                                        let written =
                                            push_pcm_samples(&mut producer, &pcm, output_rate, channels);
                                        if written < total {
                                            debug!(
                                                written = written,
                                                total = total,
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
                        let pcm = if needs_resample {
                            resample_interleaved(
                                &samples,
                                ch,
                                sample_rate,
                                output_rate,
                                &mut resample_phase,
                            )
                        } else {
                            samples
                        };
                        let total = pcm.len();
                        let written =
                            push_pcm_samples(&mut producer, &pcm, output_rate, channels);
                        if written < total {
                            debug!(
                                written = written,
                                total = total,
                                "Ring buffer full, dropped samples"
                            );
                        }
                    }
                }
            }
            None => {
                // Prefill wait or transient underrun — CPAL outputs silence from empty ring.
                if pipeline.playout_started {
                    if producer.occupied_len() >= target_pcm_samples(output_rate, channels) * 2 {
                        std::thread::sleep(Duration::from_millis(5));
                    } else {
                        debug!("Jitter buffer underrun (outputting silence until packets arrive)");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}

/// Push decoded PCM to the ring buffer (decode thread only).
fn push_pcm_samples(
    producer: &mut impl Producer<Item = f32>,
    samples: &[f32],
    device_rate: u32,
    channels: u16,
) -> usize {
    if samples.is_empty() {
        return 0;
    }

    let target = target_pcm_samples(device_rate, channels);
    let max_queued = target * 2;
    let mut offset = 0;
    let mut spins = 0;
    while offset < samples.len() {
        if producer.vacant_len() == 0 || producer.occupied_len() >= max_queued {
            spins += 1;
            if spins > 100 {
                warn!(
                    dropped_samples = samples.len() - offset,
                    "PCM ring buffer full — dropping samples"
                );
                break;
            }
            std::thread::sleep(Duration::from_micros(500));
            continue;
        }
        spins = 0;
        offset += producer.push_slice(&samples[offset..]);
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_pipeline_new() {
        let pipeline = AudioPipeline::new();
        assert!(!pipeline.playout_started);
        assert_eq!(pipeline.sample_rate, 44100);
        assert_eq!(pipeline.channels, 2);
    }

    #[test]
    fn test_resample_44100_to_48000() {
        let mut phase = 0.0;
        // 2 frames stereo = 4 samples at 44100
        let input = vec![0.0f32, 0.0, 1.0, 1.0];
        let out = resample_interleaved(&input, 2, 44100, 48000, &mut phase);
        assert!(out.len() > 4, "48 kHz should produce more samples than 44.1 kHz input");
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
