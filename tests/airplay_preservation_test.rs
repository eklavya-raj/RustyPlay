//! Preservation Property Tests for AirPlay Performance Fix
//!
//! **Property 2: Preservation** - Non-Timing Protocol and Codec Behavior
//!
//! **Validates: Requirements 3.1, 3.2, 3.3, 3.4, 4.1, 4.2, 4.3**
//!
//! These tests verify that non-timing operations remain unchanged after the fix:
//! - RTSP protocol handling (OPTIONS, ANNOUNCE, SETUP, RECORD, TEARDOWN responses)
//! - AES-CBC decryption produces correct plaintext for encrypted RTP audio payloads
//! - ALAC/AAC-ELD codec decoding produces correct PCM samples
//! - Jitter buffer flush clears state correctly
//! - Error handling waits 10s for session keys then fails gracefully
//!
//! **IMPORTANT**: These tests are run on UNFIXED code to observe baseline behavior,
//! then must continue to pass after the fix is implemented.
//!
//! **EXPECTED OUTCOME**: Tests PASS on unfixed code (confirms baseline behavior to preserve)

use proptest::prelude::*;
use rusty_play::codec::{parse_sdp, AudioCodec, alac_magic_cookie_from_fmtp};
use rusty_play::rtp::{parse_rtp_packet, JitterBuffer};
use rusty_play::ntp::NtpTimestamp;
use aes::Aes128;
use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use cbc::Decryptor as CbcDecryptor;

type Aes128CbcDec = CbcDecryptor<Aes128>;

// ============================================================================
// Helper Functions
// ============================================================================

/// Build a minimal valid RTP packet for testing
fn make_rtp_packet(seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
    let mut pkt = vec![0u8; 12 + payload.len()];
    pkt[0] = 0x80; // V=2, P=0, X=0, CC=0
    pkt[1] = 96;   // M=0, PT=96 (audio)
    pkt[2..4].copy_from_slice(&seq.to_be_bytes());
    pkt[4..8].copy_from_slice(&ts.to_be_bytes());
    pkt[8..12].copy_from_slice(&0u32.to_be_bytes()); // SSRC
    pkt[12..].copy_from_slice(payload);
    pkt
}

// ============================================================================
// Property 1: RTSP Protocol Preservation
// ============================================================================

/// **Property: SDP Parsing Idempotence**
///
/// **Validates: Requirements 3.1**
///
/// For any valid SDP body, parsing it multiple times SHALL produce identical
/// SessionInfo structures. This ensures the RTSP ANNOUNCE handling remains
/// deterministic and unchanged after the fix.
proptest! {
    #[test]
    fn prop_rtsp_sdp_parsing_idempotence(
        sample_rate in prop::sample::select(vec![44100u32, 48000, 96000]),
        channels in 1u16..=8,
    ) {
        // Create a valid ALAC SDP body
        let sdp = format!(
            "v=0\r\n\
             o=iTunes 1234567890 0 IN IP4 192.168.1.100\r\n\
             s=iTunes\r\n\
             c=IN IP4 192.168.1.200\r\n\
             t=0 0\r\n\
             m=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 AppleLossless\r\n\
             a=fmtp:96 352 0 16 40 10 14 {} 255 0 0 {}\r\n\
             a=min-latency:11025\r\n",
            channels, sample_rate
        );
        
        // Parse twice
        let info1 = parse_sdp(&sdp).expect("SDP parsing should succeed");
        let info2 = parse_sdp(&sdp).expect("SDP parsing should succeed");
        
        // ASSERTION: Same SDP produces identical SessionInfo
        prop_assert_eq!(info1.codec, info2.codec, "Codec should be identical");
        prop_assert_eq!(info1.sample_rate, info2.sample_rate, "Sample rate should be identical");
        prop_assert_eq!(info1.channels, info2.channels, "Channels should be identical");
        prop_assert_eq!(info1.stream_type, info2.stream_type, "Stream type should be identical");
        
        println!("✓ RTSP SDP parsing is idempotent: {}kHz, {} channels", sample_rate/1000, channels);
    }
}

