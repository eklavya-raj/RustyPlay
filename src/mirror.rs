use anyhow::Result;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};
use aes::cipher::{BlockEncrypt, KeyInit};

use crate::codec::SessionInfo;
use crate::video::pipeline::VideoPipeline;

type Aes128 = aes::Aes128;

/// Stateful AES-128-CTR decryptor designed specifically for AirPlay mirroring streams.
///
/// Implements block-boundary counter alignment and leftover keystream buffering
/// to perfectly match `uxplay`'s `mirror_buffer_decrypt` implementation.
pub struct MirrorDecryptor {
    aes: Aes128,
    counter: [u8; 16],
    keystream_block: [u8; 16],
    keystream_pos: usize,
}

impl MirrorDecryptor {
    pub fn new(key: &[u8; 16], iv: &[u8; 16]) -> Self {
        let aes = Aes128::new_from_slice(key).unwrap();
        Self {
            aes,
            counter: *iv,
            keystream_block: [0; 16],
            keystream_pos: 0,
        }
    }

    /// Decrypts encrypted video payloads in-place.
    pub fn decrypt_in_place(&mut self, data: &mut [u8]) {
        let mut in_pos = 0;
        let input_len = data.len();

        // 1. Consume leftover keystream from the previous packet's remainder
        if self.keystream_pos > 0 {
            let consume = std::cmp::min(self.keystream_pos, input_len);
            let og_offset = 16 - self.keystream_pos;
            for i in 0..consume {
                data[i] ^= self.keystream_block[og_offset + i];
            }
            self.keystream_pos -= consume;
            in_pos += consume;
        }

        // 2. Decrypt complete 16-byte blocks
        let remaining = input_len - in_pos;
        let encrypt_len = (remaining / 16) * 16;
        for _ in 0..(encrypt_len / 16) {
            let mut keystream = self.counter;
            self.aes.encrypt_block((&mut keystream).into());
            increment_counter(&mut self.counter);
            for i in 0..16 {
                data[in_pos + i] ^= keystream[i];
            }
            in_pos += 16;
        }

        // 3. Handle remainder bytes and save leftover keystream
        let rest_len = input_len - in_pos;
        if rest_len > 0 {
            let mut keystream = self.counter;
            self.aes.encrypt_block((&mut keystream).into());
            increment_counter(&mut self.counter);
            for i in 0..rest_len {
                data[in_pos + i] ^= keystream[i];
            }
            self.keystream_block = keystream;
            self.keystream_pos = 16 - rest_len;
        }
    }
}

/// Increment a 128-bit big-endian counter (standard AES CTR counter increment).
fn increment_counter(counter: &mut [u8; 16]) {
    for i in (0..16).rev() {
        counter[i] = counter[i].wrapping_add(1);
        if counter[i] != 0 {
            break;
        }
    }
}

/// Start the AirPlay 2 screen mirroring TCP server on the specified port (typically 7100).
pub async fn start_mirroring_server(port: u16, session_info: Arc<RwLock<Option<SessionInfo>>>) -> Result<()> {
    let addr = format!("[::]:{}", port);
    let listener = TcpListener::bind(&addr).await?;
    info!(port = port, "Mirroring TCP server started");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!(peer = %peer, "Mirroring client connection accepted");
                let session_info_clone = session_info.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, session_info_clone).await {
                        warn!(peer = %peer, error = ?e, "Mirroring client session ended with error");
                    } else {
                        info!(peer = %peer, "Mirroring client session ended successfully");
                    }
                });
            }
            Err(e) => {
                warn!("Mirroring server accept error: {:?}", e);
            }
        }
    }
}

use std::sync::atomic::{AtomicBool, Ordering};

pub static FORCE_SOFTWARE_DECODER: AtomicBool = AtomicBool::new(false);

