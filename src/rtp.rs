use anyhow::Result;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::ntp::{ClockSync, NtpTimestamp};

/// Parsed RTP packet with header fields and payload
#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

/// Jitter buffer that reorders RTP packets by sequence number.
///
/// Handles u16 sequence number wrap-around and deduplication.
/// Packets are inserted in order and popped from the front when
/// the buffer reaches a minimum fill level.
pub struct JitterBuffer {
    buffer: VecDeque<RtpPacket>,
    max_size: usize,
    min_fill: usize,
    expected_seq: Option<u16>,
    total_received: u64,
    total_dropped: u64,
    total_duplicates: u64,
    /// Incremented on each `flush()` so the audio pipeline can re-prefill.
    flush_generation: u64,
}

impl JitterBuffer {
    pub fn new(max_size: usize) -> Self {
        JitterBuffer {
            buffer: VecDeque::with_capacity(max_size),
            max_size,
            // ~500 ms prefill at 352 samples / 44.1 kHz (≈ 30 frames).
            // Increased from 12 to absorb wider packet reordering gaps and prevent underruns.
            min_fill: 30,
            expected_seq: None,
            total_received: 0,
            total_dropped: 0,
            total_duplicates: 0,
            flush_generation: 0,
        }
    }

    /// Generation counter bumped on every flush (RECORD / FLUSH / TEARDOWN).
    pub fn flush_generation(&self) -> u64 {
        self.flush_generation
    }

    /// Insert a packet in sequence order, handling u16 wrap-around.
    pub fn insert(&mut self, packet: RtpPacket) {
        self.total_received += 1;

        while self.buffer.len() >= self.max_size {
            if let Some(evicted) = self.buffer.pop_front() {
                self.total_dropped += 1;
                if self.expected_seq == Some(evicted.sequence) {
                    self.expected_seq = self.buffer.front().map(|p| p.sequence);
                }
            } else {
                break;
            }
        }

        // Fast path: most packets arrive in order (O(1) vs O(log n) binary search).
        if let Some(back) = self.buffer.back() {
            // Improved duplicate detection: check both timestamp and sequence
            if packet.sequence == back.sequence && packet.timestamp == back.timestamp {
                self.total_duplicates += 1;
                return;
            }
            if seq_compare(packet.sequence, back.sequence) == std::cmp::Ordering::Greater {
                self.buffer.push_back(packet);
                return;
            }
        } else {
            self.buffer.push_back(packet);
            return;
        }

        let pos = self.buffer.binary_search_by(|p| seq_compare(p.sequence, packet.sequence));
        match pos {
            Ok(idx) => {
                // Check if it's a true duplicate (same timestamp and sequence)
                if self.buffer[idx].timestamp == packet.timestamp {
                    self.total_duplicates += 1;
                } else {
                    // Different timestamp, same sequence (rare but possible) - insert anyway
                    self.buffer.insert(idx + 1, packet);
                }
            }
            Err(idx) => self.buffer.insert(idx, packet),
        }
    }

    /// Align playout sequence to the lowest packet currently buffered.
    pub fn sync_expected_to_front(&mut self) {
        if let Some(pkt) = self.buffer.front() {
            self.expected_seq = Some(pkt.sequence);
        }
    }

    /// RTP timestamp of the next packet to play (if any).
    pub fn front_rtp_timestamp(&self) -> Option<u32> {
        self.buffer.front().map(|p| p.timestamp)
    }

    /// Check if there's a gap at the front of the buffer.
    /// Returns Some((expected_seq, front_seq, gap_size)) if there's a gap and buffer is not backing up.
    /// Returns None if no gap or buffer is too full to wait.
    pub fn check_gap(&self) -> Option<(u16, u16, u16)> {
        if self.buffer.is_empty() {
            return None;
        }

        let expected = self.expected_seq?;
        let front_seq = self.buffer.front()?.sequence;
        
        if front_seq != expected {
            // Only request retransmit if buffer is not backing up
            if self.buffer.len() < self.max_size / 2 {
                // Calculate gap size (handle wrap-around)
                let gap_size = front_seq.wrapping_sub(expected);
                return Some((expected, front_seq, gap_size));
            }
        }
        None
    }