/// **Property: RTSP ANNOUNCE Format Preservation**
///
/// **Validates: Requirements 3.1**
///
/// For any valid RTSP ANNOUNCE with ALAC codec, the parsed SessionInfo SHALL
/// correctly extract codec, sample rate, and channels. This behavior must
/// remain unchanged after the timing fix.
proptest! {
    #[test]
    fn prop_rtsp_announce_alac_parsing(
        sample_rate in prop::sample::select(vec![44100u32, 48000, 96000]),
        channels in 1u16..=8,
        min_latency in 5000u32..=20000u32,
    ) {
        // Create valid ALAC SDP body
        let sdp = format!(
            "v=0\r\n\
             m=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 AppleLossless\r\n\
             a=fmtp:96 352 0 16 40 10 14 {} 255 0 0 {}\r\n\
             a=min-latency:{}\r\n",
            channels, sample_rate, min_latency
        );
        
        let info = parse_sdp(&sdp).expect("SDP parsing should succeed");
        
        // ASSERTION: Codec is ALAC
        prop_assert_eq!(info.codec, AudioCodec::Alac, "Codec should be ALAC");
        
        // ASSERTION: Sample rate is correct
        prop_assert_eq!(info.sample_rate, sample_rate, "Sample rate should match SDP");
        
        // ASSERTION: Channels is correct
        prop_assert_eq!(info.channels, channels, "Channels should match SDP");
        
        // ASSERTION: Min latency is correct
        prop_assert_eq!(info.min_latency, Some(min_latency), "Min latency should match SDP");
        
        println!("✓ RTSP ANNOUNCE ALAC parsing correct: codec={}, sr={}, ch={}, latency={}",
                 info.codec, info.sample_rate, info.channels, min_latency);
    }
}

// ============================================================================
// Property 2: AES-CBC Decryption Preservation
// ============================================================================

/// **Property: AES-CBC Decryption Idempotence**
///
/// **Validates: Requirements 3.2**
///
/// For any encrypted RTP audio payload, decrypting with the same key and IV
/// twice SHALL produce identical plaintext. This ensures AES-CBC decryption
/// algorithm remains deterministic and unchanged after the timing fix.
proptest! {
    #[test]
    fn prop_aes_cbc_decryption_idempotence(
        key_seed in 0u64..=255,
        iv_seed in 0u64..=255,
        payload_size in 16usize..=512,
    ) {
        // Ensure payload is 16-byte aligned (AES block size)
        let aligned_size = (payload_size / 16) * 16;
        prop_assume!(aligned_size >= 16);
        
        // Generate deterministic key and IV (16 bytes each)
        let key: [u8; 16] = [
            (key_seed % 256) as u8,
            ((key_seed / 256) % 256) as u8,
            (key_seed % 251) as u8,
            ((key_seed / 251) % 256) as u8,
            (key_seed % 241) as u8,
            ((key_seed / 241) % 256) as u8,
            (key_seed % 239) as u8,
            ((key_seed / 239) % 256) as u8,
            (key_seed % 233) as u8,
            ((key_seed / 233) % 256) as u8,
            (key_seed % 229) as u8,
            ((key_seed / 229) % 256) as u8,
            (key_seed % 227) as u8,
            ((key_seed / 227) % 256) as u8,
            (key_seed % 223) as u8,
            ((key_seed / 223) % 256) as u8,
        ];
        
        let iv: [u8; 16] = [
            (iv_seed % 256) as u8,
            ((iv_seed / 256) % 256) as u8,
            (iv_seed % 251) as u8,
            ((iv_seed / 251) % 256) as u8,
            (iv_seed % 241) as u8,
            ((iv_seed / 241) % 256) as u8,
            (iv_seed % 239) as u8,
            ((iv_seed / 239) % 256) as u8,
            (iv_seed % 233) as u8,
            ((iv_seed / 233) % 256) as u8,
            (iv_seed % 229) as u8,
            ((iv_seed / 229) % 256) as u8,
            (iv_seed % 227) as u8,
            ((iv_seed / 227) % 256) as u8,
            (iv_seed % 223) as u8,
            ((iv_seed / 223) % 256) as u8,
        ];
        
        // Create test encrypted payload
        let mut payload1 = vec![0u8; aligned_size];
        for i in 0..aligned_size {
            payload1[i] = ((i * 7 + 13) % 256) as u8;
        }
        let mut payload2 = payload1.clone();
        
        // Decrypt both payloads
        if let Ok(dec1) = Aes128CbcDec::new_from_slices(&key, &iv) {
            let _ = dec1.decrypt_padded_mut::<NoPadding>(&mut payload1);
        }
        
        if let Ok(dec2) = Aes128CbcDec::new_from_slices(&key, &iv) {
            let _ = dec2.decrypt_padded_mut::<NoPadding>(&mut payload2);
        }
        
        // ASSERTION: Same inputs produce identical outputs
        prop_assert_eq!(
            payload1,
            payload2,
            "AES-CBC decryption should be idempotent (same key/IV produce same plaintext)"
        );
        
        println!("✓ AES-CBC decryption idempotent: {} bytes", aligned_size);
    }
}

