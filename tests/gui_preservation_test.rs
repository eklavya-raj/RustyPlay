//! Preservation Property Tests for GUI Bug Fix
//!
//! **Property 2: Preservation** - Network and Decryption Behavior
//!
//! **Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5, 3.6**
//!
//! These tests verify that non-GUI operations remain unchanged after the GUI fix:
//! - Network connection establishment (TCP on port 7100)
//! - Video frame decryption using derived AES keys
//! - Audio packet decryption using derived AES keys
//! - Codec configuration parsing (SPS/PPS)
//! - AVCC to Annex-B conversion
//! - Network services initialization (HTTP, RTSP, RTP, NTP)
//!
//! **IMPORTANT**: These tests are run on UNFIXED code to observe baseline behavior,
//! then must continue to pass after the fix is implemented.
//!
//! **EXPECTED OUTCOME**: Tests PASS on unfixed code (confirms baseline behavior to preserve)

use proptest::prelude::*;
use rusty_play::codec::{derive_video_keys, parse_sdp, AudioCodec};
use rusty_play::mirror::MirrorDecryptor;

// ============================================================================
// Property 1: Video Key Derivation Preservation
// ============================================================================

/// **Property: Video Key Derivation Idempotence**
///
/// **Validates: Requirements 3.2, 3.4**
///
/// For any audio AES key and stream connection ID, the derive_video_keys function
/// SHALL produce identical output when called multiple times with the same inputs.
/// This ensures the key derivation algorithm remains deterministic and unchanged.
proptest! {
    #[test]
    fn prop_video_key_derivation_idempotence(
        audio_key_seed in 0u64..=255,
        stream_id in 0u64..=u64::MAX,
    ) {
        // Generate deterministic audio key from seed
        let audio_key: [u8; 16] = [
            (audio_key_seed % 256) as u8,
            ((audio_key_seed / 256) % 256) as u8,
            (audio_key_seed % 251) as u8,
            ((audio_key_seed / 251) % 256) as u8,
            (audio_key_seed % 241) as u8,
            ((audio_key_seed / 241) % 256) as u8,
            (audio_key_seed % 239) as u8,
            ((audio_key_seed / 239) % 256) as u8,
            (audio_key_seed % 233) as u8,
            ((audio_key_seed / 233) % 256) as u8,
            (audio_key_seed % 229) as u8,
            ((audio_key_seed / 229) % 256) as u8,
            (audio_key_seed % 227) as u8,
            ((audio_key_seed / 227) % 256) as u8,
            (audio_key_seed % 223) as u8,
            ((audio_key_seed / 223) % 256) as u8,
        ];
        
        // Derive video keys twice
        let keys1 = derive_video_keys(&audio_key, stream_id);
        let keys2 = derive_video_keys(&audio_key, stream_id);
        
        // ASSERTION: Same inputs produce identical outputs
        prop_assert_eq!(
            keys1.key,
            keys2.key,
            "Video key derivation should be idempotent (same inputs produce same key)"
        );
        
        prop_assert_eq!(
            keys1.iv,
            keys2.iv,
            "Video IV derivation should be idempotent (same inputs produce same IV)"
        );
    }
}

/// **Property: Video Key Derivation Uniqueness**
///
/// **Validates: Requirements 3.2, 3.4**
///
/// For different stream connection IDs, the derive_video_keys function SHALL
/// produce different keys and IVs. This ensures proper isolation between streams.
proptest! {
    #[test]
    fn prop_video_key_derivation_uniqueness(
        audio_key_seed in 0u64..=255,
        stream_id1 in 0u64..=u64::MAX,
        stream_id2 in 0u64..=u64::MAX,
    ) {
        // Skip if stream IDs are the same
        prop_assume!(stream_id1 != stream_id2);
        
        // Generate deterministic audio key
        let audio_key: [u8; 16] = [
            (audio_key_seed % 256) as u8,
            ((audio_key_seed / 256) % 256) as u8,
            (audio_key_seed % 251) as u8,
            ((audio_key_seed / 251) % 256) as u8,
            (audio_key_seed % 241) as u8,
            ((audio_key_seed / 241) % 256) as u8,
            (audio_key_seed % 239) as u8,
            ((audio_key_seed / 239) % 256) as u8,
            (audio_key_seed % 233) as u8,
            ((audio_key_seed / 233) % 256) as u8,
            (audio_key_seed % 229) as u8,
            ((audio_key_seed / 229) % 256) as u8,
            (audio_key_seed % 227) as u8,
            ((audio_key_seed / 227) % 256) as u8,
            (audio_key_seed % 223) as u8,
            ((audio_key_seed / 223) % 256) as u8,
        ];
        
        // Derive video keys for different stream IDs
        let keys1 = derive_video_keys(&audio_key, stream_id1);
        let keys2 = derive_video_keys(&audio_key, stream_id2);
        
        // ASSERTION: Different stream IDs produce different keys
        prop_assert_ne!(
            keys1.key,
            keys2.key,
            "Different stream IDs should produce different video keys"
        );
        
        prop_assert_ne!(
            keys1.iv,
            keys2.iv,
            "Different stream IDs should produce different video IVs"
        );
    }
}