    /// Pop the next in-order packet (uxplay-style), waiting on gaps when possible.
    pub fn pop_next(&mut self) -> Option<RtpPacket> {
        if self.buffer.is_empty() {
            return None;
        }

        let expected = match self.expected_seq {
            Some(seq) => seq,
            None => {
                self.sync_expected_to_front();
                self.expected_seq?
            }
        };

        let front_seq = self.buffer.front()?.sequence;
        if front_seq != expected {
            // Gap — wait briefly for retransmit; skip if the buffer is backing up.
            if self.buffer.len() < self.max_size / 2 {
                return None;
            }
            debug!(
                expected = expected,
                front = front_seq,
                buf_len = self.buffer.len(),
                "RTP sequence gap — skipping to next available packet"
            );
            self.expected_seq = Some(front_seq);
        }

        let pkt = self.buffer.pop_front()?;
        self.expected_seq = Some(pkt.sequence.wrapping_add(1));
        Some(pkt)
    }

    /// Check if the buffer has enough packets to start playout.
    pub fn is_ready(&self) -> bool {
        self.buffer.len() >= self.min_fill
    }

    /// Minimum packets required before playout starts.
    pub fn min_fill(&self) -> usize {
        self.min_fill
    }

    /// Current number of packets in the buffer.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Clear all packets from the buffer (used on FLUSH/RECORD).
    pub fn flush(&mut self) {
        self.buffer.clear();
        self.expected_seq = None;
        self.flush_generation = self.flush_generation.wrapping_add(1);
        info!(
            received = self.total_received,
            dropped = self.total_dropped,
            duplicates = self.total_duplicates,
            "Jitter buffer flushed"
        );
    }

    /// Get buffer statistics
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.total_received, self.total_dropped, self.total_duplicates)
    }
}

/// Compare two u16 sequence numbers with wrapping support.
///
/// Returns Ordering based on the "shorter distance" around the u16 ring.
/// This handles the 65535→0 wrap-around correctly for sequences within
/// a window of ~32768 of each other.
#[inline]
fn seq_compare(a: u16, b: u16) -> std::cmp::Ordering {
    let diff = a.wrapping_sub(b) as i16;
    diff.cmp(&0)
}

/// Parse an RTP packet from raw bytes (RFC 3550).
pub fn parse_rtp_packet(data: &[u8]) -> Option<RtpPacket> {
    if data.len() < 12 {
        return None;
    }

    let first_byte = data[0];
    let version = (first_byte >> 6) & 0x03;
    let padding = (first_byte >> 5) & 0x01 == 1;
    let extension = (first_byte >> 4) & 0x01 == 1;
    let csrc_count = (first_byte & 0x0F) as usize;

    let second_byte = data[1];
    let marker = (second_byte >> 7) & 0x01 == 1;
    let payload_type = second_byte & 0x7F;

    let sequence = u16::from_be_bytes([data[2], data[3]]);
    let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    if version != 2 {
        warn!(version = version, "Invalid RTP version");
        return None;
    }

    let mut header_size = 12 + (csrc_count * 4);
    if data.len() < header_size {
        return None;
    }

    // Handle header extension (RFC 3550 Section 5.3.1)
    if extension && data.len() >= header_size + 4 {
        let ext_length = u16::from_be_bytes([data[header_size + 2], data[header_size + 3]]) as usize;
        header_size += 4 + (ext_length * 4);
        if data.len() < header_size {
            return None;
        }
    }

    // Handle padding
    let payload_end = if padding && !data.is_empty() {
        let pad_len = data[data.len() - 1] as usize;
        if data.len() >= header_size + pad_len {
            data.len() - pad_len
        } else {
            data.len()
        }
    } else {
        data.len()
    };

    let payload = data[header_size..payload_end].to_vec();

    Some(RtpPacket {
        version,
        padding,
        extension,
        marker,
        payload_type,
        sequence,
        timestamp,
        ssrc,
        payload,
    })
}