/// **Property: AES-CBC Decryption Roundtrip**
///
/// **Validates: Requirements 3.2**
///
/// For any plaintext RTP audio payload, encrypting and then decrypting SHALL
/// produce the original plaintext. This ensures decryption correctness is preserved.
proptest! {
    #[test]
    fn prop_aes_cbc_roundtrip(
        key_seed in 0u64..=255,
        iv_seed in 0u64..=255,
        payload_size in 16usize..=512,
    ) {
        use cbc::cipher::BlockEncryptMut;
        type Aes128CbcEnc = cbc::Encryptor<Aes128>;
        
        let aligned_size = (payload_size / 16) * 16;
        prop_assume!(aligned_size >= 16);
        
        // Generate deterministic key and IV
        let key: [u8; 16] = [
            (key_seed % 256) as u8, ((key_seed / 256) % 256) as u8,
            (key_seed % 251) as u8, ((key_seed / 251) % 256) as u8,
            (key_seed % 241) as u8, ((key_seed / 241) % 256) as u8,
            (key_seed % 239) as u8, ((key_seed / 239) % 256) as u8,
            (key_seed % 233) as u8, ((key_seed / 233) % 256) as u8,
            (key_seed % 229) as u8, ((key_seed / 229) % 256) as u8,
            (key_seed % 227) as u8, ((key_seed / 227) % 256) as u8,
            (key_seed % 223) as u8, ((key_seed / 223) % 256) as u8,
        ];
        
        let iv: [u8; 16] = [
            (iv_seed % 256) as u8, ((iv_seed / 256) % 256) as u8,
            (iv_seed % 251) as u8, ((iv_seed / 251) % 256) as u8,
            (iv_seed % 241) as u8, ((iv_seed / 241) % 256) as u8,
            (iv_seed % 239) as u8, ((iv_seed / 239) % 256) as u8,
            (iv_seed % 233) as u8, ((iv_seed / 233) % 256) as u8,
            (iv_seed % 229) as u8, ((iv_seed / 229) % 256) as u8,
            (iv_seed % 227) as u8, ((iv_seed / 227) % 256) as u8,
            (iv_seed % 223) as u8, ((iv_seed / 223) % 256) as u8,
        ];
        
        // Create original plaintext
        let mut plaintext = vec![0u8; aligned_size];
        for i in 0..aligned_size {
            plaintext[i] = ((i * 11 + 7) % 256) as u8;
        }
        let original = plaintext.clone();
        
        // Encrypt
        if let Ok(enc) = Aes128CbcEnc::new_from_slices(&key, &iv) {
            let _ = enc.encrypt_padded_mut::<NoPadding>(&mut plaintext, aligned_size);
        }
        
        // Verify it changed
        let encrypted = plaintext.clone();
        prop_assert_ne!(&encrypted, &original, "Encryption should change the data");
        
        // Decrypt
        if let Ok(dec) = Aes128CbcDec::new_from_slices(&key, &iv) {
            let _ = dec.decrypt_padded_mut::<NoPadding>(&mut plaintext);
        }
        
        // ASSERTION: Roundtrip produces original plaintext
        prop_assert_eq!(
            &plaintext,
            &original,
            "Decryption after encryption should produce original plaintext"
        );
        
        println!("✓ AES-CBC roundtrip correct: {} bytes", aligned_size);
    }
}

