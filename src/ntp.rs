use anyhow::Result;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// RTSP `Audio-Latency` for AirPlay-class receivers: 11025 samples @ 44.1 kHz ≈ 250 ms.
pub const AUDIO_LATENCY_SAMPLES: u32 = 11025;

/// Reject NTP samples whose implied offset is nonsensical (e.g. t1 still zero).
const MAX_ABS_OFFSET_MICROS: i64 = 50_000_000; // 50 seconds

/// iPhone timing fields use a different epoch than our NTP transmit (~1.78e15 µs below).
const TIMING_EPOCH_GAP_US: i64 = 500_000_000_000_000;

/// Map iPhone receive/send timestamps into the same era as our NTP `t1`/`t4`.
fn normalize_peer_timing_us(local_us: i64, peer_us: i64) -> i64 {
    if local_us > peer_us && (local_us - peer_us) > TIMING_EPOCH_GAP_US {
        peer_us + (local_us - peer_us)
    } else {
        peer_us
    }
}

/// AirPlay timing packet header byte (request = 0x52, response = 0x53)
const TIMING_REQUEST: u8 = 0x52;  // payload type 82
const TIMING_RESPONSE: u8 = 0x53; // payload type 83

/// NTP timestamp: seconds since 1900-01-01 (32-bit) + fraction (32-bit)
#[derive(Debug, Clone, Copy, Default)]
pub struct NtpTimestamp {
    pub seconds: u32,
    pub fraction: u32,
}

impl NtpTimestamp {
    /// Create an NTP timestamp from the current system time.
    pub fn now() -> Self {
        // NTP epoch is Jan 1, 1900. Unix epoch is Jan 1, 1970.
        // Difference is 70 years = 2208988800 seconds
        const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

        let duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();

        let seconds = (duration.as_secs() + NTP_EPOCH_OFFSET) as u32;
        // Convert nanoseconds fraction to NTP fraction (2^32 / 10^9)
        let fraction = ((duration.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000) as u32;

        NtpTimestamp { seconds, fraction }
    }

    /// Convert to total microseconds (for offset calculations)
    pub fn to_micros(self) -> i64 {
        let secs = self.seconds as i64 * 1_000_000;
        let frac = (self.fraction as i64 * 1_000_000) >> 32;
        secs + frac
    }

    /// Parse from 8 bytes (big-endian seconds + fraction)
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        Some(NtpTimestamp {
            seconds: u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
            fraction: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        })
    }

    /// Serialize to 8 bytes (big-endian)
    pub fn to_bytes(self) -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[0..4].copy_from_slice(&self.seconds.to_be_bytes());
        buf[4..8].copy_from_slice(&self.fraction.to_be_bytes());
        buf
    }
}

/// Clock synchronization state, shared between NTP handler and audio pipeline.
#[derive(Debug, Clone)]
pub struct ClockSync {
    /// Estimated offset: server_time = client_time + offset (in microseconds)
    offset_micros: i64,
    /// Round-trip delay in microseconds
    rtt_micros: i64,
    /// Number of sync samples received
    sample_count: u32,
    /// When the last sync occurred (server monotonic time)
    last_sync: Option<Instant>,
    /// RTP clock rate (usually 44100 for audio)
    rtp_clock_rate: u32,
    /// Reference RTP timestamp (from RECORD RTP-Info header)
    rtp_reference: u32,
    /// Server monotonic time when `rtp_reference` should be presented to the listener
    reference_time: Option<Instant>,
    /// Negotiated receiver latency in RTP sample units (default 11025)
    audio_latency_samples: u32,
    /// RTP control SYNC packets applied (refines anchor)
    sync_count: u32,
}

impl ClockSync {
    pub fn new(rtp_clock_rate: u32) -> Self {
        ClockSync {
            offset_micros: 0,
            rtt_micros: 0,
            sample_count: 0,
            last_sync: None,
            rtp_clock_rate,
            rtp_reference: 0,
            reference_time: None,
            audio_latency_samples: AUDIO_LATENCY_SAMPLES,
            sync_count: 0,
        }
    }

