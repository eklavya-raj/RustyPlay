use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Sha512, Digest};
use tracing::info;

/// Audio codec types supported by AirPlay
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Alac,
    AacLc,
    AacEld,
    Pcm,
}

impl std::fmt::Display for AudioCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioCodec::Alac => write!(f, "ALAC"),
            AudioCodec::AacLc => write!(f, "AAC-LC"),
            AudioCodec::AacEld => write!(f, "AAC-ELD"),
            AudioCodec::Pcm => write!(f, "PCM"),
        }
    }
}

/// Stream type for AirPlay sessions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    /// Audio-only stream (type 96)
    AudioOnly,
    /// Screen mirroring stream (type 110)
    Mirroring,
}

impl Default for StreamType {
    fn default() -> Self {
        StreamType::AudioOnly
    }
}

/// Video encryption keys derived from audio AES key and streamConnectionID
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoKeys {
    /// 32-byte AES-256 key for video stream decryption
    pub key: [u8; 32],
    /// 32-byte initialization vector for video stream decryption
    pub iv: [u8; 32],
}

/// Derive video decryption keys for AirPlay 2 mirroring mode (stream type 110).
///
/// This function implements the AirPlay 2 key derivation scheme:
/// - Video AES Key = SHA256("AirPlayStreamKey{streamConnectionID}" || audio_aes_key)
/// - Video AES IV = SHA256("AirPlayStreamIV{streamConnectionID}" || audio_aes_key)
///
/// # Arguments
/// * `audio_aes_key` - The 16-byte audio AES key obtained from PlayFair decryption
/// * `stream_connection_id` - The 64-bit stream connection ID from the SETUP request
///
/// # Returns
/// A `VideoKeys` struct containing the derived 32-byte key and IV
///
/// # Example
/// ```
/// let audio_key = [0u8; 16];
/// let stream_id = 12345678901234567890u64;
/// let video_keys = derive_video_keys(&audio_key, stream_id);
/// ```
pub fn derive_video_keys(audio_aes_key: &[u8], stream_connection_id: u64) -> VideoKeys {
    // Derive video AES key
    let key_string = format!("AirPlayStreamKey{}", stream_connection_id);
    let mut hasher = Sha512::new();
    hasher.update(key_string.as_bytes());
    hasher.update(audio_aes_key);
    let key_hash = hasher.finalize();
    let mut key_arr = [0u8; 32];
    key_arr.copy_from_slice(&key_hash[..32]);
    
    // Derive video IV
    let iv_string = format!("AirPlayStreamIV{}", stream_connection_id);
    let mut hasher = Sha512::new();
    hasher.update(iv_string.as_bytes());
    hasher.update(audio_aes_key);
    let iv_hash = hasher.finalize();
    let mut iv_arr = [0u8; 32];
    iv_arr.copy_from_slice(&iv_hash[..32]);
    
    let video_keys = VideoKeys {
        key: key_arr,
        iv: iv_arr,
    };
    
    // Log first 8 bytes of derived keys for debugging
    info!(
        stream_connection_id = stream_connection_id,
        video_key_prefix = format!("{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            video_keys.key[0], video_keys.key[1], video_keys.key[2], video_keys.key[3],
            video_keys.key[4], video_keys.key[5], video_keys.key[6], video_keys.key[7]),
        video_iv_prefix = format!("{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            video_keys.iv[0], video_keys.iv[1], video_keys.iv[2], video_keys.iv[3],
            video_keys.iv[4], video_keys.iv[5], video_keys.iv[6], video_keys.iv[7]),
        "Derived video keys for AirPlay 2 mirroring"
    );
    
    video_keys
}

/// Session information extracted from RTSP ANNOUNCE SDP
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    pub aes_key: Option<Vec<u8>>,
    pub aes_iv: Option<Vec<u8>>,
    pub video_aes_key: Option<Vec<u8>>,
    pub video_aes_iv: Option<Vec<u8>>,
    pub stream_type: StreamType,
    pub fmtp: Option<String>,
    pub min_latency: Option<u32>,
    pub max_latency: Option<u32>,
}

impl Default for SessionInfo {
    fn default() -> Self {
        SessionInfo {
            codec: AudioCodec::Alac,
            sample_rate: 44100,
            channels: 2,
            aes_key: None,
            aes_iv: None,
            video_aes_key: None,
            video_aes_iv: None,
            stream_type: StreamType::AudioOnly,
            fmtp: None,
            min_latency: None,
            max_latency: None,
        }
    }
}