/// Start the RTP audio receiver on the specified port.
///
/// Receives RTP audio packets, parses them, and inserts into the jitter buffer.
pub async fn start_rtp_receiver(
    port: u16,
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
) -> Result<()> {
    let addr = format!("[::]:{}", port);
    let socket = UdpSocket::bind(&addr).await?;
    info!(port = port, "RTP audio receiver started");

    let mut buf = [0u8; 2048];

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((amt, src)) => {
                if let Some(packet) = parse_rtp_packet(&buf[..amt]) {
                    let mut buffer = jitter_buffer.lock().unwrap();
                    buffer.insert(packet);

                }
            }
            Err(e) => {
                warn!("RTP receive error: {}", e);
            }
        }
    }
}

/// Start the RTP control receiver on the specified port.
///
/// RTP control SYNC payload type (Airtunes2: 0x54).
const PAYLOAD_SYNC: u8 = 0x54;

/// Parse a 20-byte RTP control SYNC packet from the sender.
fn parse_sync_packet(data: &[u8]) -> Option<(u32, u32, NtpTimestamp, bool)> {
    if data.len() < 20 {
        return None;
    }
    let pt = data[1] & 0x7F;
    if pt != PAYLOAD_SYNC {
        return None;
    }
    let first_after_flush = (data[0] & 0x10) != 0;
    let rtp_now_minus_latency = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let remote_ntp = NtpTimestamp::from_bytes(&data[8..16])?;
    let rtp_now = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    Some((rtp_now_minus_latency, rtp_now, remote_ntp, first_after_flush))
}

/// Build a retransmit request packet (PT 85) for missing RTP sequence numbers.
///
/// Format: RTP-like header with PT=85, followed by:
///   - seq_start (2 bytes, big-endian u16): first missing sequence
///   - count (2 bytes, big-endian u16): number of packets to retransmit
fn build_retransmit_request(seq_start: u16, count: u16) -> Vec<u8> {
    let mut pkt = vec![0u8; 8];
    pkt[0] = 0x80; // V=2, P=0, X=0, CC=0
    pkt[1] = 0x80 | 85; // M=1, PT=85
    pkt[2..4].copy_from_slice(&seq_start.to_be_bytes());
    pkt[4..6].copy_from_slice(&count.to_be_bytes());
    pkt
}

/// Start a retransmit request handler that monitors jitter buffer gaps.
///
/// This spawns a background task that periodically checks the jitter buffer for gaps
/// and sends retransmit requests when needed.
pub async fn start_retransmit_handler(
    control_port: u16,
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    sender_addr: Arc<Mutex<Option<std::net::SocketAddr>>>,
) -> Result<()> {
    let socket = UdpSocket::bind(":::0").await?; // Bind to any available port
    info!("Retransmit request handler started");

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Check for gaps
        let gap_info = {
            let buffer = jitter_buffer.lock().unwrap();
            buffer.check_gap()
        };

        if let Some((expected, _front, gap_size)) = gap_info {
            // Only request reasonable gap sizes (avoid requesting too many on major loss)
            if gap_size > 0 && gap_size < 10 {
                if let Some(addr) = *sender_addr.lock().unwrap() {
                    let request = build_retransmit_request(expected, gap_size);
                    if let Err(e) = socket.send_to(&request, addr).await {
                        debug!("Failed to send retransmit request: {}", e);
                    } else {
                        debug!(
                            seq_start = expected,
                            count = gap_size,
                            "Sent retransmit request"
                        );
                    }
                }
            }
        }
    }
}

/// Parse a retransmit response packet (PT 86) to extract the embedded RTP packet.
///
/// Format: RTP-like header with PT=86, followed by the original RTP packet
fn parse_retransmit_response(data: &[u8]) -> Option<RtpPacket> {
    if data.len() < 4 {
        return None;
    }
    let pt = data[1] & 0x7F;
    if pt != 86 {
        return None;
    }
    // The embedded RTP packet starts after the 4-byte header
    if data.len() > 4 {
        parse_rtp_packet(&data[4..])
    } else {
        None
    }
}

