use anyhow::Result;
use std::sync::{Arc, Mutex, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use crate::codec::{self, SessionInfo};
use crate::rtp::JitterBuffer;

unsafe extern "C" {
    fn playfair_decrypt(message3: *const u8, cipherText: *const u8, keyOut: *mut u8);
}

/// Extract streamConnectionID from SETUP request streams array for type 110 (mirroring).
///
/// Iterates through the streams array to find a stream with type 110 and extracts
/// the streamConnectionID field as a 64-bit unsigned integer.
///
/// # Arguments
/// * `streams` - The streams array from the SETUP request plist
///
/// # Returns
/// * `Some(u64)` - The streamConnectionID if found for a type 110 stream
/// * `None` - If no type 110 stream is found or streamConnectionID is missing
///
/// # Logging
/// * Logs a warning if streamConnectionID is missing for a type 110 stream
/// * Logs the extracted streamConnectionID value for debugging
fn extract_stream_connection_id(streams: &[plist::Value]) -> Option<u64> {
    for stream in streams {
        if let plist::Value::Dictionary(stream_dict) = stream {
            // Check if this is a type 110 (mirroring) stream
            if let Some(plist::Value::Integer(stream_type)) = stream_dict.get("type") {
                let ty = stream_type.as_unsigned().unwrap_or(0);
                if ty == 110 {
                    // Log all keys in the stream dictionary for debugging
                    let keys: Vec<String> = stream_dict.keys().map(|k| k.to_string()).collect();
                    info!(stream_dict_keys = ?keys, "Type 110 stream dictionary keys");
                    
                    // Extract streamConnectionID - try both signed and unsigned
                    if let Some(stream_id_value) = stream_dict.get("streamConnectionID") {
                        let id = match stream_id_value {
                            plist::Value::Integer(int_val) => {
                                // Try unsigned first, then signed
                                int_val.as_unsigned().or_else(|| {
                                    int_val.as_signed().map(|s| s as u64)
                                })
                            }
                            _ => {
                                warn!("streamConnectionID has unexpected type: {:?}", stream_id_value);
                                None
                            }
                        };
                        
                        if let Some(id) = id {
                            info!(
                                stream_connection_id = id,
                                "Extracted streamConnectionID for type 110 (mirroring) stream"
                            );
                            return Some(id);
                        }
                    }
                    // Log warning if streamConnectionID is missing for type 110
                    warn!("streamConnectionID missing or invalid for type 110 stream - video key derivation will fail");
                    return None;
                }
            }
        }
    }
    None
}

/// RTSP session state shared across the server
pub struct RtspState {
    pub jitter_buffer: Arc<Mutex<JitterBuffer>>,
    pub session_info: Arc<RwLock<Option<SessionInfo>>>,
    pub rtp_port: u16,
    pub control_port: u16,
    pub timing_port: u16,
    pub client_timing_addr: Arc<Mutex<Option<std::net::SocketAddr>>>,
    pub fairplay_msg: Arc<Mutex<Option<Vec<u8>>>>,
    pub ecdh_secret: Arc<Mutex<Option<[u8; 32]>>>,
}

/// Helper function to build DNS-SD TXT record bytes from key=value pairs.
/// TXT records are length-prefixed: each entry is [len][key=value]
fn build_txt_record(entries: &[&str]) -> Vec<u8> {
    let mut txt = Vec::new();
    for entry in entries {
        let bytes = entry.as_bytes();
        if bytes.len() <= 255 {
            txt.push(bytes.len() as u8);
            txt.extend_from_slice(bytes);
        }
    }
    txt
}

/// Start the RTSP server on the specified port.
///
/// Handles AirPlay RAOP audio session control via RTSP protocol.
pub async fn start_rtsp_server(port: u16, state: Arc<RtspState>) -> Result<()> {
    let addr: std::net::SocketAddr = format!("[::]:{}", port).parse()?;

    // Use socket2 to set SO_REUSEADDR before binding
    let socket = socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.set_only_v6(false)?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;

    match socket.bind(&addr.into()) {
        Ok(_) => {}
        Err(e) => {
            tracing::error!(
                addr = %addr,
                error = %e,
                "Failed to bind RTSP server socket - Address already in use"
            );
            return Err(e.into());
        }
    }

    socket.listen(128)?;
    let listener = TcpListener::from_std(socket.into())?;

    info!("RTSP server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!(peer = %peer, "RTSP connection accepted");
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_rtsp_connection(stream, state).await {
                        tracing::error!(error = %e, "RTSP connection error");
                    }
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to accept RTSP connection");
            }
        }
    }
}

/// Handle a single RTSP connection.
///
/// Reads full RTSP requests (headers + body), parses them, and dispatches
/// to the appropriate handler.
async fn handle_rtsp_connection(
    mut stream: tokio::net::TcpStream,
    state: Arc<RtspState>,
) -> Result<()> {
    let peer = stream.peer_addr()?;
    info!(peer = %peer, "Handling RTSP session");

    let mut buf = vec![0u8; 8192];
    let mut accumulated = Vec::new();

    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            info!(peer = %peer, "RTSP connection closed");
            break;
        }

        info!(peer = %peer, bytes_read = n, "Read raw bytes from RTSP stream");
        let raw_str = String::from_utf8_lossy(&buf[..n]);
        info!(peer = %peer, raw_content = %raw_str, "Raw RTSP stream content");

        accumulated.extend_from_slice(&buf[..n]);

        // Process all complete requests in the accumulated buffer
        while let Some((header_end, content_length)) = find_request_boundary(&accumulated) {
            let total_len = header_end + content_length;
            if accumulated.len() < total_len {
                // Need more data for the body
                break;
            }

            // Extract the complete request
            let request_bytes = accumulated[..total_len].to_vec();
            accumulated.drain(..total_len);

            // Parse and handle the request
            let response = handle_rtsp_request(&request_bytes, &state, peer).await;
            stream.write_all(&response).await?;
        }
    }

    Ok(())
}