    /// Update the clock offset from an NTP exchange.
    ///
    /// Standard NTP offset formula:
    ///   offset = ((t2 - t1) + (t3 - t4)) / 2
    ///   rtt = (t4 - t1) - (t3 - t2)
    ///
    /// where:
    ///   t1 = client transmit time (from request)
    ///   t2 = server receive time
    ///   t3 = server transmit time
    ///   t4 = client receive time (when we get the response)
    pub fn update(&mut self, client_send: NtpTimestamp, server_recv: NtpTimestamp, server_send: NtpTimestamp, client_recv: NtpTimestamp) {
        let t1 = client_send.to_micros();
        let t2 = normalize_peer_timing_us(t1, server_recv.to_micros());
        let t3 = normalize_peer_timing_us(t1, server_send.to_micros());
        let t4 = client_recv.to_micros();

        let offset = ((t2 - t1) + (t3 - t4)) / 2;
        let rtt = (t4 - t1) - (t3 - t2);

        if t1 == 0 || offset.abs() > MAX_ABS_OFFSET_MICROS || rtt < 0 || rtt > MAX_ABS_OFFSET_MICROS {
            debug!(
                t1_us = t1,
                t2_us = t2,
                t3_us = t3,
                t4_us = t4,
                offset_us = offset,
                rtt_us = rtt,
                "Rejecting invalid NTP timing sample"
            );
            return;
        }

        // Apply exponential smoothing for stability
        if self.sample_count == 0 {
            self.offset_micros = offset;
            self.rtt_micros = rtt;
        } else {
            // 75% old + 25% new for smoothing
            self.offset_micros = (self.offset_micros * 3 + offset) / 4;
            self.rtt_micros = (self.rtt_micros * 3 + rtt) / 4;
        }

        self.sample_count += 1;
        self.last_sync = Some(Instant::now());

        debug!(
            offset_us = self.offset_micros,
            rtt_us = self.rtt_micros,
            samples = self.sample_count,
            "Clock sync updated"
        );
    }

    /// Get the current estimated clock offset in microseconds
    pub fn offset(&self) -> i64 {
        self.offset_micros
    }

    /// Get the round-trip time in microseconds
    pub fn rtt(&self) -> i64 {
        self.rtt_micros
    }

    /// Receiver playout delay as wall-clock duration.
    pub fn playout_latency(&self) -> Duration {
        Duration::from_micros(
            (self.audio_latency_samples as u64 * 1_000_000) / self.rtp_clock_rate as u64,
        )
    }

    /// Set the RTP anchor from RECORD `RTP-Info` (playout starts after negotiated latency).
    pub fn set_rtp_reference(&mut self, rtp_ts: u32) {
        self.rtp_reference = rtp_ts;
        self.reference_time = Some(Instant::now() + self.playout_latency());
        info!(
            rtp_reference = rtp_ts,
            latency_ms = self.playout_latency().as_millis(),
            "RTP reference timestamp set (RECORD)"
        );
    }

    /// Update the playout anchor from an RTP control SYNC packet (Airtunes2 / shairport-sync).
    ///
    /// `rtp_now_minus_latency` is the RTP timestamp playing at the receiver *now*;
    /// `rtp_now` is the sender's next transmit timestamp.
    pub fn apply_sync_packet(
        &mut self,
        rtp_now_minus_latency: u32,
        rtp_now: u32,
        _remote_ntp: NtpTimestamp,
        first_after_flush: bool,
    ) {
        let computed = rtp_now.wrapping_sub(self.audio_latency_samples);
        let anchor_rtp = if rtp_now_minus_latency != 0 {
            rtp_now_minus_latency
        } else {
            computed
        };

        self.rtp_reference = anchor_rtp;
        self.reference_time = Some(Instant::now());
        self.sync_count += 1;

        info!(
            anchor_rtp = anchor_rtp,
            rtp_now = rtp_now,
            first_after_flush = first_after_flush,
            sync_count = self.sync_count,
            "RTP playout anchor updated from SYNC"
        );
    }

    /// Clear anchor after RTSP FLUSH / RECORD (wait for next SYNC or RECORD).
    pub fn reset_playout_anchor(&mut self) {
        self.reference_time = None;
        self.rtp_reference = 0;
        debug!("RTP playout anchor cleared");
    }

    /// Number of SYNC packets processed.
    pub fn sync_count(&self) -> u32 {
        self.sync_count
    }