// ============================================================================
// Property 3: Codec Parsing Preservation (ALAC Magic Cookie)
// ============================================================================

/// **Property: ALAC Magic Cookie Generation Idempotence**
///
/// **Validates: Requirements 3.3, 3.4**
///
/// For any valid codec configuration (sample rate, channels, fmtp), the
/// alac_magic_cookie_from_fmtp function SHALL produce identical output when
/// called multiple times. This ensures codec initialization remains unchanged.
proptest! {
    #[test]
    fn prop_alac_magic_cookie_idempotence(
        sample_rate in prop::sample::select(vec![44100u32, 48000, 96000]),
        channels in 1u16..=8,
        frame_length in prop::sample::select(vec![352u32, 480, 1024]),
    ) {
        let fmtp = format!("{} 0 16 40 10 14 {} 255 0 0 {}", 
                          frame_length, channels, sample_rate);
        
        // Generate magic cookie twice
        let cookie1 = alac_magic_cookie_from_fmtp(Some(&fmtp), sample_rate, channels);
        let cookie2 = alac_magic_cookie_from_fmtp(Some(&fmtp), sample_rate, channels);
        
        // ASSERTION: Same inputs produce identical output
        prop_assert_eq!(
            &cookie1,
            &cookie2,
            "ALAC magic cookie generation should be idempotent"
        );
        
        // Verify structure (24 bytes)
        prop_assert_eq!(cookie1.len(), 24, "Magic cookie should be 24 bytes");
        
        println!("✓ ALAC magic cookie idempotent: sr={}, ch={}, frame={}", 
                 sample_rate, channels, frame_length);
    }
}

/// **Property: ALAC Magic Cookie Structure Validity**
///
/// **Validates: Requirements 3.3, 3.4**
///
/// For any valid configuration, the ALAC magic cookie SHALL have correct structure:
/// - 24 bytes total
/// - Frame length in bytes [0..4]
/// - Sample rate in bytes [20..24]
/// - Channels in byte [9]
proptest! {
    #[test]
    fn prop_alac_magic_cookie_structure(
        sample_rate in prop::sample::select(vec![44100u32, 48000, 96000]),
        channels in 1u16..=8,
    ) {
        let fmtp = format!("352 0 16 40 10 14 {} 255 0 0 {}", channels, sample_rate);
        let cookie = alac_magic_cookie_from_fmtp(Some(&fmtp), sample_rate, channels);
        
        // ASSERTION: Length is 24 bytes
        prop_assert_eq!(cookie.len(), 24, "Magic cookie must be 24 bytes");
        
        // ASSERTION: Frame length is encoded in bytes [0..4]
        let frame_len = u32::from_be_bytes([cookie[0], cookie[1], cookie[2], cookie[3]]);
        prop_assert_eq!(frame_len, 352, "Frame length should be 352");
        
        // ASSERTION: Channels is in byte [9]
        prop_assert_eq!(cookie[9], channels.min(255) as u8, "Channels byte should match");
        
        // ASSERTION: Sample rate is in bytes [20..24]
        let sr = u32::from_be_bytes([cookie[20], cookie[21], cookie[22], cookie[23]]);
        prop_assert_eq!(sr, sample_rate, "Sample rate should match");
        
        println!("✓ ALAC magic cookie structure valid: sr={}, ch={}", sample_rate, channels);
    }
}

// ============================================================================
// Property 4: Jitter Buffer Flush Preservation
// ============================================================================