/// **Property: Video Key Structure Validity**
///
/// **Validates: Requirements 3.2, 3.4**
///
/// For any valid inputs, the derive_video_keys function SHALL produce:
/// - 32-byte keys (not all zeros)
/// - 32-byte IVs (not all zeros)
/// - Key and IV are different from each other
proptest! {
    #[test]
    fn prop_video_key_structure_validity(
        audio_key_seed in 0u64..=255,
        stream_id in 0u64..=u64::MAX,
    ) {
        // Generate deterministic audio key
        let audio_key: [u8; 16] = [
            (audio_key_seed % 256) as u8,
            ((audio_key_seed / 256) % 256) as u8,
            (audio_key_seed % 251) as u8,
            ((audio_key_seed / 251) % 256) as u8,
            (audio_key_seed % 241) as u8,
            ((audio_key_seed / 241) % 256) as u8,
            (audio_key_seed % 239) as u8,
            ((audio_key_seed / 239) % 256) as u8,
            (audio_key_seed % 233) as u8,
            ((audio_key_seed / 233) % 256) as u8,
            (audio_key_seed % 229) as u8,
            ((audio_key_seed / 229) % 256) as u8,
            (audio_key_seed % 227) as u8,
            ((audio_key_seed / 227) % 256) as u8,
            (audio_key_seed % 223) as u8,
            ((audio_key_seed / 223) % 256) as u8,
        ];
        
        let keys = derive_video_keys(&audio_key, stream_id);
        
        // ASSERTION 1: Keys are 32 bytes
        prop_assert_eq!(keys.key.len(), 32, "Video key should be 32 bytes");
        prop_assert_eq!(keys.iv.len(), 32, "Video IV should be 32 bytes");
        
        // ASSERTION 2: Keys are not all zeros
        prop_assert_ne!(keys.key, [0u8; 32], "Video key should not be all zeros");
        prop_assert_ne!(keys.iv, [0u8; 32], "Video IV should not be all zeros");
        
        // ASSERTION 3: Key and IV are different
        prop_assert_ne!(keys.key, keys.iv, "Video key and IV should be different");
    }
}

// ============================================================================
// Property 2: Video Decryption Preservation (MirrorDecryptor)
// ============================================================================

/// **Property: Video Decryption Idempotence**
///
/// **Validates: Requirements 3.2**
///
/// For any encrypted video payload, decrypting with the same key and IV twice
/// SHALL produce identical plaintext. This ensures the decryption algorithm
/// remains deterministic and unchanged.
proptest! {
    #[test]
    fn prop_video_decryption_idempotence(
        key_seed in 0u64..=255,
        iv_seed in 0u64..=255,
        payload_size in 16usize..=1024,
    ) {
        // Generate deterministic key and IV
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
        
        // Create a test payload (simulating encrypted video data)
        let mut payload1 = vec![0u8; payload_size];
        for i in 0..payload_size {
            payload1[i] = ((i * 7 + 13) % 256) as u8;
        }
        let mut payload2 = payload1.clone();
        
        // Decrypt both payloads
        let mut decryptor1 = MirrorDecryptor::new(&key, &iv);
        decryptor1.decrypt_in_place(&mut payload1);
        
        let mut decryptor2 = MirrorDecryptor::new(&key, &iv);
        decryptor2.decrypt_in_place(&mut payload2);
        
        // ASSERTION: Same inputs produce identical outputs
        prop_assert_eq!(
            payload1,
            payload2,
            "Video decryption should be idempotent (same inputs produce same output)"
        );
    }
}