    /// Convert an RTP timestamp to a playout Instant on the server.
    ///
    /// Uses the RTP clock rate to compute elapsed time from the reference point.
    pub fn rtp_to_playout_time(&self, rtp_ts: u32) -> Option<Instant> {
        let ref_time = self.reference_time?;

        // Handle u32 wrap; timestamps far behind reference play immediately.
        let elapsed_samples = rtp_ts.wrapping_sub(self.rtp_reference);
        if elapsed_samples > (1u32 << 31) {
            return Some(ref_time);
        }

        let elapsed_micros = (elapsed_samples as u64 * 1_000_000) / self.rtp_clock_rate as u64;
        Some(ref_time + std::time::Duration::from_micros(elapsed_micros))
    }

    /// Check if clock sync is established
    pub fn is_synced(&self) -> bool {
        self.sample_count >= 1
    }

    /// True after RECORD provides an RTP-Info anchor (enables RTP playout scheduling).
    pub fn has_rtp_anchor(&self) -> bool {
        self.reference_time.is_some()
    }
}

/// AirPlay timing packet structure.
///
/// AirPlay uses a custom timing protocol on the RAOP timing port.
/// Request packets (type 0x52) are 32 bytes:
///   [0..4]   - RTP-like header (V=2, P=0, X=0, CC=0, M=1, PT=82 or 83)
///   [4..8]   - zero padding
///   [8..16]  - reference timestamp (origin time from previous response)
///   [16..24] - receive timestamp
///   [24..32] - send timestamp
///
/// The server responds with type 0x53 (response) filling in timestamps.
#[derive(Debug)]
struct TimingPacket {
    /// The three NTP timestamps
    reference_time: NtpTimestamp,
    receive_time: NtpTimestamp,
    send_time: NtpTimestamp,
}

impl TimingPacket {
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 32 {
            return None;
        }

        // Verify it's an RTP-like packet
        let _version = (data[0] >> 6) & 0x03;
        let payload_type = data[1] & 0x7F;

        if payload_type != TIMING_REQUEST && payload_type != TIMING_RESPONSE {
            debug!(pt = payload_type, "Not a timing packet");
            return None;
        }

        let reference_time = NtpTimestamp::from_bytes(&data[8..16])?;
        let receive_time = NtpTimestamp::from_bytes(&data[16..24])?;
        let send_time = NtpTimestamp::from_bytes(&data[24..32])?;

        Some(TimingPacket {
            reference_time,
            receive_time,
            send_time,
        })
    }

    /// Build a timing request packet.
    fn build_request(
        reference_time: NtpTimestamp,
        receive_time: NtpTimestamp,
        send_time: NtpTimestamp,
    ) -> [u8; 32] {
        let mut pkt = [0u8; 32];
        pkt[0] = 0x80;
        pkt[1] = 0x80 | TIMING_REQUEST; // 0xd2
        pkt[2] = 0x00;
        pkt[3] = 0x07;
        pkt[8..16].copy_from_slice(&reference_time.to_bytes());
        pkt[16..24].copy_from_slice(&receive_time.to_bytes());
        pkt[24..32].copy_from_slice(&send_time.to_bytes());
        pkt
    }

    /// Build a timing response packet.
    fn build_response(
        reference_time: NtpTimestamp,
        receive_time: NtpTimestamp,
        send_time: NtpTimestamp,
    ) -> [u8; 32] {
        let mut pkt = [0u8; 32];

        // RTP header: V=2, P=0, X=0, CC=0 → 0x80
        pkt[0] = 0x80;
        // Marker=1, PT=83 (timing response) → 0xD3
        pkt[1] = 0x80 | TIMING_RESPONSE;
        // Sequence number (not used in timing, set to 7 like Apple)
        pkt[2] = 0x00;
        pkt[3] = 0x07;
        // Bytes [4..8] are zero

        // Reference timestamp (echo back the client's send time)
        pkt[8..16].copy_from_slice(&reference_time.to_bytes());
        // Receive timestamp (when we received the request)
        pkt[16..24].copy_from_slice(&receive_time.to_bytes());
        // Send timestamp (now, when we send the response)
        pkt[24..32].copy_from_slice(&send_time.to_bytes());

        pkt
    }
}