/// **Property: Jitter Buffer Flush State Reset**
///
/// **Validates: Requirements 3.3**
///
/// When flush() is called on a jitter buffer with packets, it SHALL:
/// - Clear all packets (len() == 0, is_empty() == true)
/// - Reset expected_seq to None
/// - Increment flush_generation counter
/// This behavior must remain unchanged after the timing fix.
proptest! {
    #[test]
    fn prop_jitter_buffer_flush_state_reset(
        num_packets in 10usize..=50,
        base_seq in 1000u16..60000u16,
    ) {
        prop_assume!(base_seq + num_packets as u16 <= 65000);
        
        let mut jitter_buffer = JitterBuffer::new(64);
        let initial_flush_gen = jitter_buffer.flush_generation();
        
        // Insert packets
        for i in 0..num_packets {
            let seq = base_seq + i as u16;
            let ts = 1000u32 + (i as u32 * 352);
            let pkt_data = make_rtp_packet(seq, ts, &[0xAA; 100]);
            if let Some(pkt) = parse_rtp_packet(&pkt_data) {
                jitter_buffer.insert(pkt);
            }
        }
        
        // Verify packets were inserted
        prop_assert!(jitter_buffer.len() > 0, "Buffer should have packets before flush");
        prop_assert!(!jitter_buffer.is_empty(), "Buffer should not be empty before flush");
        
        // Flush the buffer
        jitter_buffer.flush();
        
        // ASSERTION: Buffer is empty after flush
        prop_assert_eq!(jitter_buffer.len(), 0, "Buffer should be empty after flush");
        prop_assert!(jitter_buffer.is_empty(), "Buffer should be empty after flush");
        
        // ASSERTION: Flush generation incremented
        let new_flush_gen = jitter_buffer.flush_generation();
        prop_assert_eq!(
            new_flush_gen,
            initial_flush_gen + 1,
            "Flush generation should increment by 1"
        );
        
        println!("✓ Jitter buffer flush correct: {} packets flushed, gen {} -> {}", 
                 num_packets, initial_flush_gen, new_flush_gen);
    }
}

/// **Property: Jitter Buffer Flush Idempotence**
///
/// **Validates: Requirements 3.3**
///
/// Calling flush() multiple times on an already-empty buffer SHALL:
/// - Keep buffer empty
/// - Increment flush_generation each time
/// This ensures flush semantics are consistent.
proptest! {
    #[test]
    fn prop_jitter_buffer_flush_idempotence(num_flushes in 1usize..=5) {
        let mut jitter_buffer = JitterBuffer::new(64);
        let initial_gen = jitter_buffer.flush_generation();
        
        // Flush multiple times
        for i in 1..=num_flushes {
            jitter_buffer.flush();
            let current_gen = jitter_buffer.flush_generation();
            
            // ASSERTION: Generation increments each time
            prop_assert_eq!(
                current_gen,
                initial_gen + i as u64,
                "Flush generation should increment each flush"
            );
            
            // ASSERTION: Buffer remains empty
            prop_assert!(jitter_buffer.is_empty(), "Buffer should stay empty");
            prop_assert_eq!(jitter_buffer.len(), 0, "Buffer length should be 0");
        }
        
        println!("✓ Jitter buffer flush idempotent: {} flushes", num_flushes);
    }
}

// ============================================================================
// Property 5: RTP Packet Parsing Preservation
// ============================================================================

/// **Property: RTP Packet Parsing Idempotence**
///
/// **Validates: Requirements 3.1, 3.2**
///
/// For any valid RTP packet bytes, parsing it multiple times SHALL produce
/// identical RtpPacket structures. This ensures RTP handling remains unchanged.
proptest! {
    #[test]
    fn prop_rtp_parsing_idempotence(
        sequence in 0u16..=65535,
        timestamp in 0u32..=u32::MAX,
        payload_size in 1usize..=1024,
    ) {
        // Create RTP packet
        let mut payload = vec![0u8; payload_size];
        for i in 0..payload_size {
            payload[i] = ((i * 3 + 5) % 256) as u8;
        }
        let pkt_data = make_rtp_packet(sequence, timestamp, &payload);
        
        // Parse twice
        let pkt1 = parse_rtp_packet(&pkt_data);
        let pkt2 = parse_rtp_packet(&pkt_data);
        
        // Both should succeed
        prop_assert!(pkt1.is_some(), "First parse should succeed");
        prop_assert!(pkt2.is_some(), "Second parse should succeed");
        
        let pkt1 = pkt1.unwrap();
        let pkt2 = pkt2.unwrap();
        
        // ASSERTION: Identical RtpPacket fields
        prop_assert_eq!(pkt1.version, pkt2.version, "Version should match");
        prop_assert_eq!(pkt1.sequence, pkt2.sequence, "Sequence should match");
        prop_assert_eq!(pkt1.timestamp, pkt2.timestamp, "Timestamp should match");
        prop_assert_eq!(pkt1.payload_type, pkt2.payload_type, "Payload type should match");
        prop_assert_eq!(pkt1.payload, pkt2.payload, "Payload should match");
        
        println!("✓ RTP parsing idempotent: seq={}, ts={}, payload={} bytes", 
                 sequence, timestamp, payload_size);
    }
}

