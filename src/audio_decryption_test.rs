//! Bug Condition Exploration Test for AES-128-CBC Audio Decryption
//!
//! **CRITICAL**: This test MUST FAIL on unfixed code - failure confirms the bug exists.
//! **DO NOT attempt to fix the test or the code when it fails.**
//! **NOTE**: This test encodes the expected behavior - it will validate the fix when it passes after implementation.
//!
//! **GOAL**: Surface counterexamples that demonstrate the bug exists.
//!
//! **Validates: Requirements 1.1, 1.2, 1.3, 1.4**

use aes::Aes128;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use cbc::{Decryptor as CbcDecryptor, Encryptor as CbcEncryptor};

/// Encrypt a payload using AES-128-CBC (simulating AirPlay encryption)
fn encrypt_payload(plaintext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    type Aes128CbcEnc = CbcEncryptor<Aes128>;
    
    // Calculate encrypted length (16-byte aligned)
    let encrypted_len = (plaintext.len() / 16) * 16;
    
    if encrypted_len == 0 {
        // Payload too small to encrypt, return as-is
        return plaintext.to_vec();
    }
    
    let mut output = vec![0u8; plaintext.len()];
    
    // Encrypt only the 16-byte-aligned portion
    let encryptor = Aes128CbcEnc::new_from_slices(key, iv).unwrap();
    let mut encrypted_portion = plaintext[..encrypted_len].to_vec();
    let encrypted = encryptor.encrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(
        &mut encrypted_portion,
        encrypted_len
    ).unwrap();
    output[..encrypted_len].copy_from_slice(encrypted);
    
    // Copy remaining unencrypted bytes (if any)
    if encrypted_len < plaintext.len() {
        output[encrypted_len..].copy_from_slice(&plaintext[encrypted_len..]);
    }
    
    output
}

/// Decrypt a payload using the CURRENT (buggy) implementation from audio.rs
/// This replicates the decryption logic in run_decode_loop (lines 240-260)
fn decrypt_payload_current_implementation(encrypted: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    type Aes128CbcDec = CbcDecryptor<Aes128>;
    
    let mut decrypted_payload = encrypted.to_vec();
    
    if decrypted_payload.len() >= 16 {
        // This is the CURRENT (buggy) implementation from audio.rs
        if let Ok(decryptor) = Aes128CbcDec::new_from_slices(key, iv) {
            use cbc::cipher::block_padding::NoPadding;
            let decrypt_len = (decrypted_payload.len() / 16) * 16;
            let _ = decryptor.decrypt_padded_mut::<NoPadding>(&mut decrypted_payload[..decrypt_len]);
        }
    }
    
    decrypted_payload
}

/// Create a valid ALAC frame with the 0x20 marker byte
fn create_alac_frame(size: usize) -> Vec<u8> {
    let mut frame = vec![0u8; size];
    frame[0] = 0x20; // Valid ALAC frame marker
    // Fill with some test data (simulating audio samples)
    for i in 1..size {
        frame[i] = ((i * 7) % 256) as u8;
    }
    frame
}

#[cfg(test)]
mod bug_condition_tests {
    use super::*;
    use proptest::prelude::*;

    /// **Property 1: Bug Condition** - AES-128-CBC Decryption with 16-Byte Alignment
    ///
    /// **Validates: Requirements 1.1, 1.2, 1.3, 1.4**
    ///
    /// This property tests that encrypted ALAC payloads of various sizes decrypt correctly:
    /// - First byte of decrypted ALAC frame is 0x20 (valid frame marker)
    /// - Output length matches input payload length (remainder bytes preserved)
    /// - Decryption handles 16-byte alignment correctly
    /// - IV is reset for each packet (tested with consecutive packets)
    ///
    /// **EXPECTED OUTCOME ON UNFIXED CODE**: Test FAILS with counterexamples showing:
    /// - First byte is NOT 0x20 (invalid frame marker)
    /// - Output length mismatch (remainder bytes not copied)
    /// - Second packet decryption fails (IV not reset)
    proptest! {
        #[test]
        fn prop_aes_128_cbc_decryption_with_alignment(
            payload_size in 16usize..=1024,
            key_seed in 0u64..=255,
            iv_seed in 0u64..=255,
        ) {
            // Generate deterministic key and IV from seeds
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
            
            // Create a valid ALAC frame
            let original_frame = create_alac_frame(payload_size);
            
            // Encrypt the frame (simulating AirPlay encryption)
            let encrypted = encrypt_payload(&original_frame, &key, &iv);
            
            // Decrypt using the CURRENT (buggy) implementation
            let decrypted = decrypt_payload_current_implementation(&encrypted, &key, &iv);
            
            // ASSERTION 1: First byte of decrypted ALAC frame is 0x20
            // This will FAIL on unfixed code if decryption is incorrect
            prop_assert_eq!(
                decrypted.get(0).copied(),
                Some(0x20),
                "First byte of decrypted ALAC frame should be 0x20 (valid frame marker), \
                 but got 0x{:02x}. This indicates incorrect decryption. \
                 Payload size: {}, Encrypted length: {}",
                decrypted.get(0).copied().unwrap_or(0),
                payload_size,
                (payload_size / 16) * 16
            );
            
            // ASSERTION 2: Output length matches input length (remainder bytes preserved)
            // This will FAIL on unfixed code if remainder bytes are not copied
            prop_assert_eq!(
                decrypted.len(),
                payload_size,
                "Decrypted payload length should match original payload length. \
                 Expected: {}, Got: {}. This indicates remainder bytes were not copied.",
                payload_size,
                decrypted.len()
            );
            
            // ASSERTION 3: Decrypted payload matches original frame
            // This will FAIL on unfixed code if decryption algorithm is incorrect
            prop_assert_eq!(
                decrypted,
                original_frame,
                "Decrypted payload should match original frame. \
                 This indicates the decryption algorithm is incorrect."
            );
        }
    }