/// Parse an SDP body from an RTSP ANNOUNCE request.
///
/// Extracts codec type, sample rate, channels, encryption keys (AES),
/// and codec-specific parameters (fmtp).
pub fn parse_sdp(body: &str) -> Result<SessionInfo> {
    let mut info = SessionInfo::default();

    for line in body.lines() {
        let line = line.trim();

        // Parse rtpmap: e.g. "a=rtpmap:96 AppleLossless" or "a=rtpmap:96 mpeg4-generic/44100/2"
        if let Some(rtpmap_val) = line.strip_prefix("a=rtpmap:") {
            let parts: Vec<&str> = rtpmap_val.splitn(2, ' ').collect();
            if parts.len() >= 2 {
                let encoding = parts[1];
                if encoding.starts_with("AppleLossless") {
                    info.codec = AudioCodec::Alac;
                    info!(codec = %info.codec, "SDP: detected ALAC codec");
                } else if encoding.starts_with("mpeg4-generic") || encoding.starts_with("MP4A-LATM") {
                    // Parse sample rate and channels from encoding/clock/channels
                    let enc_parts: Vec<&str> = encoding.split('/').collect();
                    if encoding.contains("eld") || encoding.contains("ELD") {
                        info.codec = AudioCodec::AacEld;
                    } else {
                        info.codec = AudioCodec::AacLc;
                    }
                    if enc_parts.len() >= 2
                        && let Ok(sr) = enc_parts[1].parse::<u32>() {
                            info.sample_rate = sr;
                        }
                    if enc_parts.len() >= 3
                        && let Ok(ch) = enc_parts[2].parse::<u16>() {
                            info.channels = ch;
                        }
                    info!(codec = %info.codec, sample_rate = info.sample_rate, channels = info.channels, "SDP: detected AAC codec");
                } else if encoding.starts_with("L16") {
                    info.codec = AudioCodec::Pcm;
                    let enc_parts: Vec<&str> = encoding.split('/').collect();
                    if enc_parts.len() >= 2
                        && let Ok(sr) = enc_parts[1].parse::<u32>() {
                            info.sample_rate = sr;
                        }
                    if enc_parts.len() >= 3
                        && let Ok(ch) = enc_parts[2].parse::<u16>() {
                            info.channels = ch;
                        }
                    info!(codec = %info.codec, "SDP: detected PCM codec");
                }
            }
        }

        // Parse fmtp (codec-specific parameters)
        // e.g. "a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100"
        if let Some(fmtp_val) = line.strip_prefix("a=fmtp:") {
            let parts: Vec<&str> = fmtp_val.splitn(2, ' ').collect();
            if parts.len() >= 2 {
                info.fmtp = Some(parts[1].to_string());
                info!(fmtp = %parts[1], "SDP: codec parameters");

                // For ALAC, parse sample rate from fmtp if not set from rtpmap
                // ALAC fmtp: "352 0 16 40 10 14 2 255 0 0 44100"
                //                                          ^^^^^ sample rate
                if info.codec == AudioCodec::Alac {
                    let fmtp_parts: Vec<&str> = parts[1].split_whitespace().collect();
                    if fmtp_parts.len() >= 11
                        && let Ok(sr) = fmtp_parts[10].parse::<u32>() {
                            info.sample_rate = sr;
                        }
                    // channels from fmtp[6]
                    if fmtp_parts.len() >= 7
                        && let Ok(ch) = fmtp_parts[6].parse::<u16>() {
                            info.channels = ch;
                        }
                }
            }
        }

        // Parse AES key: "a=fpaeskey:<base64>"
        if let Some(key_b64) = line.strip_prefix("a=fpaeskey:") {
            match BASE64.decode(key_b64.trim()) {
                Ok(key_bytes) => {
                    info!(key_len = key_bytes.len(), "SDP: AES key decoded");
                    info.aes_key = Some(key_bytes);
                }
                Err(e) => {
                    tracing::warn!("Failed to decode AES key: {}", e);
                }
            }
        } else if line.contains("fpaeskey") {
            info!(line = %line, "SDP: Found fpaeskey line");
        }

        // Parse AES IV: "a=aesiv:<base64>"
        if let Some(iv_b64) = line.strip_prefix("a=aesiv:") {
            match BASE64.decode(iv_b64.trim()) {
                Ok(iv_bytes) => {
                    info!(iv_len = iv_bytes.len(), "SDP: AES IV decoded");
                    info.aes_iv = Some(iv_bytes);
                }
                Err(e) => {
                    tracing::warn!("Failed to decode AES IV: {}", e);
                }
            }
        } else if line.contains("aesiv") {
            info!(line = %line, "SDP: Found aesiv line");
        }

        // Parse min-latency
        if let Some(val_str) = line.strip_prefix("a=min-latency:")
            && let Ok(val) = val_str.trim().parse::<u32>() {
                info.min_latency = Some(val);
                info!(min_latency = val, "SDP: min latency");
            }

        // Parse max-latency
        if let Some(val_str) = line.strip_prefix("a=max-latency:")
            && let Ok(val) = val_str.trim().parse::<u32>() {
                info.max_latency = Some(val);
                info!(max_latency = val, "SDP: max latency");
            }
    }

    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sdp_alac() {
        let sdp = "\
v=0\r\n\
o=iTunes 1234567890 0 IN IP4 192.168.1.100\r\n\
s=iTunes\r\n\
c=IN IP4 192.168.1.200\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 96\r\n\
a=rtpmap:96 AppleLossless\r\n\
a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
a=min-latency:11025\r\n";

        let info = parse_sdp(sdp).unwrap();
        assert_eq!(info.codec, AudioCodec::Alac);
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 2);
        assert!(info.aes_key.is_none());
        assert_eq!(info.min_latency, Some(11025));
    }

    #[test]
    fn test_parse_sdp_aac_with_keys() {
        let sdp = "\
v=0\r\n\
m=audio 0 RTP/AVP 96\r\n\
a=rtpmap:96 mpeg4-generic/44100/2\r\n\
a=fpaeskey:dGVzdGtleQ==\r\n\
a=aesiv:dGVzdGl2\r\n";

        let info = parse_sdp(sdp).unwrap();
        assert_eq!(info.codec, AudioCodec::AacLc);
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.aes_key.unwrap(), b"testkey");
        assert_eq!(info.aes_iv.unwrap(), b"testiv");
    }

    #[test]
    fn test_parse_sdp_default() {
        let sdp = "v=0\r\nm=audio 0 RTP/AVP 96\r\n";
        let info = parse_sdp(sdp).unwrap();
        // Falls back to defaults
        assert_eq!(info.codec, AudioCodec::Alac);
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 2);
    }

    #[test]
    fn test_derive_video_keys_basic() {
        let audio_key = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                         0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10];
        let stream_id = 12345678901234567890u64;
        
        let keys = derive_video_keys(&audio_key, stream_id);
        
        // Verify keys are 32 bytes
        assert_eq!(keys.key.len(), 32);
        assert_eq!(keys.iv.len(), 32);
        
        // Verify keys are not all zeros
        assert_ne!(keys.key, [0u8; 32]);
        assert_ne!(keys.iv, [0u8; 32]);
        
        // Verify key and IV are different
        assert_ne!(keys.key, keys.iv);
    }

    #[test]
    fn test_derive_video_keys_idempotence() {
        let audio_key = [0xaa; 16];
        let stream_id = 9876543210u64;
        
        let keys1 = derive_video_keys(&audio_key, stream_id);
        let keys2 = derive_video_keys(&audio_key, stream_id);
        
        // Same inputs should produce identical outputs
        assert_eq!(keys1.key, keys2.key);
        assert_eq!(keys1.iv, keys2.iv);
    }

    #[test]
    fn test_derive_video_keys_different_stream_ids() {
        let audio_key = [0x55; 16];
        let stream_id1 = 1000u64;
        let stream_id2 = 2000u64;
        
        let keys1 = derive_video_keys(&audio_key, stream_id1);
        let keys2 = derive_video_keys(&audio_key, stream_id2);
        
        // Different stream IDs should produce different keys
        assert_ne!(keys1.key, keys2.key);
        assert_ne!(keys1.iv, keys2.iv);
    }

    #[test]
    fn test_derive_video_keys_different_audio_keys() {
        let audio_key1 = [0x11; 16];
        let audio_key2 = [0x22; 16];
        let stream_id = 5555u64;
        
        let keys1 = derive_video_keys(&audio_key1, stream_id);
        let keys2 = derive_video_keys(&audio_key2, stream_id);
        
        // Different audio keys should produce different video keys
        assert_ne!(keys1.key, keys2.key);
        assert_ne!(keys1.iv, keys2.iv);
    }

    #[test]
    fn test_video_keys_structure() {
        let audio_key = [0xff; 16];
        let stream_id = 123u64;
        
        let keys = derive_video_keys(&audio_key, stream_id);
        
        // Verify the VideoKeys structure can be cloned and compared
        let keys_clone = keys.clone();
        assert_eq!(keys, keys_clone);
    }

    #[test]
    fn test_derive_video_keys_sha512_correctness() {
        // Test that the derivation matches manual SHA-512 computation
        let audio_key = [0xaa; 16];
        let stream_id = 1000u64;
        
        let keys = derive_video_keys(&audio_key, stream_id);
        
        // Manually compute expected key
        let key_string = "AirPlayStreamKey1000";
        let mut hasher = Sha512::new();
        hasher.update(key_string.as_bytes());
        hasher.update(&audio_key);
        let key_hash = hasher.finalize();
        let mut expected_key = [0u8; 32];
        expected_key.copy_from_slice(&key_hash[..32]);
        
        // Manually compute expected IV
        let iv_string = "AirPlayStreamIV1000";
        let mut hasher = Sha512::new();
        hasher.update(iv_string.as_bytes());
        hasher.update(&audio_key);
        let iv_hash = hasher.finalize();
        let mut expected_iv = [0u8; 32];
        expected_iv.copy_from_slice(&iv_hash[..32]);
        
        assert_eq!(keys.key, expected_key);
        assert_eq!(keys.iv, expected_iv);
    }

    #[test]
    fn test_derive_video_keys_large_stream_id() {
        // Test with maximum u64 value
        let audio_key = [0x42; 16];
        let stream_id = u64::MAX;
        
        let keys = derive_video_keys(&audio_key, stream_id);
        
        // Verify keys are derived without panic
        assert_eq!(keys.key.len(), 32);
        assert_eq!(keys.iv.len(), 32);
        assert_ne!(keys.key, [0u8; 32]);
        assert_ne!(keys.iv, [0u8; 32]);
    }
}