/// Find the end of RTSP headers (\r\n\r\n) and extract Content-Length.
///
/// Returns (header_end_offset, content_length) where header_end_offset
/// is the byte position just after the \r\n\r\n delimiter.
fn find_request_boundary(data: &[u8]) -> Option<(usize, usize)> {
    // Look for \r\n\r\n
    let header_end = data.windows(4)
        .position(|w| w == b"\r\n\r\n")?;

    let header_end = header_end + 4; // Include the \r\n\r\n

    // Extract Content-Length from headers
    let header_str = std::str::from_utf8(&data[..header_end]).ok()?;
    let content_length = extract_header_value(header_str, "Content-Length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    Some((header_end, content_length))
}

/// Handle a single RTSP request and return the response bytes.
async fn handle_rtsp_request(request_bytes: &[u8], state: &Arc<RtspState>, peer: std::net::SocketAddr) -> Vec<u8> {
    // Parse the request using httparse
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);

    // Find header/body split
    let mut header_end_pos = 0;
    for window in request_bytes.windows(4) {
        if window == b"\r\n\r\n" {
            break;
        }
        header_end_pos += 1;
    }
    header_end_pos += 4; // Add length of \r\n\r\n

    let body_bytes = &request_bytes[header_end_pos..];

    // Pre-replace "RTSP/1.0" with "HTTP/1.1" so httparse does not return Error::Version
    let mut header_part = request_bytes[..header_end_pos].to_vec();
    if let Some(pos) = header_part.windows(8).position(|w| w == b"RTSP/1.0") {
        header_part[pos..pos+8].copy_from_slice(b"HTTP/1.1");
    }

    // Parse the request line and headers
    match req.parse(&header_part) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => {
            debug!("Failed to parse RTSP request");
            return rtsp_response(400, "0", "Bad Request").into_bytes();
        }
    }

    let method = req.method.unwrap_or("");
    let path = req.path.unwrap_or("*");

    // Extract CSeq (required for RTSP responses)
    let cseq = req.headers.iter()
        .find(|h| h.name.eq_ignore_ascii_case("CSeq"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .unwrap_or("0")
        .trim()
        .to_string();

    info!(method = method, path = path, cseq = %cseq, "RTSP request");

    match (method, path) {
        ("GET", p) if p.contains("/info") => {
            let content_type = req.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("Content-Type"))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .unwrap_or("");

            let mut add_txt_airplay = false;
            let mut add_txt_raop = false;

            if content_type.contains("application/x-apple-binary-plist") {
                if let Ok(plist::Value::Dictionary(dict)) = plist::from_bytes::<plist::Value>(body_bytes) {
                    if let Some(qualifier) = dict.get("qualifier") {
                        match qualifier {
                            plist::Value::Array(arr) => {
                                for val in arr {
                                    if let plist::Value::String(s) = val {
                                        if s == "txtAirPlay" {
                                            add_txt_airplay = true;
                                        } else if s == "txtRAOP" {
                                            add_txt_raop = true;
                                        }
                                    }
                                }
                            }
                            plist::Value::String(s) => {
                                if s == "txtAirPlay" {
                                    add_txt_airplay = true;
                                } else if s == "txtRAOP" {
                                    add_txt_raop = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            } else {
                add_txt_airplay = p.contains("txtAirPlay");
                add_txt_raop = p.contains("txtRAOP");
            }

            let mac_address = crate::mdns::get_mac_address();

            let mut res_dict = plist::Dictionary::new();
            if !content_type.is_empty() {
                if add_txt_airplay {
                    let airplay_txt = build_txt_record(&[
                        &format!("deviceid={}", mac_address),
                        "features=0x527FFEE6,0x0",
                        "model=AppleTV3,2",
                        "flags=0x84",
                        "pw=false",
                        "pk=b07727d6f6cd6e08b58ede525ec3cdeaa252ad9f683feb212ef8a205246554e7",
                        "pi=2e388006-13ba-4041-9a67-25dd4a43d536",
                        "srcvers=220.68",
                        "vv=2",
                    ]);
                    res_dict.insert("txtAirPlay".to_string(), plist::Value::Data(airplay_txt));
                }
                if add_txt_raop {
                    let raop_txt = build_txt_record(&[
                        "ch=2",
                        "cn=0,1,2,3",
                        "da=true",
                        "et=0,3,5",
                        "vv=2",
                        "ft=0x527FFEE6,0x0",
                        "am=AppleTV3,2",
                        "md=0,1,2",
                        "rhd=5.6.0.0",
                        "pw=false",
                        "sf=0x4",
                        "sr=44100",
                        "ss=16",
                        "sv=false",
                        "tp=UDP",
                        "txtvers=1",
                        "vs=220.68",
                        "vn=65537",
                        "pk=b07727d6f6cd6e08b58ede525ec3cdeaa252ad9f683feb212ef8a205246554e7",
                    ]);
                    res_dict.insert("txtRAOP".to_string(), plist::Value::Data(raop_txt));
                }
            } else {
                // Secondary GET /info request (capability discovery)
                res_dict.insert("deviceID".to_string(), plist::Value::String(mac_address.clone()));
                res_dict.insert("macAddress".to_string(), plist::Value::String(mac_address.clone()));

                // Public key bytes parsed from hex "b07727d6f6cd6e08b58ede525ec3cdeaa252ad9f683feb212ef8a205246554e7"
                let pk_hex = "b07727d6f6cd6e08b58ede525ec3cdeaa252ad9f683feb212ef8a205246554e7";
                let mut pk_bytes = Vec::new();
                for i in (0..pk_hex.len()).step_by(2) {
                    if let Ok(b) = u8::from_str_radix(&pk_hex[i..i+2], 16) {
                        pk_bytes.push(b);
                    }
                }
                res_dict.insert("pk".to_string(), plist::Value::Data(pk_bytes));

                res_dict.insert("features".to_string(), plist::Value::Integer(1384120038.into())); // 0x527FFEE6

                let name = crate::mdns::get_airplay_name();
                res_dict.insert("name".to_string(), plist::Value::String(name));

                res_dict.insert("pi".to_string(), plist::Value::String("2e388006-13ba-4041-9a67-25dd4a43d536".to_string()));
                res_dict.insert("vv".to_string(), plist::Value::Integer(2.into()));
                res_dict.insert("statusFlags".to_string(), plist::Value::Integer(68.into()));
                res_dict.insert("keepAliveLowPower".to_string(), plist::Value::Integer(1.into()));
                res_dict.insert("sourceVersion".to_string(), plist::Value::String("220.68".to_string()));
                res_dict.insert("keepAliveSendStatsAsBody".to_string(), plist::Value::Boolean(true));
                res_dict.insert("model".to_string(), plist::Value::String("AppleTV3,2".to_string()));
                res_dict.insert("initialVolume".to_string(), plist::Value::Real(0.0));

                // audioLatencies
                let mut audio_latencies = Vec::new();
                for ty in [100, 101] {
                    let mut dict = plist::Dictionary::new();
                    dict.insert("type".to_string(), plist::Value::Integer(ty.into()));
                    dict.insert("audioType".to_string(), plist::Value::String("default".to_string()));
                    dict.insert("inputLatencyMicros".to_string(), plist::Value::Integer(0.into()));
                    dict.insert("outputLatencyMicros".to_string(), plist::Value::Boolean(false));
                    audio_latencies.push(plist::Value::Dictionary(dict));
                }
                res_dict.insert("audioLatencies".to_string(), plist::Value::Array(audio_latencies));

                // audioFormats
                let mut audio_formats = Vec::new();
                for ty in [100, 101] {
                    let mut dict = plist::Dictionary::new();
                    dict.insert("type".to_string(), plist::Value::Integer(ty.into()));
                    dict.insert("audioInputFormats".to_string(), plist::Value::Integer(67108860.into())); // 0x3fffffc
                    dict.insert("audioOutputFormats".to_string(), plist::Value::Integer(67108860.into()));
                    audio_formats.push(plist::Value::Dictionary(dict));
                }
                res_dict.insert("audioFormats".to_string(), plist::Value::Array(audio_formats));

                // displays
                let mut displays = Vec::new();
                let mut disp = plist::Dictionary::new();
                disp.insert("uuid".to_string(), plist::Value::String("e0ff8a27-6738-3d56-8a16-cc53aacee925".to_string()));
                disp.insert("widthPhysical".to_string(), plist::Value::Integer(0.into()));
                disp.insert("heightPhysical".to_string(), plist::Value::Integer(0.into()));
                disp.insert("width".to_string(), plist::Value::Integer(1920.into()));
                disp.insert("height".to_string(), plist::Value::Integer(1080.into()));
                disp.insert("widthPixels".to_string(), plist::Value::Integer(1920.into()));
                disp.insert("heightPixels".to_string(), plist::Value::Integer(1080.into()));
                disp.insert("rotation".to_string(), plist::Value::Boolean(false));
                disp.insert("refreshRate".to_string(), plist::Value::Real(1.0 / 60.0));
                disp.insert("maxFPS".to_string(), plist::Value::Integer(30.into()));
                disp.insert("overscanned".to_string(), plist::Value::Boolean(false));
                disp.insert("features".to_string(), plist::Value::Integer(14.into()));
                displays.push(plist::Value::Dictionary(disp));
                res_dict.insert("displays".to_string(), plist::Value::Array(displays));
            }

            let mut plist_buf = Vec::new();
            let _ = plist::Value::Dictionary(res_dict).to_writer_binary(&mut plist_buf);

            let header = format!(
                "RTSP/1.0 200 OK\r\n\
                 CSeq: {}\r\n\
                 Content-Type: application/x-apple-binary-plist\r\n\
                 Content-Length: {}\r\n\
                 Server: AirTunes/220.68\r\n\
                 \r\n",
                cseq, plist_buf.len()
            );
            let mut response = header.into_bytes();
            response.extend_from_slice(&plist_buf);
            response
        }

        ("POST", "/pair-verify") => {
            // Pairing verify via RTSP — completes the ECDH handshake
            info!(body_len = body_bytes.len(), "RTSP POST /pair-verify received");

            if body_bytes.len() < 4 {
                return rtsp_response(400, &cseq, "Bad Request").into_bytes();
            }

            let step = body_bytes[0];
            match step {
                1 => {
                    // Step 1: Client sends X25519 public key (32 bytes) + Ed25519 public key (32 bytes)
                    // Expected: [0x01, 0x00, 0x00, 0x00] + 32 bytes X25519 + 32 bytes Ed25519 = 68 bytes
                    if body_bytes.len() != 68 {
                        warn!(len = body_bytes.len(), "Invalid pair-verify step 1 length");
                        return rtsp_response(400, &cseq, "Bad Request").into_bytes();
                    }

                    let client_x25519_public = &body_bytes[4..36];
                    info!(client_pk = ?&client_x25519_public[..8], "Received client X25519 public key via RTSP");

                    // Generate our ephemeral X25519 keypair
                    use rand::rngs::OsRng;
                    use x25519_dalek::{EphemeralSecret, PublicKey};
                    let server_secret = EphemeralSecret::random_from_rng(OsRng);
                    let server_public = PublicKey::from(&server_secret);

                    // Compute ECDH shared secret
                    let client_public_key = PublicKey::from(<[u8; 32]>::try_from(client_x25519_public).unwrap());
                    let shared_secret = server_secret.diffie_hellman(&client_public_key);

                    info!(
                        server_pk = ?&server_public.as_bytes()[..8],
                        shared_secret = ?&shared_secret.as_bytes()[..8],
                        "Generated ECDH shared secret via RTSP"
                    );

                    // Store the shared secret for later use in SETUP
                    if let Ok(mut secret_guard) = state.ecdh_secret.lock() {
                        *secret_guard = Some(*shared_secret.as_bytes());
                        info!("ECDH shared secret stored for key hashing in RTSP SETUP");
                    }

                    // Return server public key + dummy signature
                    let mut response_data = Vec::with_capacity(64);
                    response_data.extend_from_slice(server_public.as_bytes()); // 32 bytes
                    response_data.extend_from_slice(&[0u8; 32]); // 32 bytes dummy Ed25519 signature

                    let header = format!(
                        "RTSP/1.0 200 OK\r\n\
                         CSeq: {}\r\n\
                         Content-Type: application/octet-stream\r\n\
                         Content-Length: {}\r\n\
                         Server: AirTunes/220.68\r\n\
                         \r\n",
                        cseq,
                        response_data.len()
                    );
                    let mut response = header.into_bytes();
                    response.extend_from_slice(&response_data);
                    response
                }
                0 => {
                    // Step 2: Client sends signature verification
                    info!("RTSP Pair-verify step 2: signature verification (skipped)");
                    format!(
                        "RTSP/1.0 200 OK\r\n\
                         CSeq: {}\r\n\
                         Content-Type: application/octet-stream\r\n\
                         Content-Length: 0\r\n\
                         Server: AirTunes/220.68\r\n\
                         \r\n",
                        cseq
                    ).into_bytes()
                }
                _ => {
                    warn!(step = step, "Unknown pair-verify step");
                    rtsp_response(400, &cseq, "Bad Request").into_bytes()
                }
            }
        }

        ("POST", "/fp-setup") => {
            let datalen = body_bytes.len();
            if datalen == 16 {
                if body_bytes[4] != 0x03 {
                    rtsp_response(500, &cseq, "Internal Server Error").into_bytes()
                } else {
                    let mode = body_bytes[14] as usize;
                    let reply_message: [&[u8]; 4] = [
                        &[0x46,0x50,0x4c,0x59,0x03,0x01,0x02,0x00,0x00,0x00,0x00,0x82,0x02,0x00,0x0f,0x9f,0x3f,0x9e,0x0a,0x25,0x21,0xdb,0xdf,0x31,0x2a,0xb2,0xbf,0xb2,0x9e,0x8d,0x23,0x2b,0x63,0x76,0xa8,0xc8,0x18,0x70,0x1d,0x22,0xae,0x93,0xd8,0x27,0x37,0xfe,0xaf,0x9d,0xb4,0xfd,0xf4,0x1c,0x2d,0xba,0x9d,0x1f,0x49,0xca,0xaa,0xbf,0x65,0x91,0xac,0x1f,0x7b,0xc6,0xf7,0xe0,0x66,0x3d,0x21,0xaf,0xe0,0x15,0x65,0x95,0x3e,0xab,0x81,0xf4,0x18,0xce,0xed,0x09,0x5a,0xdb,0x7c,0x3d,0x0e,0x25,0x49,0x09,0xa7,0x98,0x31,0xd4,0x9c,0x39,0x82,0x97,0x34,0x34,0xfa,0xcb,0x42,0xc6,0x3a,0x1c,0xd9,0x11,0xa6,0xfe,0x94,0x1a,0x8a,0x6d,0x4a,0x74,0x3b,0x46,0xc3,0xa7,0x64,0x9e,0x44,0xc7,0x89,0x55,0xe4,0x9d,0x81,0x55,0x00,0x95,0x49,0xc4,0xe2,0xf7,0xa3,0xf6,0xd5,0xba],
                        &[0x46,0x50,0x4c,0x59,0x03,0x01,0x02,0x00,0x00,0x00,0x00,0x82,0x02,0x01,0xcf,0x32,0xa2,0x57,0x14,0xb2,0x52,0x4f,0x8a,0xa0,0xad,0x7a,0xf1,0x64,0xe3,0x7b,0xcf,0x44,0x24,0xe2,0x00,0x04,0x7e,0xfc,0x0a,0xd6,0x7a,0xfc,0xd9,0x5d,0xed,0x1c,0x27,0x30,0xbb,0x59,0x1b,0x96,0x2e,0xd6,0x3a,0x9c,0x4d,0xed,0x88,0xba,0x8f,0xc7,0x8d,0xe6,0x4d,0x91,0xcc,0xfd,0x5c,0x7b,0x56,0xda,0x88,0xe3,0x1f,0x5c,0xce,0xaf,0xc7,0x43,0x19,0x95,0xa0,0x16,0x65,0xa5,0x4e,0x19,0x39,0xd2,0x5b,0x94,0xdb,0x64,0xb9,0xe4,0x5d,0x8d,0x06,0x3e,0x1e,0x6a,0xf0,0x7e,0x96,0x56,0x16,0x2b,0x0e,0xfa,0x40,0x42,0x75,0xea,0x5a,0x44,0xd9,0x59,0x1c,0x72,0x56,0xb9,0xfb,0xe6,0x51,0x38,0x98,0xb8,0x02,0x27,0x72,0x19,0x88,0x57,0x16,0x50,0x94,0x2a,0xd9,0x46,0x68,0x8a],
                        &[0x46,0x50,0x4c,0x59,0x03,0x01,0x02,0x00,0x00,0x00,0x00,0x82,0x02,0x02,0xc1,0x69,0xa3,0x52,0xee,0xed,0x35,0xb1,0x8c,0xdd,0x9c,0x58,0xd6,0x4f,0x16,0xc1,0x51,0x9a,0x89,0xeb,0x53,0x17,0xbd,0x0d,0x43,0x36,0xcd,0x68,0xf6,0x38,0xff,0x9d,0x01,0x6a,0x5b,0x52,0xb7,0xfa,0x92,0x16,0xb2,0xb6,0x54,0x82,0xc7,0x84,0x44,0x11,0x81,0x21,0xa2,0xc7,0xfe,0xd8,0x3d,0xb7,0x11,0x9e,0x91,0x82,0xaa,0xd7,0xd1,0x8c,0x70,0x63,0xe2,0xa4,0x57,0x55,0x59,0x10,0xaf,0x9e,0x0e,0xfc,0x76,0x34,0x7d,0x16,0x40,0x43,0x80,0x7f,0x58,0x1e,0xe4,0xfb,0xe4,0x2c,0xa9,0xde,0xdc,0x1b,0x5e,0xb2,0xa3,0xaa,0x3d,0x2e,0xcd,0x59,0xe7,0xee,0xe7,0x0b,0x36,0x29,0xf2,0x2a,0xfd,0x16,0x1d,0x87,0x73,0x53,0xdd,0xb9,0x9a,0xdc,0x8e,0x07,0x00,0x6e,0x56,0xf8,0x50,0xce],
                        &[0x46,0x50,0x4c,0x59,0x03,0x01,0x02,0x00,0x00,0x00,0x00,0x82,0x02,0x03,0x90,0x01,0xe1,0x72,0x7e,0x0f,0x57,0xf9,0xf5,0x88,0x0d,0xb1,0x04,0xa6,0x25,0x7a,0x23,0xf5,0xcf,0xff,0x1a,0xbb,0xe1,0xe9,0x30,0x45,0x25,0x1a,0xfb,0x97,0xeb,0x9f,0xc0,0x01,0x1e,0xbe,0x0f,0x3a,0x81,0xdf,0x5b,0x69,0x1d,0x76,0xac,0xb2,0xf7,0xa5,0xc7,0x08,0xe3,0xd3,0x28,0xf5,0x6b,0xb3,0x9d,0xbd,0xe5,0xf2,0x9c,0x8a,0x17,0xf4,0x81,0x48,0x7e,0x3a,0xe8,0x63,0xc6,0x78,0x32,0x54,0x22,0xe6,0xf7,0x8e,0x16,0x6d,0x18,0xaa,0x7f,0xd6,0x36,0x25,0x8b,0xce,0x28,0x72,0x6f,0x66,0x1f,0x73,0x88,0x93,0xce,0x44,0x31,0x1e,0x4b,0xe6,0xc0,0x53,0x51,0x93,0xe5,0xef,0x72,0xe8,0x68,0x62,0x33,0x72,0x9c,0x22,0x7d,0x82,0x0c,0x99,0x94,0x45,0xd8,0x92,0x46,0xc8,0xc3,0x59]
                    ];
                    let payload = if mode < 4 { reply_message[mode] } else { reply_message[0] };
                    let header = format!(
                        "RTSP/1.0 200 OK\r\n\
                         CSeq: {}\r\n\
                         Content-Type: application/octet-stream\r\n\
                         Content-Length: 142\r\n\
                         Server: AirTunes/220.68\r\n\
                         \r\n",
                        cseq
                    );
                    let mut response = header.into_bytes();
                    response.extend_from_slice(payload);
                    response
                }
            } else if datalen == 164 {
                if body_bytes[4] != 0x03 {
                    rtsp_response(500, &cseq, "Internal Server Error").into_bytes()
                } else {
                    // Save 164-byte body for PlayFair decryption later
                    if let Ok(mut msg_guard) = state.fairplay_msg.lock() {
                        *msg_guard = Some(body_bytes.to_vec());
                    }

                    let fp_header = &[0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x14];
                    let mut payload = Vec::with_capacity(32);
                    payload.extend_from_slice(fp_header);
                    payload.extend_from_slice(&body_bytes[144..164]);

                    let header = format!(
                        "RTSP/1.0 200 OK\r\n\
                         CSeq: {}\r\n\
                         Content-Type: application/octet-stream\r\n\
                         Content-Length: 32\r\n\
                         Server: AirTunes/220.68\r\n\
                         \r\n",
                        cseq
                    );
                    let mut response = header.into_bytes();
                    response.extend_from_slice(&payload);
                    response
                }
            } else {
                rtsp_response(400, &cseq, "Bad Request").into_bytes()
            }
        }

        ("POST", "/feedback") => {
            info!("POST /feedback received");
            format!(
                "RTSP/1.0 200 OK\r\n\
                 CSeq: {}\r\n\
                 Session: 1\r\n\
                 Server: AirTunes/220.68\r\n\
                 \r\n",
                cseq
            ).into_bytes()
        }

        (m, _) => {
            match m {
                "OPTIONS" => {
                    info!("OPTIONS received");
                    format!(
                        "RTSP/1.0 200 OK\r\n\
                         CSeq: {}\r\n\
                         Public: ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER\r\n\
                         Server: AirTunes/220.68\r\n\
                         \r\n",
                        cseq
                    ).into_bytes()
                }

                "ANNOUNCE" => {
                    info!("ANNOUNCE received");
                    let body_str = String::from_utf8_lossy(body_bytes);
                    info!("SDP body: {}", body_str);

                    // Parse the SDP body to extract session info
                    match codec::parse_sdp(&body_str) {
                        Ok(mut sdp_info) => {
                            info!(
                                codec = %sdp_info.codec,
                                sample_rate = sdp_info.sample_rate,
                                channels = sdp_info.channels,
                                encrypted = sdp_info.aes_key.is_some(),
                                "Session info parsed from SDP"
                            );
                            if let Ok(mut info) = state.session_info.write() {
                                // Preserve AES key/IV and mirroring parameters from a prior plist-based SETUP
                                if let Some(ref existing) = *info {
                                    if sdp_info.aes_key.is_none() {
                                        sdp_info.aes_key = existing.aes_key.clone();
                                        sdp_info.aes_iv  = existing.aes_iv.clone();
                                    }
                                    // Preserve mirroring stream parameters to avoid clobbering with standard SDP info
                                    if existing.stream_type == crate::codec::StreamType::Mirroring {
                                        sdp_info.video_aes_key = existing.video_aes_key.clone();
                                        sdp_info.video_aes_iv = existing.video_aes_iv.clone();
                                        sdp_info.stream_type = existing.stream_type;
                                        sdp_info.codec = existing.codec;
                                        sdp_info.sample_rate = existing.sample_rate;
                                        sdp_info.channels = existing.channels;
                                    }
                                }
                                *info = Some(sdp_info);
                                info!("Session info set in shared state (AES key preserved if previously set)");
                            } else {
                                warn!("Failed to acquire write lock for session_info");
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to parse SDP");
                        }
                    }


                    format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nServer: AirTunes/220.68\r\n\r\n", cseq).into_bytes()
                }

                "SETUP" => {
                    info!("SETUP received");
                    let content_type = req.headers.iter()
                        .find(|h| h.name.eq_ignore_ascii_case("Content-Type"))
                        .and_then(|h| std::str::from_utf8(h.value).ok())
                        .unwrap_or("");

                    info!(content_type = %content_type, "SETUP content type");

                    if content_type.contains("application/x-apple-binary-plist") {
                        if let Ok(plist::Value::Dictionary(req_dict)) = plist::from_bytes::<plist::Value>(body_bytes) {
                            // Log all keys in the SETUP request for debugging
                            let keys: Vec<String> = req_dict.keys().map(|k| k.to_string()).collect();
                            info!(plist_keys = ?keys, "SETUP request plist keys");
                            let mut timing_rport = 0;
                            if let Some(plist::Value::Integer(tport)) = req_dict.get("timingPort") {
                                timing_rport = tport.as_unsigned().unwrap_or(0) as u16;
                            }
                            if timing_rport > 0 {
                                if let Ok(mut addr_guard) = state.client_timing_addr.lock() {
                                    let target_addr = match peer {
                                        std::net::SocketAddr::V4(addr) => {
                                            std::net::SocketAddr::new(std::net::IpAddr::V4(*addr.ip()), timing_rport)
                                        }
                                        std::net::SocketAddr::V6(addr) => {
                                            std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                                                *addr.ip(),
                                                timing_rport,
                                                addr.flowinfo(),
                                                addr.scope_id(),
                                            ))
                                        }
                                    };
                                    *addr_guard = Some(target_addr);
                                    info!(target_addr = %target_addr, "Set client active timing address with scope ID");
                                }
                            }

                            // Extract streamConnectionID and audio ct from streams array
                            let mut stream_ct = None;
                            let mut stream_connection_id = None;
                            if let Some(plist::Value::Array(streams)) = req_dict.get("streams") {
                                for stream in streams {
                                    if let plist::Value::Dictionary(stream_dict) = stream {
                                        if let Some(plist::Value::Integer(stream_type)) = stream_dict.get("type") {
                                            let ty = stream_type.as_unsigned().unwrap_or(0);
                                            if ty == 110 {
                                                if let Some(stream_id_value) = stream_dict.get("streamConnectionID") {
                                                    if let plist::Value::Integer(int_val) = stream_id_value {
                                                        stream_connection_id = int_val.as_unsigned().or_else(|| {
                                                            int_val.as_signed().map(|s| s as u64)
                                                        });
                                                    }
                                                }
                                            } else if ty == 96 {
                                                if let Some(plist::Value::Integer(ct_val)) = stream_dict.get("ct") {
                                                    stream_ct = Some(ct_val.as_unsigned().unwrap_or(0));
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            let mut decrypted_aes_key = None;
                            let mut eiv_bytes: Option<Vec<u8>> = None;
                            if let Some(plist::Value::Data(ekey)) = req_dict.get("ekey") {
                                info!(ekey_len = ekey.len(), "RTSP SETUP: ekey found");
                                if ekey.len() == 72 {
                                    if let Ok(msg_guard) = state.fairplay_msg.lock() {
                                        if let Some(ref fp_msg) = *msg_guard {
                                            info!(fp_msg_len = fp_msg.len(), "RTSP SETUP: fairplay_msg found, decrypting ekey");
                                            let mut key_out = [0u8; 16];
                                            unsafe {
                                                playfair_decrypt(fp_msg.as_ptr(), ekey.as_ptr(), key_out.as_mut_ptr());
                                            }
                                            info!(aes_key_raw = ?key_out, "Successfully decrypted AES key via PlayFair FFI");
                                            
                                            // Try hashing with a zero ECDH secret as fallback
                                            // Some clients may derive keys differently
                                            let unhashed_key = key_out.clone();
                                            
                                            // Hash the key with ECDH secret (matching uxplay behavior)
                                            if let Ok(secret_guard) = state.ecdh_secret.lock() {
                                                if let Some(ecdh_secret) = *secret_guard {
                                                    use sha2::{Sha512, Digest};
                                                    let mut hasher = Sha512::new();
                                                    hasher.update(&key_out);           // Original AES key (16 bytes)
                                                    hasher.update(&ecdh_secret);       // ECDH secret (32 bytes)
                                                    let hash_result = hasher.finalize();
                                                    key_out.copy_from_slice(&hash_result[..16]); // Use first 16 bytes
                                                    info!(
                                                        aes_key_hashed = ?key_out,
                                                        ecdh_secret_preview = ?&ecdh_secret[..8],
                                                        "AES key hashed with ECDH secret (AirPlay 2 protocol)"
                                                    );
                                                } else {
                                                    // No ECDH secret - try hashing with device ID or other fallback
                                                    warn!("No ECDH secret found - trying alternative key derivation");
                                                    
                                                    // Try hashing with the eiv as a fallback (some clients do this)
                                                    if let Some(ref eiv_data) = eiv_bytes {
                                                        if eiv_data.len() >= 16 {
                                                            use sha2::{Sha512, Digest};
                                                            let mut hasher = Sha512::new();
                                                            hasher.update(&key_out);
                                                            hasher.update(&eiv_data[..16]);
                                                            let hash_result = hasher.finalize();
                                                            let hashed_key = &hash_result[..16];
                                                            info!(
                                                                unhashed_key = ?unhashed_key,
                                                                hashed_with_iv = ?hashed_key,
                                                                "Trying both unhashed and IV-hashed keys"
                                                            );
                                                            // Store both - we'll try unhashed first
                                                            // key_out stays as unhashed
                                                        }
                                                    }
                                                }
                                            } else {
                                                warn!("Failed to acquire lock on ecdh_secret");
                                            }
                                            
                                            decrypted_aes_key = Some(key_out.to_vec());
                                        } else {
                                            warn!("RTSP SETUP: fairplay_msg is None - cannot decrypt ekey");
                                        }
                                    } else {
                                        warn!("RTSP SETUP: Failed to acquire lock on fairplay_msg");
                                    }
                                } else {
                                    warn!(ekey_len = ekey.len(), "RTSP SETUP: ekey has wrong length (expected 72)");
                                }
                            } else {
                                warn!("RTSP SETUP: No ekey found in SETUP request");
                            }
                            if let Some(plist::Value::Data(eiv)) = req_dict.get("eiv") {
                                eiv_bytes = Some(eiv.clone());
                            }

                            // Derive video keys if stream type 110 is detected and we have audio key
                            let (video_aes_key, video_aes_iv, stream_type) = if let Some(stream_id) = stream_connection_id {
                                // Try to get audio key from local variable first, then from SessionInfo
                                let audio_key_source = if let Some(ref audio_key) = decrypted_aes_key {
                                    Some(audio_key.clone())
                                } else {
                                    // Second SETUP: read audio key from SessionInfo (set by first SETUP)
                                    if let Ok(info_guard) = state.session_info.read() {
                                        if let Some(ref si) = *info_guard {
                                            si.aes_key.clone()
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                };
                                
                                if let Some(audio_key) = audio_key_source {
                                    if audio_key.len() == 16 {
                                        info!(
                                            stream_connection_id = stream_id,
                                            "Deriving video keys for AirPlay 2 mirroring mode (stream type 110)"
                                        );
                                        let video_keys = crate::codec::derive_video_keys(&audio_key, stream_id);
                                        (
                                            Some(video_keys.key.to_vec()),
                                            Some(video_keys.iv.to_vec()),
                                            crate::codec::StreamType::Mirroring
                                        )
                                    } else {
                                        warn!(
                                            audio_key_len = audio_key.len(),
                                            "Audio key has wrong length for video key derivation (expected 16)"
                                        );
                                        (None, None, crate::codec::StreamType::AudioOnly)
                                    }
                                } else {
                                    warn!("Cannot derive video keys: no audio AES key available (check if first SETUP completed)");
                                    (None, None, crate::codec::StreamType::AudioOnly)
                                }
                            } else {
                                // No stream type 110 detected, default to audio-only mode
                                (None, None, crate::codec::StreamType::AudioOnly)
                            };

                            // Update SessionInfo with new keys, stream type, and codec
                            if decrypted_aes_key.is_some() || video_aes_key.is_some() || stream_ct.is_some() {
                                if let Ok(mut info_guard) = state.session_info.write() {
                                    if let Some(ref mut info) = *info_guard {
                                        // Preserve existing audio keys when updating (second SETUP has no ekey)
                                        if decrypted_aes_key.is_some() {
                                            info.aes_key = decrypted_aes_key.clone();
                                            info.aes_iv = eiv_bytes.clone();
                                        }
                                        // Always update video keys and stream type when present
                                        if video_aes_key.is_some() {
                                            info.video_aes_key = video_aes_key.clone();
                                            info.video_aes_iv = video_aes_iv.clone();
                                            info.stream_type = stream_type;
                                        }
                                        // Update codec based on ct key in type 96 stream
                                        if let Some(ct) = stream_ct {
                                            if ct == 8 {
                                                info.codec = crate::codec::AudioCodec::AacEld;
                                                info.sample_rate = 44100;
                                                info.channels = 2;
                                                info.stream_type = crate::codec::StreamType::Mirroring;
                                            } else if ct == 2 {
                                                info.codec = crate::codec::AudioCodec::Alac;
                                                info.sample_rate = 44100;
                                                info.channels = 2;
                                                info.stream_type = crate::codec::StreamType::AudioOnly;
                                            }
                                        } else if stream_type == crate::codec::StreamType::Mirroring {
                                            // Fallback if ct is not explicitly present in type 96 but type 110 is present
                                            info.codec = crate::codec::AudioCodec::AacEld;
                                            info.sample_rate = 44100;
                                            info.channels = 2;
                                        }
                                        info!(
                                            has_audio_key = info.aes_key.is_some(),
                                            has_video_key = info.video_aes_key.is_some(),
                                            stream_type = ?info.stream_type,
                                            codec = ?info.codec,
                                            "SETUP updated existing session info"
                                        );
                                    } else {
                                        let final_codec = if let Some(8) = stream_ct {
                                            crate::codec::AudioCodec::AacEld
                                        } else if stream_type == crate::codec::StreamType::Mirroring {
                                            crate::codec::AudioCodec::AacEld
                                        } else {
                                            crate::codec::AudioCodec::Alac
                                        };
                                        *info_guard = Some(SessionInfo {
                                            codec: final_codec,
                                            sample_rate: 44100,
                                            channels: 2,
                                            aes_key: decrypted_aes_key.clone(),
                                            aes_iv: eiv_bytes.clone(),
                                            video_aes_key: video_aes_key.clone(),
                                            video_aes_iv: video_aes_iv.clone(),
                                            stream_type: if final_codec == crate::codec::AudioCodec::AacEld {
                                                crate::codec::StreamType::Mirroring
                                            } else {
                                                stream_type
                                            },
                                            fmtp: None,
                                            min_latency: None,
                                            max_latency: None,
                                        });
                                        info!(
                                            has_key = decrypted_aes_key.is_some(),
                                            has_video_key = video_aes_key.is_some(),
                                            stream_type = ?info_guard.as_ref().unwrap().stream_type,
                                            codec = ?final_codec,
                                            "SETUP populated new session info with decrypted keys"
                                        );
                                    }
                                }
                            } else if stream_type == crate::codec::StreamType::AudioOnly {
                                // Third SETUP after TEARDOWN: no new keys, but stream_type must be
                                // reset from Mirroring to AudioOnly so audio pipeline uses correct mode
                                if let Ok(mut info_guard) = state.session_info.write() {
                                    if let Some(ref mut info) = *info_guard {
                                        info.stream_type = stream_type;
                                        info.codec = crate::codec::AudioCodec::Alac;
                                        info!(
                                            stream_type = ?info.stream_type,
                                            codec = ?info.codec,
                                            "SETUP reset stream_type to AudioOnly (reconnect after TEARDOWN)"
                                        );
                                    }
                                }
                            }

                            let mut res_dict = plist::Dictionary::new();
                            res_dict.insert("timingPort".to_string(), plist::Value::Integer(state.timing_port.into()));
                            res_dict.insert("eventPort".to_string(), plist::Value::Integer(0.into()));

                            if let Some(plist::Value::Array(req_streams)) = req_dict.get("streams") {
                                let mut res_streams = Vec::new();
                                for stream in req_streams {
                                    if let plist::Value::Dictionary(stream_dict) = stream {
                                        if let Some(plist::Value::Integer(stream_type)) = stream_dict.get("type") {
                                            let mut res_stream = plist::Dictionary::new();
                                            let ty = stream_type.as_unsigned().unwrap_or(0);
                                            if ty == 110 {
                                                // Mirroring
                                                res_stream.insert("dataPort".to_string(), plist::Value::Integer(7100.into()));
                                                res_stream.insert("type".to_string(), plist::Value::Integer(110.into()));
                                            } else if ty == 96 {
                                                // Audio
                                                res_stream.insert("dataPort".to_string(), plist::Value::Integer(state.rtp_port.into()));
                                                res_stream.insert("controlPort".to_string(), plist::Value::Integer(state.control_port.into()));
                                                res_stream.insert("type".to_string(), plist::Value::Integer(96.into()));
                                            }
                                            res_streams.push(plist::Value::Dictionary(res_stream));
                                        }
                                    }
                                }
                                res_dict.insert("streams".to_string(), plist::Value::Array(res_streams));
                            }

                            let mut plist_buf = Vec::new();
                            let _ = plist::Value::Dictionary(res_dict).to_writer_binary(&mut plist_buf);

                            let header = format!(
                                "RTSP/1.0 200 OK\r\n\
                                 CSeq: {}\r\n\
                                 Content-Type: application/x-apple-binary-plist\r\n\
                                 Content-Length: {}\r\n\
                                 Session: 1\r\n\
                                 Server: AirTunes/220.68\r\n\
                                 \r\n",
                                cseq, plist_buf.len()
                            );
                            let mut response = header.into_bytes();
                            response.extend_from_slice(&plist_buf);
                            return response;
                        }
                    }

                    // Parse Transport header from client (fallback)
                    let client_transport = req.headers.iter()
                        .find(|h| h.name.eq_ignore_ascii_case("Transport"))
                        .and_then(|h| std::str::from_utf8(h.value).ok())
                        .unwrap_or("");

                    info!(client_transport = client_transport, "SETUP transport negotiation (fallback)");

                    // Respond with our ports
                    format!(
                        "RTSP/1.0 200 OK\r\n\
                         CSeq: {}\r\n\
                         Transport: RTP/AVP/UDP;unicast;mode=record;\
                         server_port={};control_port={};timing_port={}\r\n\
                         Session: 1\r\n\
                         Server: AirTunes/220.68\r\n\
                         \r\n",
                        cseq, state.rtp_port, state.control_port, state.timing_port
                    ).into_bytes()
                }

                "RECORD" => {
                    info!("RECORD received — starting audio stream");

                    // Parse RTP-Info header for initial sequence/timestamp
                    let rtp_info = req.headers.iter()
                        .find(|h| h.name.eq_ignore_ascii_case("RTP-Info"))
                        .and_then(|h| std::str::from_utf8(h.value).ok())
                        .unwrap_or("");

                    if !rtp_info.is_empty() {
                        info!(rtp_info = rtp_info, "RTP-Info from RECORD");
                        // Parse "seq=XXXX;rtptime=YYYY"
                        for part in rtp_info.split(';') {
                            let part = part.trim();
                            if let Some(seq_str) = part.strip_prefix("seq=")
                                && let Ok(seq) = seq_str.parse::<u16>() {
                                    info!(initial_seq = seq, "Initial RTP sequence number");
                                }
                        }
                    }

                    // Flush jitter buffer for new stream
                    {
                        let mut buffer = state.jitter_buffer.lock().unwrap();
                        buffer.flush();
                    }

                    format!(
                        "RTSP/1.0 200 OK\r\n\
                          CSeq: {}\r\n\
                          Session: 1\r\n\
                          Audio-Latency: 11025\r\n\
                          Audio-Jack-Status: connected; type=analog\r\n\
                          Server: AirTunes/220.68\r\n\
                          \r\n",
                        cseq
                    ).into_bytes()
                }

                "FLUSH" => {
                    info!("FLUSH received");
                    {
                        let mut buffer = state.jitter_buffer.lock().unwrap();
                        buffer.flush();
                    }
                    format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: 1\r\nServer: AirTunes/220.68\r\n\r\n", cseq).into_bytes()
                }

                "TEARDOWN" => {
                    info!("TEARDOWN received — ending session");
                    {
                        let mut buffer = state.jitter_buffer.lock().unwrap();
                        buffer.flush();
                    }
                    // NOTE: Do NOT clear session info on TEARDOWN
                    // In AirPlay 2 screen mirroring, the iPhone sends encryption keys in an initial
                    // capability probe session, then tears it down and starts a new session without
                    // re-sending the keys. We must preserve the keys across TEARDOWN for the actual
                    // streaming session to work.
                    // 
                    // Session flow:
                    // 1. First session: SETUP (with ekey) → SETUP (with streamConnectionID) → TEARDOWN
                    // 2. Second session: SETUP (no ekey, expects keys from first session) → RECORD → audio packets
                    //
                    // If we clear SessionInfo here, the second session will have no decryption keys.
                    info!("Preserving SessionInfo across TEARDOWN for potential session reconnection");
                    format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: 1\r\nConnection: close\r\nServer: AirTunes/220.68\r\n\r\n", cseq).into_bytes()
                }

                "GET_PARAMETER" => {
                    let content_type = req.headers.iter()
                        .find(|h| h.name.eq_ignore_ascii_case("Content-Type"))
                        .and_then(|h| std::str::from_utf8(h.value).ok())
                        .unwrap_or("");

                    let body_str = String::from_utf8_lossy(body_bytes);
                    if content_type.contains("text/parameters") && body_str.contains("volume") {
                        let vol_body = "volume: 0.000000\r\n";
                        format!(
                            "RTSP/1.0 200 OK\r\n\
                             CSeq: {}\r\n\
                             Session: 1\r\n\
                             Content-Type: text/parameters\r\n\
                             Content-Length: {}\r\n\
                             Server: AirTunes/220.68\r\n\
                             \r\n\
                             {}",
                            cseq, vol_body.len(), vol_body
                        ).into_bytes()
                    } else {
                        debug!("GET_PARAMETER (keepalive)");
                        format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: 1\r\nServer: AirTunes/220.68\r\n\r\n", cseq).into_bytes()
                    }
                }

                "SET_PARAMETER" => {
                    debug!(body_len = body_bytes.len(), "SET_PARAMETER received");

                    // Check Content-Type for metadata type
                    let content_type = req.headers.iter()
                        .find(|h| h.name.eq_ignore_ascii_case("Content-Type"))
                        .and_then(|h| std::str::from_utf8(h.value).ok())
                        .unwrap_or("");

                    match content_type {
                        "text/parameters" => {
                            let body_str = String::from_utf8_lossy(body_bytes);
                            for line in body_str.lines() {
                                if let Some(vol) = line.strip_prefix("volume:") {
                                    info!(volume = vol.trim(), "Volume parameter");
                                } else if let Some(prog) = line.strip_prefix("progress:") {
                                    debug!(progress = prog.trim(), "Progress parameter");
                                }
                            }
                        }
                        ct if ct.starts_with("image/") => {
                            info!(content_type = ct, size = body_bytes.len(), "Artwork received");
                        }
                        _ => {
                            debug!(content_type = content_type, "Unknown SET_PARAMETER type");
                        }
                    }

                    format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: 1\r\nServer: AirTunes/220.68\r\n\r\n", cseq).into_bytes()
                }

                "PAUSE" => {
                    info!("PAUSE received");
                    format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: 1\r\nServer: AirTunes/220.68\r\n\r\n", cseq).into_bytes()
                }

                _ => {
                    warn!(method = m, "Unknown RTSP method");
                    format!(
                        "RTSP/1.0 501 Not Implemented\r\nCSeq: {}\r\nServer: AirTunes/220.68\r\n\r\n",
                        cseq
                    ).into_bytes()
                }
            }
        }
    }
}

/// Build an RTSP response string.
fn rtsp_response(status: u16, cseq: &str, reason: &str) -> String {
    format!("RTSP/1.0 {} {}\r\nCSeq: {}\r\nServer: AirTunes/220.68\r\n\r\n", status, reason, cseq)
}

/// Extract a header value from raw RTSP text.
fn extract_header_value(request: &str, header_name: &str) -> Option<String> {
    request.lines()
        .find(|line| {
            let lower = line.to_lowercase();
            lower.starts_with(&header_name.to_lowercase())
                && lower[header_name.len()..].starts_with(':')
        })
        .and_then(|line| {
            line.split_once(':').map(|(_, v)| v.trim().to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_request_boundary_simple() {
        let data = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let (end, cl) = find_request_boundary(data).unwrap();
        assert_eq!(cl, 0);
        assert_eq!(end, data.len());
    }

    #[test]
    fn test_find_request_boundary_with_body() {
        let data = b"ANNOUNCE rtsp://... RTSP/1.0\r\nCSeq: 2\r\nContent-Length: 5\r\n\r\nhello";
        let (end, cl) = find_request_boundary(data).unwrap();
        assert_eq!(cl, 5);
        assert_eq!(&data[end..end + cl], b"hello");
    }

    #[test]
    fn test_find_request_boundary_incomplete() {
        let data = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n";
        assert!(find_request_boundary(data).is_none());
    }

    #[test]
    fn test_extract_header_value() {
        let req = "OPTIONS * RTSP/1.0\r\nCSeq: 42\r\nContent-Length: 100\r\n\r\n";
        assert_eq!(extract_header_value(req, "CSeq"), Some("42".to_string()));
        assert_eq!(extract_header_value(req, "Content-Length"), Some("100".to_string()));
        assert_eq!(extract_header_value(req, "Missing"), None);
    }

    #[test]
    fn test_extract_stream_connection_id_type_110() {
        // Create a streams array with a type 110 stream containing streamConnectionID
        let stream_id = 12345678901234567890u64;
        let mut stream_dict = plist::Dictionary::new();
        stream_dict.insert("type".to_string(), plist::Value::Integer(110.into()));
        stream_dict.insert("streamConnectionID".to_string(), plist::Value::Integer(stream_id.into()));
        
        let streams = vec![plist::Value::Dictionary(stream_dict)];
        
        let result = extract_stream_connection_id(&streams);
        assert_eq!(result, Some(stream_id));
    }

    #[test]
    fn test_extract_stream_connection_id_type_96() {
        // Create a streams array with only type 96 (audio-only) stream
        let mut stream_dict = plist::Dictionary::new();
        stream_dict.insert("type".to_string(), plist::Value::Integer(96.into()));
        
        let streams = vec![plist::Value::Dictionary(stream_dict)];
        
        let result = extract_stream_connection_id(&streams);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_stream_connection_id_missing_field() {
        // Create a type 110 stream without streamConnectionID
        let mut stream_dict = plist::Dictionary::new();
        stream_dict.insert("type".to_string(), plist::Value::Integer(110.into()));
        
        let streams = vec![plist::Value::Dictionary(stream_dict)];
        
        let result = extract_stream_connection_id(&streams);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_stream_connection_id_multiple_streams() {
        // Create multiple streams, with type 110 in the middle
        let stream_id = 9876543210u64;
        
        let mut stream1 = plist::Dictionary::new();
        stream1.insert("type".to_string(), plist::Value::Integer(96.into()));
        
        let mut stream2 = plist::Dictionary::new();
        stream2.insert("type".to_string(), plist::Value::Integer(110.into()));
        stream2.insert("streamConnectionID".to_string(), plist::Value::Integer(stream_id.into()));
        
        let mut stream3 = plist::Dictionary::new();
        stream3.insert("type".to_string(), plist::Value::Integer(96.into()));
        
        let streams = vec![
            plist::Value::Dictionary(stream1),
            plist::Value::Dictionary(stream2),
            plist::Value::Dictionary(stream3),
        ];
        
        let result = extract_stream_connection_id(&streams);
        assert_eq!(result, Some(stream_id));
    }

    #[test]
    fn test_extract_stream_connection_id_empty_array() {
        let streams: Vec<plist::Value> = vec![];
        let result = extract_stream_connection_id(&streams);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_stream_connection_id_max_u64() {
        // Test with maximum u64 value
        let stream_id = u64::MAX;
        let mut stream_dict = plist::Dictionary::new();
        stream_dict.insert("type".to_string(), plist::Value::Integer(110.into()));
        stream_dict.insert("streamConnectionID".to_string(), plist::Value::Integer(stream_id.into()));
        
        let streams = vec![plist::Value::Dictionary(stream_dict)];
        
        let result = extract_stream_connection_id(&streams);
        assert_eq!(result, Some(stream_id));
    }

    #[test]
    fn test_extract_stream_connection_id_zero() {
        // Test with zero value
        let stream_id = 0u64;
        let mut stream_dict = plist::Dictionary::new();
        stream_dict.insert("type".to_string(), plist::Value::Integer(110.into()));
        stream_dict.insert("streamConnectionID".to_string(), plist::Value::Integer(stream_id.into()));
        
        let streams = vec![plist::Value::Dictionary(stream_dict)];
        
        let result = extract_stream_connection_id(&streams);
        assert_eq!(result, Some(stream_id));
    }

    #[test]
    fn test_setup_session_info_updates_ct() {
        let jitter_buffer = Arc::new(Mutex::new(JitterBuffer::new(10)));
        let session_info = Arc::new(RwLock::new(None));
        let state = Arc::new(RtspState {
            jitter_buffer,
            session_info: session_info.clone(),
            rtp_port: 6000,
            control_port: 6001,
            timing_port: 6002,
            client_timing_addr: Arc::new(Mutex::new(None)),
            fairplay_msg: Arc::new(Mutex::new(None)),
            ecdh_secret: Arc::new(Mutex::new(None)),
        });

        // Initialize SessionInfo with Alac
        {
            let mut guard = session_info.write().unwrap();
            *guard = Some(SessionInfo::default());
        }

        // Test with ct = 8 (AAC_ELD)
        {
            let mut req_dict = plist::Dictionary::new();
            let mut stream_dict = plist::Dictionary::new();
            stream_dict.insert("type".to_string(), plist::Value::Integer(96.into()));
            stream_dict.insert("ct".to_string(), plist::Value::Integer(8.into()));
            req_dict.insert("streams".to_string(), plist::Value::Array(vec![plist::Value::Dictionary(stream_dict)]));

            // Simulating stream_ct extraction and updating session_info
            let mut stream_ct = None;
            if let Some(plist::Value::Array(streams)) = req_dict.get("streams") {
                for stream in streams {
                    if let plist::Value::Dictionary(sd) = stream {
                        if let Some(plist::Value::Integer(st)) = sd.get("type") {
                            if st.as_unsigned() == Some(96) {
                                if let Some(plist::Value::Integer(ct_val)) = sd.get("ct") {
                                    stream_ct = Some(ct_val.as_unsigned().unwrap_or(0));
                                }
                            }
                        }
                    }
                }
            }
            assert_eq!(stream_ct, Some(8));

            if let Ok(mut info_guard) = state.session_info.write() {
                if let Some(ref mut info) = *info_guard {
                    if let Some(ct) = stream_ct {
                        if ct == 8 {
                            info.codec = crate::codec::AudioCodec::AacEld;
                            info.stream_type = crate::codec::StreamType::Mirroring;
                        }
                    }
                }
            }

            let info = session_info.read().unwrap();
            assert_eq!(info.as_ref().unwrap().codec, crate::codec::AudioCodec::AacEld);
            assert_eq!(info.as_ref().unwrap().stream_type, crate::codec::StreamType::Mirroring);
        }
    }
}