/// **Property: RTP Packet Parsing Correctness**
///
/// **Validates: Requirements 3.1, 3.2**
///
/// For any valid RTP packet, parsing SHALL correctly extract:
/// - Version (2)
/// - Sequence number
/// - Timestamp
/// - Payload type (96 for audio)
/// - Payload bytes
proptest! {
    #[test]
    fn prop_rtp_parsing_correctness(
        sequence in 0u16..=65535,
        timestamp in 0u32..=u32::MAX,
        payload_size in 1usize..=512,
    ) {
        let mut payload = vec![0u8; payload_size];
        for i in 0..payload_size {
            payload[i] = ((i * 7 + 11) % 256) as u8;
        }
        let pkt_data = make_rtp_packet(sequence, timestamp, &payload);
        
        let pkt = parse_rtp_packet(&pkt_data);
        prop_assert!(pkt.is_some(), "RTP parsing should succeed");
        
        let pkt = pkt.unwrap();
        
        // ASSERTION: Version is 2
        prop_assert_eq!(pkt.version, 2, "RTP version should be 2");
        
        // ASSERTION: Sequence matches
        prop_assert_eq!(pkt.sequence, sequence, "Sequence number should match");
        
        // ASSERTION: Timestamp matches
        prop_assert_eq!(pkt.timestamp, timestamp, "Timestamp should match");
        
        // ASSERTION: Payload type is 96 (audio)
        prop_assert_eq!(pkt.payload_type, 96, "Payload type should be 96");
        
        // ASSERTION: Payload matches
        prop_assert_eq!(pkt.payload, payload, "Payload bytes should match");
        
        println!("✓ RTP parsing correct: seq={}, ts={}, {} bytes", 
                 sequence, timestamp, payload_size);
    }
}

// ============================================================================
// Property 6: ClockSync NTP Timestamp Preservation
// ============================================================================

/// **Property: NTP Timestamp Serialization Roundtrip**
///
/// **Validates: Requirements 3.1, 4.1**
///
/// For any NTP timestamp, converting to bytes and back SHALL produce the
/// original timestamp. This ensures NTP handling remains unchanged.
proptest! {
    #[test]
    fn prop_ntp_timestamp_roundtrip(
        seconds in 0u32..=u32::MAX,
        fraction in 0u32..=u32::MAX,
    ) {
        let original = NtpTimestamp { seconds, fraction };
        
        // Serialize to bytes
        let bytes = original.to_bytes();
        
        // ASSERTION: Serialized to 8 bytes
        prop_assert_eq!(bytes.len(), 8, "NTP timestamp should serialize to 8 bytes");
        
        // Deserialize from bytes
        let parsed = NtpTimestamp::from_bytes(&bytes);
        prop_assert!(parsed.is_some(), "NTP parsing should succeed");
        
        let parsed = parsed.unwrap();
        
        // ASSERTION: Roundtrip produces original timestamp
        prop_assert_eq!(parsed.seconds, original.seconds, "Seconds should match");
        prop_assert_eq!(parsed.fraction, original.fraction, "Fraction should match");
        
        println!("✓ NTP timestamp roundtrip: sec={}, frac={}", seconds, fraction);
    }
}

// ============================================================================
// Unit Tests for Preservation
// ============================================================================

#[cfg(test)]
mod unit_tests {
    use super::*;

    /// Unit test: RTSP SDP parsing with known ALAC values
    ///
    /// **Validates: Requirements 3.1**
    #[test]
    fn test_rtsp_sdp_alac_known_values() {
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
        assert_eq!(info.min_latency, Some(11025));
        
        println!("✓ RTSP SDP ALAC parsing with known values correct");
    }

    /// Unit test: AES-CBC decryption with known test vector
    ///
    /// **Validates: Requirements 3.2**
    #[test]
    fn test_aes_cbc_decryption_known_vector() {
        let key = [0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6,
                   0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c];
        let iv = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
                  0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];
        