/// Start the NTP timing handler on the specified port.
///
/// This handles AirPlay timing requests from the client. The client sends
/// timing request packets and we respond with our timestamps, allowing
/// the client (and us) to compute clock offset.
pub async fn start_timing_server(
    port: u16,
    clock_sync: Arc<RwLock<ClockSync>>,
    client_timing_addr: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>>,
) -> Result<()> {
    let addr = format!("[::]:{}", port);
    let socket = Arc::new(UdpSocket::bind(&addr).await?);
    info!(port = port, "NTP timing server started");

    // t1 from our last active timing request (must match what the client echoes in response.reference)
    let pending_active_send: Arc<Mutex<Option<NtpTimestamp>>> = Arc::new(Mutex::new(None));
    let pending_active_send_loop = pending_active_send.clone();
    // Chained timing state shared between active sender and response handler
    let timing_chain: Arc<Mutex<(NtpTimestamp, NtpTimestamp)>> =
        Arc::new(Mutex::new((NtpTimestamp::default(), NtpTimestamp::default())));
    let timing_chain_loop = timing_chain.clone();
    let timing_chain_recv = timing_chain.clone();

    // Spawn an active synchronization loop that polls the client every 3 seconds
    let socket_send = socket.clone();
    let client_addr_send = client_timing_addr.clone();
    tokio::spawn(async move {
        loop {
            let target_addr = {
                let guard = client_addr_send.lock().unwrap();
                *guard
            };

            if let Some(addr) = target_addr {
                let (last_origin, last_remote_recv) = *timing_chain_loop.lock().unwrap();
                let now = NtpTimestamp::now();
                let request = TimingPacket::build_request(
                    last_origin,
                    last_remote_recv,
                    now,
                );

                if let Err(e) = socket_send.send_to(&request, addr).await {
                    warn!("Failed to send active timing request to {}: {}", addr, e);
                } else {
                    *pending_active_send_loop.lock().unwrap() = Some(now);
                    debug!("Sent active timing request to {}", addr);
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        }
    });

    let mut buf = [0u8; 256];

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((amt, src)) => {
                if amt < 32 {
                    debug!(bytes = amt, "Timing packet too small, ignoring");
                    continue;
                }

                let payload_type = buf[1] & 0x7F;

                if payload_type == TIMING_REQUEST {
                    if let Some(request) = TimingPacket::parse(&buf[..amt]) {
                        let receive_time = NtpTimestamp::now();
                        let send_time = NtpTimestamp::now();

                        // Build response: reference = client's send_time, our receive + send times
                        let response = TimingPacket::build_response(
                            request.send_time, // echo back as reference
                            receive_time,
                            send_time,
                        );

                        if let Err(e) = socket.send_to(&response, src).await {
                            warn!("Failed to send timing response: {}", e);
                        }

                        // Do not call ClockSync::update here — t4 (when the client receives
                        // our response) is unknown on the server. Only complete active
                        // exchanges (our request → client TIMING_RESPONSE) update offset.
                        *timing_chain_recv.lock().unwrap() =
                            (request.send_time, request.receive_time);

                        debug!(peer = %src, "Timing request answered (passive)");
                    } else {
                        debug!(bytes = amt, "Failed to parse timing request packet");
                    }
                } else if payload_type == TIMING_RESPONSE {
                    if let Some(response) = TimingPacket::parse(&buf[..amt]) {
                        // NTP: t1=our transmit, t2=client receive, t3=client transmit, t4=our receive
                        let t1 = pending_active_send
                            .lock()
                            .unwrap()
                            .take()
                            .unwrap_or(response.reference_time);
                        let t2 = response.receive_time;
                        let t3 = response.send_time;
                        let t4 = NtpTimestamp::now();

                        *timing_chain_recv.lock().unwrap() = (t3, t2);

                        if let Ok(mut sync) = clock_sync.write() {
                            sync.update(t1, t2, t3, t4);
                        }
                    } else {
                        debug!(bytes = amt, "Failed to parse timing response packet");
                    }
                }
            }
            Err(e) => {
                warn!("Timing receive error: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ntp_timestamp_now() {
        let ts = NtpTimestamp::now();
        // Should be well past year 2020 in NTP seconds
        // 2020-01-01 in NTP = 3786825600
        assert!(ts.seconds > 3_786_825_600);
    }

    #[test]
    fn test_ntp_timestamp_roundtrip() {
        let ts = NtpTimestamp {
            seconds: 0xDEADBEEF,
            fraction: 0xCAFEBABE,
        };
        let bytes = ts.to_bytes();
        let parsed = NtpTimestamp::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.seconds, ts.seconds);
        assert_eq!(parsed.fraction, ts.fraction);
    }

    #[test]
    fn test_clock_sync_update() {
        let mut sync = ClockSync::new(44100);
        assert!(!sync.is_synced());

        let t1 = NtpTimestamp { seconds: 100, fraction: 0 };
        let t2 = NtpTimestamp { seconds: 100, fraction: 500_000 };
        let t3 = NtpTimestamp { seconds: 100, fraction: 600_000 };
        let t4 = NtpTimestamp { seconds: 101, fraction: 0 };

        sync.update(t1, t2, t3, t4);
        assert!(sync.is_synced());
        assert_eq!(sync.sample_count, 1);
    }

    #[test]
    fn test_normalize_peer_timing_us() {
        let local = 3_989_225_826_092_610_i64;
        let peer = 2_209_104_133_858_832_i64;
        let norm = normalize_peer_timing_us(local, peer);
        assert!((norm - local).abs() < 10_000);
    }

    #[test]
    fn test_clock_sync_rejects_zero_origin() {
        let mut sync = ClockSync::new(44100);
        sync.update(
            NtpTimestamp::default(),
            NtpTimestamp { seconds: 100, fraction: 0 },
            NtpTimestamp { seconds: 100, fraction: 0 },
            NtpTimestamp { seconds: 100, fraction: 0 },
        );
        assert!(!sync.is_synced());
    }

    #[test]
    fn test_rtp_to_playout_time() {
        let mut sync = ClockSync::new(44100);
        sync.set_rtp_reference(0);

        // 44100 samples = 1 second at 44100Hz
        let playout = sync.rtp_to_playout_time(44100).unwrap();
        let ref_time = sync.reference_time.unwrap();
        let elapsed = playout.duration_since(ref_time);
        // Should be approximately 1 second
        assert!((elapsed.as_millis() as i64 - 1000).abs() < 5);
    }

    #[test]
    fn test_rtp_to_playout_time_behind_reference() {
        let mut sync = ClockSync::new(44100);
        sync.set_rtp_reference(10_000);
        let ref_time = sync.reference_time.unwrap();
        // Timestamp before reference must not schedule far in the future.
        let playout = sync.rtp_to_playout_time(9_000).unwrap();
        assert_eq!(playout, ref_time);
    }

    #[test]
    fn test_apply_sync_packet_anchor() {
        let mut sync = ClockSync::new(44100);
        let now_rtp = 50_000u32;
        let minus_lat = now_rtp.wrapping_sub(AUDIO_LATENCY_SAMPLES);
        sync.apply_sync_packet(minus_lat, now_rtp, NtpTimestamp::default(), true);
        assert_eq!(sync.rtp_reference, minus_lat);
        assert_eq!(sync.sync_count(), 1);
        let playout_now = sync.rtp_to_playout_time(minus_lat).unwrap();
        let playout_future = sync.rtp_to_playout_time(now_rtp).unwrap();
        assert!(playout_future > playout_now);
        let delta = playout_future.duration_since(playout_now);
        assert!((delta.as_millis() as i64 - 250).abs() < 5);
    }

    #[test]
    fn test_record_anchor_includes_latency() {
        let mut sync = ClockSync::new(44100);
        let before = Instant::now();
        sync.set_rtp_reference(0);
        let ref_time = sync.reference_time.unwrap();
        assert!(ref_time >= before + sync.playout_latency() - Duration::from_millis(5));
    }

    #[test]
    fn test_timing_packet_build_response() {
        let ref_time = NtpTimestamp { seconds: 1, fraction: 0 };
        let recv_time = NtpTimestamp { seconds: 2, fraction: 0 };
        let send_time = NtpTimestamp { seconds: 3, fraction: 0 };

        let pkt = TimingPacket::build_response(ref_time, recv_time, send_time);
        assert_eq!(pkt.len(), 32);
        assert_eq!(pkt[0], 0x80); // RTP V=2
        assert_eq!(pkt[1], 0x80 | TIMING_RESPONSE); // M=1, PT=83
    }
}