/// Handle a single mirroring TCP stream session.
async fn handle_client(mut stream: tokio::net::TcpStream, session_info: Arc<RwLock<Option<SessionInfo>>>) -> Result<()> {
    // 1. Wait/Poll for derived video keys to be populated in session_info
    let mut decryptor = None;
    for _ in 0..200 {
        let keys = {
            let guard = session_info.read().unwrap();
            if let Some(ref si) = *guard {
                if let (Some(key), Some(iv)) = (&si.video_aes_key, &si.video_aes_iv) {
                    Some((key.clone(), iv.clone()))
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some((key, iv)) = keys {
            let mut key_16 = [0u8; 16];
            let mut iv_16 = [0u8; 16];
            key_16.copy_from_slice(&key[..16]);
            iv_16.copy_from_slice(&iv[..16]);
            decryptor = Some(MirrorDecryptor::new(&key_16, &iv_16));
            info!("Mirroring decryptor initialized with derived video keys");
            break;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    let mut decryptor = match decryptor {
        Some(d) => d,
        None => {
            anyhow::bail!("Failed to initialize mirroring decryptor: no video keys available in session_info");
        }
    };

    // Initialize video pipeline (lazy initialization on first video frame or codec config)
    let mut video_pipeline: Option<VideoPipeline> = None;

    let mut header = [0u8; 128];
    loop {
        // Read 128-byte frame header
        stream.read_exact(&mut header).await?;

        // Extract metadata fields from the 128-byte header
        let payload_size = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_type = header[4];
        let payload_option = header[6];
        let ntp_timestamp_raw = u64::from_le_bytes(header[8..16].try_into().unwrap());

        // Read payload body
        let mut payload = vec![0u8; payload_size];
        if payload_size > 0 {
            stream.read_exact(&mut payload).await?;
        }

        match payload_type {
            0x00 => {
                // Encrypted video frame (VCL NAL)
                // Decrypt payload in-place
                decryptor.decrypt_in_place(&mut payload);

                // Convert AVCC length prefixes (4-byte big-endian) to Annex-B start codes ([0, 0, 0, 1])
                let mut offset = 0;
                let mut nal_units = Vec::new();
                let mut is_valid = true;

                while offset < payload_size {
                    if offset + 4 > payload_size {
                        is_valid = false;
                        break;
                    }
                    let nalu_len = u32::from_be_bytes(payload[offset..offset+4].try_into().unwrap()) as usize;
                    if offset + 4 + nalu_len > payload_size {
                        is_valid = false;
                        break;
                    }

                    // Validate forbidden_zero_bit (must be 0) of H.264 NAL header
                    if (payload[offset + 4] & 0x80) != 0 {
                        is_valid = false;
                        break;
                    }

                    // Convert to Annex-B start code
                    payload[offset..offset+4].copy_from_slice(&[0, 0, 0, 1]);

                    // Parse H.264 NAL Unit Type
                    let nalu_type = payload[offset + 4] & 0x1f;
                    let nalu_type_str = match nalu_type {
                        1 => "Non-IDR Slice",
                        5 => "IDR Slice",
                        6 => "SEI",
                        7 => "SPS",
                        8 => "PPS",
                        _ => "Other NAL",
                    };
                    nal_units.push((nalu_type_str, nalu_len));

                    offset += 4 + nalu_len;
                }

                if is_valid {
                    info!(
                        payload_size = payload_size,
                        ntp_timestamp = ntp_timestamp_raw,
                        option = payload_option,
                        nal_units = ?nal_units,
                        "Received video frame (decrypted & Annex-B formatted)"
                    );

                    // Process frame through VideoPipeline (only if already initialized)
                    // Note: VideoPipeline should be initialized by a valid codec config (type 0x01) first
                    if let Some(ref mut pipeline) = video_pipeline {
                        if let Err(e) = pipeline.process_nal_unit(&payload) {
                            warn!("Failed to process video frame: {:?}", e);
                            // If window was closed, break the loop
                            if !pipeline.is_window_open() {
                                info!("Video window closed by user, ending session");
                                break;
                            }
                        }
                    } else {
                        // VideoPipeline not initialized yet - waiting for valid codec config
                        debug!("Skipping video frame - waiting for valid codec configuration");
                    }
                } else {
                    warn!(
                        payload_size = payload_size,
                        ntp_timestamp = ntp_timestamp_raw,
                        "Received malformed video frame or failed decryption"
                    );
                }
            }

            0x01 => {
                // Unencrypted codec configuration packet / resolution changes
                let width_source = f32::from_le_bytes(header[40..44].try_into().unwrap());
                let height_source = f32::from_le_bytes(header[44..48].try_into().unwrap());
                let width = f32::from_le_bytes(header[56..60].try_into().unwrap());
                let height = f32::from_le_bytes(header[60..64].try_into().unwrap());

                info!(
                    width_source = width_source,
                    height_source = height_source,
                    width = width,
                    height = height,
                    payload_size = payload_size,
                    first_bytes = ?&payload[..std::cmp::min(16, payload_size)],
                    "Received video codec config / resolution change"
                );

                if payload_size > 0 {
                    // Parse avcC (AVCC extradata) format and convert to Annex-B
                    // avcC format: [version, profile, compat, level, lengthSize, numSPS, spsLen, sps, numPPS, ppsLen, pps]
                    
                    if payload_size >= 7 && payload[0] == 1 {
                        // This is avcC format
                        info!("Parsing avcC format codec config");
                        
                        let mut annexb_data = Vec::new();
                        let mut offset = 5; // Skip version, profile, compat, level, lengthSize
                        
                        // Parse SPS
                        let num_sps = (payload[offset] & 0x1F) as usize;
                        offset += 1;
                        
                        for _ in 0..num_sps {
                            if offset + 2 > payload_size {
                                warn!("Invalid avcC: insufficient data for SPS length");
                                break;
                            }
                            let sps_len = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                            offset += 2;
                            
                            if offset + sps_len > payload_size {
                                warn!("Invalid avcC: SPS length exceeds payload");
                                break;
                            }
                            
                            // Add Annex-B start code
                            annexb_data.extend_from_slice(&[0, 0, 0, 1]);
                            // Add SPS data
                            annexb_data.extend_from_slice(&payload[offset..offset + sps_len]);
                            offset += sps_len;
                        }
                        
                        // Parse PPS
                        if offset < payload_size {
                            let num_pps = payload[offset] as usize;
                            offset += 1;
                            
                            for _ in 0..num_pps {
                                if offset + 2 > payload_size {
                                    warn!("Invalid avcC: insufficient data for PPS length");
                                    break;
                                }
                                let pps_len = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                                offset += 2;
                                
                                if offset + pps_len > payload_size {
                                    warn!("Invalid avcC: PPS length exceeds payload");
                                    break;
                                }
                                
                                // Add Annex-B start code
                                annexb_data.extend_from_slice(&[0, 0, 0, 1]);
                                // Add PPS data
                                annexb_data.extend_from_slice(&payload[offset..offset + pps_len]);
                                offset += pps_len;
                            }
                        }
                        
                        if !annexb_data.is_empty() {
                            info!(
                                annexb_size = annexb_data.len(),
                                "Converted avcC to Annex-B format"
                            );
                            payload = annexb_data;
                        } else {
                            warn!("Failed to parse avcC format");
                        }
                    }
                    
                    // Now payload should be in Annex-B format
                    // Lazy-init or reconfigure VideoPipeline with codec configuration
                    if let Some(ref mut pipeline) = video_pipeline {
                        // Pipeline already exists, reconfigure it
                        if let Err(e) = pipeline.process_codec_config(&payload, width, height) {
                            warn!("Failed to process codec config: {:?}", e);
                        }
                    } else {
                        // Initialize pipeline with codec config
                        let use_hardware = !FORCE_SOFTWARE_DECODER.load(Ordering::Relaxed);
                        match VideoPipeline::new(use_hardware) {
                            Ok(mut pipeline) => {
                                if let Err(e) = pipeline.process_codec_config(&payload, width, height) {
                                    warn!("Failed to process initial codec config: {:?}", e);
                                } else {
                                    info!("VideoPipeline initialized with codec config");
                                    video_pipeline = Some(pipeline);
                                }
                            }
                            Err(e) => {
                                warn!("Failed to initialize VideoPipeline: {:?}", e);
                            }
                        }
                    }
                }
            }

            0x02 => {
                debug!("Received old-protocol once-per-second packet");
            }

            0x05 => {
                // Performance / Activity report (binary plist)
                if payload_size > 0 {
                    let cursor = std::io::Cursor::new(&payload);
                    if let Ok(plist_val) = plist::Value::from_reader(cursor) {
                        if let Some(dict) = plist_val.as_dictionary() {
                            let tx_usage = dict.get("txUsageAvg").and_then(|v| v.as_real());
                            info!(tx_usage = ?tx_usage, "Received client performance/activity report");
                        }
                    }
                }
            }

            _ => {
                info!(
                    payload_type = payload_type,
                    payload_size = payload_size,
                    "Received unknown mirroring packet type"
                );
            }
        }
    }

    // Cleanup: shutdown video pipeline if it was initialized
    if let Some(mut pipeline) = video_pipeline {
        if let Err(e) = pipeline.shutdown() {
            warn!("Error shutting down video pipeline: {:?}", e);
        } else {
            info!("Video pipeline shut down successfully");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_increment_counter() {
        let mut ctr = [0u8; 16];
        ctr[15] = 255;
        increment_counter(&mut ctr);
        assert_eq!(ctr[14], 1);
        assert_eq!(ctr[15], 0);
    }

    #[test]
    fn test_mirror_decryptor_roundtrip() {
        let key = [0x55u8; 16];
        let iv = [0xAAu8; 16];
        
        let mut enc = MirrorDecryptor::new(&key, &iv);
        let mut dec = MirrorDecryptor::new(&key, &iv);
        
        let mut plaintext = b"Hello, AirPlay mirroring decryption!".to_vec();
        let original = plaintext.clone();
        
        // Encrypt in place (encryption and decryption are symmetric in CTR mode)
        enc.decrypt_in_place(&mut plaintext);
        
        // Verify it changed
        assert_ne!(plaintext, original);
        
        // Decrypt in place
        dec.decrypt_in_place(&mut plaintext);
        
        // Verify roundtrip succeeds
        assert_eq!(plaintext, original);
    }
}