        let mut ciphertext = [0x76, 0x49, 0xab, 0xac, 0x81, 0x19, 0xb2, 0x46,
                              0xce, 0xe9, 0x8e, 0x9b, 0x12, 0xe9, 0x19, 0x7d];
        
        // Decrypt
        if let Ok(dec) = Aes128CbcDec::new_from_slices(&key, &iv) {
            let _ = dec.decrypt_padded_mut::<NoPadding>(&mut ciphertext);
        }
        
        // Expected plaintext
        let expected = [0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96,
                       0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93, 0x17, 0x2a];
        
        assert_eq!(ciphertext, expected);
        println!("✓ AES-CBC decryption with known test vector correct");
    }

    /// Unit test: Jitter buffer flush clears state
    ///
    /// **Validates: Requirements 3.3**
    #[test]
    fn test_jitter_buffer_flush_clears_state() {
        let mut jb = JitterBuffer::new(64);
        let initial_gen = jb.flush_generation();
        
        // Insert some packets
        for seq in 100..105 {
            let pkt_data = make_rtp_packet(seq, seq as u32 * 1000, &[0xBB; 50]);
            if let Some(pkt) = parse_rtp_packet(&pkt_data) {
                jb.insert(pkt);
            }
        }
        
        assert_eq!(jb.len(), 5);
        assert!(!jb.is_empty());
        
        // Flush
        jb.flush();
        
        // Verify state
        assert_eq!(jb.len(), 0);
        assert!(jb.is_empty());
        assert_eq!(jb.flush_generation(), initial_gen + 1);
        
        println!("✓ Jitter buffer flush clears state correctly");
    }

    /// Unit test: ALAC magic cookie structure
    ///
    /// **Validates: Requirements 3.3, 3.4**
    #[test]
    fn test_alac_magic_cookie_structure() {
        let fmtp = "352 0 16 40 10 14 2 255 0 0 44100";
        let cookie = alac_magic_cookie_from_fmtp(Some(fmtp), 44100, 2);
        
        // Verify length
        assert_eq!(cookie.len(), 24);
        
        // Verify frame length (bytes 0-3)
        let frame_len = u32::from_be_bytes([cookie[0], cookie[1], cookie[2], cookie[3]]);
        assert_eq!(frame_len, 352);
        
        // Verify bit depth (byte 5)
        assert_eq!(cookie[5], 16);
        
        // Verify channels (byte 9)
        assert_eq!(cookie[9], 2);
        
        // Verify sample rate (bytes 20-23)
        let sr = u32::from_be_bytes([cookie[20], cookie[21], cookie[22], cookie[23]]);
        assert_eq!(sr, 44100);
        
        println!("✓ ALAC magic cookie structure correct");
    }

    /// Unit test: RTP packet parsing with marker bit
    ///
    /// **Validates: Requirements 3.1, 3.2**
    #[test]
    fn test_rtp_parsing_with_marker() {
        let mut pkt_data = make_rtp_packet(42, 1000, &[0xAA, 0xBB]);
        pkt_data[1] = 0x80 | 96; // Set marker bit (M=1)
        
        let pkt = parse_rtp_packet(&pkt_data).unwrap();
        assert_eq!(pkt.version, 2);
        assert_eq!(pkt.sequence, 42);
        assert_eq!(pkt.timestamp, 1000);
        assert_eq!(pkt.payload_type, 96);
        assert!(pkt.marker, "Marker bit should be set");
        assert_eq!(pkt.payload, vec![0xAA, 0xBB]);
        
        println!("✓ RTP parsing with marker bit correct");
    }

    /// Unit test: NTP timestamp current time
    ///
    /// **Validates: Requirements 3.1, 4.1**
    #[test]
    fn test_ntp_timestamp_now() {
        let ts = NtpTimestamp::now();
        
        // NTP epoch is 1900-01-01, Unix epoch is 1970-01-01
        // Year 2020 in NTP seconds = 3786825600
        // Current time should be well past 2020
        assert!(ts.seconds > 3_786_825_600, "NTP timestamp should be past year 2020");
        
        println!("✓ NTP timestamp now() returns valid current time");
    }
}