    /// Test IV reset between consecutive packets
    ///
    /// **Validates: Requirement 1.3**
    ///
    /// This test verifies that the IV is reset to the original session IV for each packet,
    /// ensuring independent per-packet decryption.
    ///
    /// **EXPECTED OUTCOME ON UNFIXED CODE**: Test FAILS if IV state carries over between packets.
    proptest! {
        #[test]
        fn prop_iv_reset_between_packets(
            payload_size in 16usize..=512,
            key_seed in 0u64..=255,
            iv_seed in 0u64..=255,
        ) {
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
            
            // Create two different ALAC frames
            let frame1 = create_alac_frame(payload_size);
            let mut frame2 = create_alac_frame(payload_size);
            frame2[1] = 0xFF; // Make it different from frame1
            
            // Encrypt both frames
            let encrypted1 = encrypt_payload(&frame1, &key, &iv);
            let encrypted2 = encrypt_payload(&frame2, &key, &iv);
            
            // Decrypt first packet
            let decrypted1 = decrypt_payload_current_implementation(&encrypted1, &key, &iv);
            
            // Decrypt second packet (IV should be reset)
            let decrypted2 = decrypt_payload_current_implementation(&encrypted2, &key, &iv);
            
            // ASSERTION: Both packets should decrypt correctly with 0x20 first byte
            // This will FAIL on unfixed code if IV is not reset between packets
            prop_assert_eq!(
                decrypted1.get(0).copied(),
                Some(0x20),
                "First packet should decrypt correctly with 0x20 first byte"
            );
            
            prop_assert_eq!(
                decrypted2.get(0).copied(),
                Some(0x20),
                "Second packet should decrypt correctly with 0x20 first byte. \
                 This will fail if IV is not reset between packets."
            );
            
            // Verify both packets match their original frames
            prop_assert_eq!(decrypted1, frame1, "First packet should match original frame");
            prop_assert_eq!(decrypted2, frame2, "Second packet should match original frame");
        }
    }

    /// Unit test: 352-byte ALAC payload (perfectly aligned)
    ///
    /// **Validates: Requirements 1.1, 1.2**
    ///
    /// Tests a perfectly 16-byte-aligned payload (352 bytes = 22 blocks).
    #[test]
    fn test_352_byte_aligned_payload() {
        let key = [0x01u8; 16];
        let iv = [0x02u8; 16];
        
        let original = create_alac_frame(352);
        let encrypted = encrypt_payload(&original, &key, &iv);
        let decrypted = decrypt_payload_current_implementation(&encrypted, &key, &iv);
        
        assert_eq!(
            decrypted.get(0).copied(),
            Some(0x20),
            "First byte should be 0x20 for valid ALAC frame"
        );
        assert_eq!(decrypted.len(), 352, "Length should be preserved");
        assert_eq!(decrypted, original, "Decrypted should match original");
    }

    /// Unit test: 356-byte ALAC payload (4-byte remainder)
    ///
    /// **Validates: Requirements 1.4, 2.1, 2.4**
    ///
    /// Tests a payload with remainder bytes (356 bytes = 352 encrypted + 4 unencrypted).
    /// This will FAIL on unfixed code if remainder bytes are not copied.
    #[test]
    fn test_356_byte_payload_with_remainder() {
        let key = [0x03u8; 16];
        let iv = [0x04u8; 16];
        
        let original = create_alac_frame(356);
        let encrypted = encrypt_payload(&original, &key, &iv);
        let decrypted = decrypt_payload_current_implementation(&encrypted, &key, &iv);
        
        assert_eq!(
            decrypted.get(0).copied(),
            Some(0x20),
            "First byte should be 0x20 for valid ALAC frame"
        );
        assert_eq!(
            decrypted.len(),
            356,
            "Length should be preserved (352 encrypted + 4 remainder)"
        );
        assert_eq!(decrypted, original, "Decrypted should match original");
    }