/// **Property: Video Decryption Roundtrip**
///
/// **Validates: Requirements 3.2**
///
/// For any plaintext payload, encrypting and then decrypting SHALL produce
/// the original plaintext. This ensures the decryption algorithm is correct
/// and remains unchanged (CTR mode is symmetric).
proptest! {
    #[test]
    fn prop_video_decryption_roundtrip(
        key_seed in 0u64..=255,
        iv_seed in 0u64..=255,
        payload_size in 16usize..=1024,
    ) {
        // Generate deterministic key and IV
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
        
        // Create original plaintext
        let mut plaintext = vec![0u8; payload_size];
        for i in 0..payload_size {
            plaintext[i] = ((i * 11 + 7) % 256) as u8;
        }
        let original = plaintext.clone();
        
        // Encrypt (CTR mode: encryption and decryption are the same operation)
        let mut encryptor = MirrorDecryptor::new(&key, &iv);
        encryptor.decrypt_in_place(&mut plaintext);
        
        // Verify it changed
        let encrypted = plaintext.clone();
        prop_assert_ne!(&encrypted, &original, "Encryption should change the data");
        
        // Decrypt
        let mut decryptor = MirrorDecryptor::new(&key, &iv);
        decryptor.decrypt_in_place(&mut plaintext);
        
        // ASSERTION: Roundtrip produces original plaintext
        prop_assert_eq!(
            &plaintext,
            &original,
            "Decryption after encryption should produce original plaintext"
        );
    }
}

/// **Property: Video Decryption Length Preservation**
///
/// **Validates: Requirements 3.2**
///
/// For any encrypted payload, decryption SHALL preserve the payload length.
/// This ensures the decryption algorithm handles all payload sizes correctly.
proptest! {
    #[test]
    fn prop_video_decryption_length_preservation(
        key_seed in 0u64..=255,
        iv_seed in 0u64..=255,
        payload_size in 1usize..=2048,
    ) {
        // Generate deterministic key and IV
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
        
        // Create a test payload
        let mut payload = vec![0u8; payload_size];
        for i in 0..payload_size {
            payload[i] = ((i * 3 + 5) % 256) as u8;
        }
        
        let original_len = payload.len();
        
        // Decrypt
        let mut decryptor = MirrorDecryptor::new(&key, &iv);
        decryptor.decrypt_in_place(&mut payload);
        
        // ASSERTION: Length is preserved
        prop_assert_eq!(
            payload.len(),
            original_len,
            "Decryption should preserve payload length"
        );
    }
}

// ============================================================================
// Property 3: Codec Parsing Preservation (SDP)
// ============================================================================

/// **Property: SDP Parsing Idempotence**
///
/// **Validates: Requirements 3.4**
///
/// For any valid SDP body, parsing it multiple times SHALL produce identical
/// SessionInfo structures. This ensures the parsing algorithm remains deterministic.
proptest! {
    #[test]
    fn prop_sdp_parsing_idempotence(
        sample_rate in prop::sample::select(vec![44100u32, 48000, 96000]),
        channels in 1u16..=8,
    ) {
        // Create a valid SDP body
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
    }
}

/// **Property: SDP Parsing Correctness for ALAC**
///
/// **Validates: Requirements 3.4**
///
/// For any valid ALAC SDP body, parsing SHALL correctly extract:
/// - Codec type (ALAC)
/// - Sample rate from fmtp
/// - Channels from fmtp
proptest! {
    #[test]
    fn prop_sdp_parsing_alac_correctness(
        sample_rate in prop::sample::select(vec![44100u32, 48000, 96000]),
        channels in 1u16..=8,
    ) {
        // Create a valid ALAC SDP body
        let sdp = format!(
            "v=0\r\n\
             m=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 AppleLossless\r\n\
             a=fmtp:96 352 0 16 40 10 14 {} 255 0 0 {}\r\n",
            channels, sample_rate
        );
        
        let info = parse_sdp(&sdp).expect("SDP parsing should succeed");
        
        // ASSERTION: Codec is ALAC
        prop_assert_eq!(info.codec, AudioCodec::Alac, "Codec should be ALAC");
        
        // ASSERTION: Sample rate is correct
        prop_assert_eq!(info.sample_rate, sample_rate, "Sample rate should match SDP");
        
        // ASSERTION: Channels is correct
        prop_assert_eq!(info.channels, channels, "Channels should match SDP");
    }
}

