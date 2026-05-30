use anyhow::Result;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

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
}

impl JitterBuffer {
    pub fn new(max_size: usize) -> Self {
        JitterBuffer {
            buffer: VecDeque::with_capacity(max_size),
            max_size,
            min_fill: max_size / 4, // 25% fill before playout starts
            expected_seq: None,
            total_received: 0,
            total_dropped: 0,
            total_duplicates: 0,
        }
    }

    /// Insert a packet in sequence order, handling u16 wrap-around.
    pub fn insert(&mut self, packet: RtpPacket) {
        self.total_received += 1;

        // Set initial expected sequence
        if self.expected_seq.is_none() {
            self.expected_seq = Some(packet.sequence);
        }

        // Find insertion position using wrapping comparison
        let pos = self.buffer.binary_search_by(|p| {
            seq_compare(p.sequence, packet.sequence)
        });

        match pos {
            Ok(_) => {
                // Duplicate packet
                self.total_duplicates += 1;
                debug!(seq = packet.sequence, "Duplicate RTP packet, ignoring");
            }
            Err(idx) => {
                if self.buffer.len() < self.max_size {
                    debug!(
                        seq = packet.sequence,
                        pos = idx,
                        buf_len = self.buffer.len(),
                        "Inserted RTP packet"
                    );
                    self.buffer.insert(idx, packet);
                } else {
                    self.total_dropped += 1;
                    warn!(
                        seq = packet.sequence,
                        dropped = self.total_dropped,
                        "Jitter buffer full, dropping packet"
                    );
                }
            }
        }
    }

    /// Pop the next packet from the front of the buffer.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        let pkt = self.buffer.pop_front()?;
        self.expected_seq = Some(pkt.sequence.wrapping_add(1));
        Some(pkt)
    }

    /// Check if the buffer has enough packets to start playout.
    pub fn is_ready(&self) -> bool {
        self.buffer.len() >= self.min_fill
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

    debug!(
        seq = sequence,
        ts = timestamp,
        pt = payload_type,
        marker = marker,
        payload_len = payload.len(),
        "RTP packet parsed"
    );

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

                    if buffer.len().is_multiple_of(100) {
                        debug!(
                            buf_len = buffer.len(),
                            peer = %src,
                            "Jitter buffer status"
                        );
                    }
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
/// Handles RTCP-like control packets (retransmission requests, sync, etc.)
pub async fn start_rtp_control(port: u16) -> Result<()> {
    let addr = format!("[::]:{}", port);
    let socket = UdpSocket::bind(&addr).await?;
    info!(port = port, "RTP control receiver started");

    let mut buf = [0u8; 2048];

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((amt, src)) => {
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
                        debug!("Retransmit response from client");
                        // TODO: extract the embedded RTP packet and insert into jitter buffer
                    }
                    // Sync packet (type 84)
                    84 => {
                        debug!("Sync packet received");
                        // Contains RTP timestamp correlation info
                        // TODO: update clock sync
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
        assert_eq!(jb.pop().unwrap().sequence, 1);
        assert_eq!(jb.pop().unwrap().sequence, 2);
        assert_eq!(jb.pop().unwrap().sequence, 3);
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
        assert_eq!(jb.pop().unwrap().sequence, 65534);
        assert_eq!(jb.pop().unwrap().sequence, 65535);
        assert_eq!(jb.pop().unwrap().sequence, 0);
        assert_eq!(jb.pop().unwrap().sequence, 1);
    }

    #[test]
    fn test_jitter_buffer_full() {
        let mut jb = JitterBuffer::new(3);

        jb.insert(parse_rtp_packet(&make_rtp(1, 100, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(2, 200, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(3, 300, &[])).unwrap());
        jb.insert(parse_rtp_packet(&make_rtp(4, 400, &[])).unwrap()); // should drop

        assert_eq!(jb.len(), 3);
        assert_eq!(jb.stats().1, 1); // 1 dropped
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
        let mut jb = JitterBuffer::new(8); // min_fill = 2
        assert!(!jb.is_ready());
        jb.insert(parse_rtp_packet(&make_rtp(1, 100, &[])).unwrap());
        assert!(!jb.is_ready());
        jb.insert(parse_rtp_packet(&make_rtp(2, 200, &[])).unwrap());
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
}