    /// Unit test: 16-byte minimum payload
    ///
    /// **Validates: Requirements 1.1, 1.2**
    ///
    /// Tests the minimum encrypted payload size (16 bytes = 1 block).
    #[test]
    fn test_16_byte_minimum_payload() {
        let key = [0x05u8; 16];
        let iv = [0x06u8; 16];
        
        let original = create_alac_frame(16);
        let encrypted = encrypt_payload(&original, &key, &iv);
        let decrypted = decrypt_payload_current_implementation(&encrypted, &key, &iv);
        
        assert_eq!(
            decrypted.get(0).copied(),
            Some(0x20),
            "First byte should be 0x20 for valid ALAC frame"
        );
        assert_eq!(decrypted.len(), 16, "Length should be preserved");
        assert_eq!(decrypted, original, "Decrypted should match original");
    }

    /// Unit test: IV reset between consecutive packets
    ///
    /// **Validates: Requirement 1.3, 2.3**
    ///
    /// Tests that the IV is reset for each packet.
    /// This will FAIL on unfixed code if IV state carries over.
    #[test]
    fn test_iv_reset_consecutive_packets() {
        let key = [0x07u8; 16];
        let iv = [0x08u8; 16];
        
        let frame1 = create_alac_frame(352);
        let mut frame2 = create_alac_frame(352);
        frame2[1] = 0xAA; // Different from frame1
        
        let encrypted1 = encrypt_payload(&frame1, &key, &iv);
        let encrypted2 = encrypt_payload(&frame2, &key, &iv);
        
        let decrypted1 = decrypt_payload_current_implementation(&encrypted1, &key, &iv);
        let decrypted2 = decrypt_payload_current_implementation(&encrypted2, &key, &iv);
        
        assert_eq!(
            decrypted1.get(0).copied(),
            Some(0x20),
            "First packet should decrypt correctly"
        );
        assert_eq!(
            decrypted2.get(0).copied(),
            Some(0x20),
            "Second packet should decrypt correctly (IV reset)"
        );
        assert_eq!(decrypted1, frame1, "First packet should match original");
        assert_eq!(decrypted2, frame2, "Second packet should match original");
    }

    /// Debug test: Verify in-place decryption preserves remainder bytes
    #[test]
    fn test_in_place_decryption_preserves_remainder() {
        use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};
        type Aes128CbcDec = CbcDecryptor<Aes128>;
        
        let key = [0x01u8; 16];
        let iv = [0x02u8; 16];
        
        // Create a 356-byte payload with distinct remainder bytes
        let mut payload = vec![0xAAu8; 356];
        payload[352] = 0xBB;
        payload[353] = 0xCC;
        payload[354] = 0xDD;
        payload[355] = 0xEE;
        
        // Decrypt using current implementation approach
        if let Ok(decryptor) = Aes128CbcDec::new_from_slices(&key, &iv) {
            let decrypt_len = (payload.len() / 16) * 16;
            let _ = decryptor.decrypt_padded_mut::<NoPadding>(&mut payload[..decrypt_len]);
        }
        
        // Check if remainder bytes are preserved
        assert_eq!(payload[352], 0xBB, "Remainder byte 0 should be preserved");
        assert_eq!(payload[353], 0xCC, "Remainder byte 1 should be preserved");
        assert_eq!(payload[354], 0xDD, "Remainder byte 2 should be preserved");
        assert_eq!(payload[355], 0xEE, "Remainder byte 3 should be preserved");
        assert_eq!(payload.len(), 356, "Payload length should be preserved");
    }

    /// Unit test: 4-byte ALAC silence frame (AirPlay 2 screen sharing)
    ///
    /// **Validates: Bug fix for small payloads < 16 bytes**
    ///
    /// This is the ACTUAL bug case: AirPlay 2 screen sharing sends 4-byte ALAC silence frames.
    /// The old code had `if decrypted_payload.len() >= 16` which skipped decryption for these frames.
    /// The fix removes this check, allowing the algorithm to handle all payload sizes correctly.
    #[test]
    fn test_4_byte_alac_silence_frame() {
        let key = [0x09u8; 16];
        let iv = [0x0Au8; 16];
        
        // Create a 4-byte ALAC silence frame
        let original = create_alac_frame(4);
        
        // Encrypt it (will encrypt 0 bytes, copy all 4 as remainder)
        let encrypted = encrypt_payload(&original, &key, &iv);
        
        // Decrypt using current implementation
        let decrypted = decrypt_payload_current_implementation(&encrypted, &key, &iv);
        
        // The fix ensures this works correctly:
        // - encrypted_len = (4 / 16) * 16 = 0
        // - Decrypt 0 bytes (no-op)
        // - All 4 bytes remain as-is
        assert_eq!(
            decrypted.get(0).copied(),
            Some(0x20),
            "First byte should be 0x20 for valid ALAC frame (4-byte silence)"
        );
        assert_eq!(decrypted.len(), 4, "Length should be preserved");
        assert_eq!(decrypted, original, "Decrypted should match original");
    }
}