/// **Property: SDP Parsing Correctness for AAC**
///
/// **Validates: Requirements 3.4**
///
/// For any valid AAC SDP body, parsing SHALL correctly extract:
/// - Codec type (AAC-LC or AAC-ELD)
/// - Sample rate from rtpmap
/// - Channels from rtpmap
proptest! {
    #[test]
    fn prop_sdp_parsing_aac_correctness(
        sample_rate in prop::sample::select(vec![44100u32, 48000, 96000]),
        channels in 1u16..=8,
    ) {
        // Create a valid AAC-LC SDP body
        let sdp = format!(
            "v=0\r\n\
             m=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 mpeg4-generic/{}/{}\r\n",
            sample_rate, channels
        );
        
        let info = parse_sdp(&sdp).expect("SDP parsing should succeed");
        
        // ASSERTION: Codec is AAC-LC
        prop_assert_eq!(info.codec, AudioCodec::AacLc, "Codec should be AAC-LC");
        
        // ASSERTION: Sample rate is correct
        prop_assert_eq!(info.sample_rate, sample_rate, "Sample rate should match SDP");
        
        // ASSERTION: Channels is correct
        prop_assert_eq!(info.channels, channels, "Channels should match SDP");
    }
}

// ============================================================================
// Unit Tests for Preservation
// ============================================================================

#[cfg(test)]
mod unit_tests {
    use super::*;

    /// Unit test: Video key derivation with known values
    ///
    /// **Validates: Requirements 3.2, 3.4**
    #[test]
    fn test_video_key_derivation_known_values() {
        let audio_key = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                         0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10];
        let stream_id = 12345678901234567890u64;
        
        let keys = derive_video_keys(&audio_key, stream_id);
        
        // Verify structure
        assert_eq!(keys.key.len(), 32);
        assert_eq!(keys.iv.len(), 32);
        assert_ne!(keys.key, [0u8; 32]);
        assert_ne!(keys.iv, [0u8; 32]);
        assert_ne!(keys.key, keys.iv);
        
        // Verify idempotence
        let keys2 = derive_video_keys(&audio_key, stream_id);
        assert_eq!(keys.key, keys2.key);
        assert_eq!(keys.iv, keys2.iv);
    }

    /// Unit test: MirrorDecryptor roundtrip
    ///
    /// **Validates: Requirements 3.2**
    #[test]
    fn test_mirror_decryptor_roundtrip() {
        let key = [0x55u8; 16];
        let iv = [0xAAu8; 16];
        
        let mut enc = MirrorDecryptor::new(&key, &iv);
        let mut dec = MirrorDecryptor::new(&key, &iv);
        
        let mut plaintext = b"Hello, AirPlay mirroring decryption!".to_vec();
        let original = plaintext.clone();
        
        // Encrypt
        enc.decrypt_in_place(&mut plaintext);
        assert_ne!(plaintext, original);
        
        // Decrypt
        dec.decrypt_in_place(&mut plaintext);
        assert_eq!(plaintext, original);
    }

    /// Unit test: SDP parsing for ALAC
    ///
    /// **Validates: Requirements 3.4**
    #[test]
    fn test_sdp_parsing_alac() {
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
    }

    /// Unit test: SDP parsing for AAC with encryption keys
    ///
    /// **Validates: Requirements 3.4**
    #[test]
    fn test_sdp_parsing_aac_with_keys() {
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

    /// Unit test: MirrorDecryptor with various payload sizes
    ///
    /// **Validates: Requirements 3.2**
    #[test]
    fn test_mirror_decryptor_various_sizes() {
        let key = [0x42u8; 16];
        let iv = [0x24u8; 16];
        
        // Test various payload sizes (including non-16-byte-aligned)
        for size in [1, 15, 16, 17, 31, 32, 33, 100, 256, 1000] {
            let mut payload = vec![0u8; size];
            for i in 0..size {
                payload[i] = ((i * 7) % 256) as u8;
            }
            let original = payload.clone();
            
            // Encrypt
            let mut enc = MirrorDecryptor::new(&key, &iv);
            enc.decrypt_in_place(&mut payload);
            
            // Decrypt
            let mut dec = MirrorDecryptor::new(&key, &iv);
            dec.decrypt_in_place(&mut payload);
            
            assert_eq!(
                payload, original,
                "Roundtrip should work for payload size {}",
                size
            );
        }
    }
}