/// Handles RTCP-like control packets (retransmission requests, sync, etc.)
pub async fn start_rtp_control(
    port: u16,
    clock_sync: Arc<RwLock<ClockSync>>,
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    sender_addr: Arc<Mutex<Option<std::net::SocketAddr>>>,
) -> Result<()> {
    let addr = format!("[::]:{}", port);
    let socket = Arc::new(UdpSocket::bind(&addr).await?);
    info!(port = port, "RTP control receiver started");

    let mut buf = [0u8; 2048];

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((amt, src)) => {
                // Update sender address for retransmit requests
                {
                    let mut addr_guard = sender_addr.lock().unwrap();
                    if addr_guard.is_none() {
                        *addr_guard = Some(src);
                    }
                }

                if amt < 4 {
                    continue;
                }

                let payload_type = buf[1] & 0x7F;
                debug!(
                    bytes = amt,
                    pt = payload_type,
                    peer = %src,
                    "RTP control packet received"
                );

                match payload_type {
                    // Retransmit request (type 85) — client asking us to resend
                    85 => {
                        debug!("Retransmit request from client (ignoring — we are receiver)");
                    }
                    // Retransmit response (type 86)
                    86 => {
                        // Client is resending a packet we requested
                        if let Some(packet) = parse_retransmit_response(&buf[..amt]) {
                            debug!(
                                seq = packet.sequence,
                                ts = packet.timestamp,
                                "Retransmit response received, inserting into jitter buffer"
                            );
                            // Insert into jitter buffer (duplicate check will handle if already received)
                            let mut buffer = jitter_buffer.lock().unwrap();
                            buffer.insert(packet);
                        } else {
                            debug!(bytes = amt, "Failed to parse retransmit response");
                        }
                    }
                    // Sync packet (PT 0x54 / 84 decimal)
                    0x54 => {
                        if let Some((rtp_minus_lat, rtp_now, remote_ntp, first)) =
                            parse_sync_packet(&buf[..amt])
                        {
                            if let Ok(mut sync) = clock_sync.write() {
                                sync.apply_sync_packet(rtp_minus_lat, rtp_now, remote_ntp, first);
                            }
                        } else {
                            debug!(bytes = amt, "Failed to parse SYNC packet");
                        }
                    }
                    _ => {
                        debug!(pt = payload_type, "Unknown control packet type");
                    }
                }
            }
            Err(e) => {
                warn!("RTP control receive error: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid RTP packet
    fn make_rtp(seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0u8; 12 + payload.len()];
        pkt[0] = 0x80; // V=2, P=0, X=0, CC=0
        pkt[1] = 96;   // M=0, PT=96
        pkt[2..4].copy_from_slice(&seq.to_be_bytes());
        pkt[4..8].copy_from_slice(&ts.to_be_bytes());
        pkt[8..12].copy_from_slice(&0u32.to_be_bytes()); // SSRC
        pkt[12..].copy_from_slice(payload);
        pkt
    }

    #[test]
    fn test_parse_sync_packet() {
        let mut pkt = [0u8; 20];
        pkt[0] = 0x90;
        pkt[1] = 0x80 | 0x54;
        pkt[4..8].copy_from_slice(&1000u32.to_be_bytes());
        pkt[16..20].copy_from_slice(&5000u32.to_be_bytes());
        let (minus, now, _, first) = parse_sync_packet(&pkt).unwrap();
        assert_eq!(minus, 1000);
        assert_eq!(now, 5000);
        assert!(first);
    }

    #[test]
    fn test_parse_rtp_basic() {
        let data = make_rtp(42, 1000, &[0xAA, 0xBB, 0xCC]);
        let pkt = parse_rtp_packet(&data).unwrap();
        assert_eq!(pkt.version, 2);
        assert_eq!(pkt.sequence, 42);
        assert_eq!(pkt.timestamp, 1000);
        assert_eq!(pkt.payload_type, 96);
        assert!(!pkt.marker);
        assert_eq!(pkt.payload, vec![0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn test_parse_rtp_too_short() {
        assert!(parse_rtp_packet(&[0; 11]).is_none());
    }

    #[test]
    fn test_parse_rtp_wrong_version() {
        let mut data = make_rtp(1, 1, &[]);
        data[0] = 0x40; // V=1
        assert!(parse_rtp_packet(&data).is_none());
    }

    #[test]
    fn test_parse_rtp_with_marker() {
        let mut data = make_rtp(1, 1, &[0xFF]);
        data[1] = 0x80 | 96; // M=1, PT=96
        let pkt = parse_rtp_packet(&data).unwrap();
        assert!(pkt.marker);
    }

    #[test]
    fn test_jitter_buffer_ordering() {
        let mut jb = JitterBuffer::new(10);

        // Insert out of order
        jb.insert(parse_rtp_packet(&make_rtp(3, 300, &[3])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(1, 100, &[1])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(2, 200, &[2])).unwrap());

        // Should come out in order
        jb.sync_expected_to_front();
        assert_eq!(jb.pop_next().unwrap().sequence, 1);
        assert_eq!(jb.pop_next().unwrap().sequence, 2);
        assert_eq!(jb.pop_next().unwrap().sequence, 3);
    }

    #[test]
    fn test_jitter_buffer_duplicates() {
        let mut jb = JitterBuffer::new(10);

        jb.insert(parse_rtp_packet(&make_rtp(1, 100, &[1])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(1, 100, &[1])).unwrap());

        assert_eq!(jb.len(), 1);
        assert_eq!(jb.stats().2, 1); // 1 duplicate
    }

    #[test]
    fn test_jitter_buffer_wrap_around() {
        let mut jb = JitterBuffer::new(10);

        // Insert packets around the u16 wrap point
        jb.insert(parse_rtp_packet(&make_rtp(65534, 100, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(0, 300, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(65535, 200, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(1, 400, &[])).unwrap());

        // Should come out in wrapping order: 65534, 65535, 0, 1
        jb.sync_expected_to_front();
        assert_eq!(jb.pop_next().unwrap().sequence, 65534);
        assert_eq!(jb.pop_next().unwrap().sequence, 65535);
        assert_eq!(jb.pop_next().unwrap().sequence, 0);
        assert_eq!(jb.pop_next().unwrap().sequence, 1);
    }

    #[test]
    fn test_jitter_buffer_full() {
        let mut jb = JitterBuffer::new(3);

        jb.insert(parse_rtp_packet(&make_rtp(1, 100, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(2, 200, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(3, 300, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(4, 400, &[])).unwrap()); // evicts seq 1

        assert_eq!(jb.len(), 3);
        assert_eq!(jb.stats().1, 1); // 1 evicted
        jb.sync_expected_to_front();
        assert_eq!(jb.pop_next().unwrap().sequence, 2);
    }

    #[test]
    fn test_jitter_buffer_flush() {
        let mut jb = JitterBuffer::new(10);
        jb.insert(parse_rtp_packet(&make_rtp(1, 100, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(2, 200, &[])).unwrap());
        jb.flush();
        assert!(jb.is_empty());
    }

    #[test]
    fn test_jitter_buffer_is_ready() {
        let mut jb = JitterBuffer::new(32);
        let min = jb.min_fill();
        assert!(!jb.is_ready());
        for seq in 1..=min as u16 {
            jb.insert(parse_rtp_packet(&make_rtp(seq, 100 * seq as u32, &[])).unwrap());
        }
        assert!(jb.is_ready());
    }

    #[test]
    fn test_seq_compare() {
        use std::cmp::Ordering;
        assert_eq!(seq_compare(1, 2), Ordering::Less);
        assert_eq!(seq_compare(2, 1), Ordering::Greater);
        assert_eq!(seq_compare(5, 5), Ordering::Equal);
        // Wrap-around: 65535 < 0 (65535 is "before" 0)
        assert_eq!(seq_compare(65535, 0), Ordering::Less);
        assert_eq!(seq_compare(0, 65535), Ordering::Greater);
    }

    #[test]
    fn test_improved_duplicate_detection() {
        let mut jb = JitterBuffer::new(10);

        // Insert packet with seq=1, ts=100
        jb.insert(parse_rtp_packet(&make_rtp(1, 100, &[1])).unwrap());
        
        // Insert duplicate with same seq and ts - should be detected
        jb.insert(parse_rtp_packet(&make_rtp(1, 100, &[1])).unwrap());
        assert_eq!(jb.len(), 1);
        assert_eq!(jb.stats().2, 1); // 1 duplicate detected

        // Insert packet with same seq but different ts - should NOT be duplicate
        jb.insert(parse_rtp_packet(&make_rtp(1, 200, &[2])).unwrap());
        assert_eq!(jb.len(), 2); // Both packets kept
        assert_eq!(jb.stats().2, 1); // Still only 1 duplicate
    }

    #[test]
    fn test_timestamp_sequence_duplicate_detection() {
        let mut jb = JitterBuffer::new(10);

        // Scenario: sender retransmits with same timestamp but different sequence
        jb.insert(parse_rtp_packet(&make_rtp(100, 1000, &[1])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(101, 1000, &[2])).unwrap());
        
        // Both should be kept since they have different sequences
        assert_eq!(jb.len(), 2);
    }

    #[test]
    fn test_build_retransmit_request() {
        let req = build_retransmit_request(100, 5);
        assert_eq!(req.len(), 8);
        assert_eq!(req[0], 0x80); // V=2
        assert_eq!(req[1], 0x80 | 85); // M=1, PT=85
        
        let seq_start = u16::from_be_bytes([req[2], req[3]]);
        let count = u16::from_be_bytes([req[4], req[5]]);
        assert_eq!(seq_start, 100);
        assert_eq!(count, 5);
    }

    #[test]
    fn test_parse_retransmit_response() {
        // Build a retransmit response with embedded RTP packet
        let embedded_rtp = make_rtp(42, 1000, &[0xAA, 0xBB]);
        let mut response = vec![0u8; 4 + embedded_rtp.len()];
        response[0] = 0x80; // V=2
        response[1] = 0x80 | 86; // M=1, PT=86
        response[4..].copy_from_slice(&embedded_rtp);
        
        let parsed = parse_retransmit_response(&response).unwrap();
        assert_eq!(parsed.sequence, 42);
        assert_eq!(parsed.timestamp, 1000);
        assert_eq!(parsed.payload, vec![0xAA, 0xBB]);
    }

    #[test]
    fn test_jitter_buffer_check_gap() {
        let mut jb = JitterBuffer::new(10);
        
        // No gap when empty
        assert!(jb.check_gap().is_none());
        
        // Insert packets with a gap: seq 100, 102, 103 (missing 101)
        jb.insert(parse_rtp_packet(&make_rtp(100, 1000, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(102, 1200, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(103, 1300, &[])).unwrap());
        
        jb.sync_expected_to_front(); // Set expected to 100
        jb.pop_next(); // Pop 100, now expected is 101
        
        // Should detect gap: expected 101, front is 102
        let gap = jb.check_gap();
        assert!(gap.is_some());
        let (expected, front, gap_size) = gap.unwrap();
        assert_eq!(expected, 101);
        assert_eq!(front, 102);
        assert_eq!(gap_size, 1);
    }

    #[test]
    fn test_jitter_buffer_no_gap_when_full() {
        let mut jb = JitterBuffer::new(10);
        
        // Fill buffer to more than half capacity
        for i in 100..107 {
            jb.insert(parse_rtp_packet(&make_rtp(i, i as u32 * 100, &[])).unwrap());
        }
        
        jb.sync_expected_to_front();
        jb.pop_next(); // Pop 100
        
        // Insert packet creating a gap but buffer is > max_size/2
        jb.insert(parse_rtp_packet(&make_rtp(110, 11000, &[])).unwrap());
        
        // Should not report gap because buffer is too full
        assert!(jb.check_gap().is_none());
    }

    #[test]
    fn test_seq_compare_duplicate() {
        use std::cmp::Ordering;
        assert_eq!(seq_compare(1, 2), Ordering::Less);
        assert_eq!(seq_compare(2, 1), Ordering::Greater);
        assert_eq!(seq_compare(5, 5), Ordering::Equal);
        // Wrap-around: 65535 < 0 (65535 is "before" 0)
        assert_eq!(seq_compare(65535, 0), Ordering::Less);
        assert_eq!(seq_compare(0, 65535), Ordering::Greater);
    }
}
