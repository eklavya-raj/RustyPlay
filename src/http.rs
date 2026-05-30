use anyhow::Result;
use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::codec::SessionInfo;

/// HTTP server state shared across connections
pub struct HttpState {
    pub fairplay_msg: Arc<Mutex<Option<Vec<u8>>>>,
    pub session_info: Arc<RwLock<Option<SessionInfo>>>,
    pub ecdh_secret: Arc<Mutex<Option<[u8; 32]>>>,
}

/// The server-info plist XML returned dynamically for GET /server-info
fn make_server_info_plist() -> String {
    let mac = crate::mdns::get_mac_address();
    let name = crate::mdns::get_airplay_name();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>deviceid</key>
    <string>{}</string>
    <key>features</key>
    <integer>14807</integer>
    <key>model</key>
    <string>AppleTV3,2</string>
    <key>protovers</key>
    <string>1.0</string>
    <key>srcvers</key>
    <string>220.68</string>
    <key>statusflags</key>
    <integer>4</integer>
    <key>name</key>
    <string>{}</string>
</dict>
</plist>"#,
        mac, name
    )
}

/// The playback-info plist XML returned for GET /playback-info
const PLAYBACK_INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>duration</key>
    <real>0</real>
    <key>position</key>
    <real>0</real>
    <key>rate</key>
    <real>1</real>
    <key>readyToPlay</key>
    <true/>
    <key>playbackBufferEmpty</key>
    <true/>
    <key>playbackBufferFull</key>
    <false/>
    <key>playbackLikelyToKeepUp</key>
    <true/>
</dict>
</plist>"#;

type BoxBody = Full<Bytes>;

fn text_response(status: StatusCode, content_type: &str, body: &str) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn empty_response(status: StatusCode) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

async fn handle_request(req: Request<hyper::body::Incoming>, state: Arc<HttpState>) -> Result<Response<BoxBody>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    info!(method = %method, path = %path, "HTTP request");

    match (method.as_str(), path.as_str()) {
        ("GET", "/server-info") => {
            Ok(text_response(
                StatusCode::OK,
                "text/x-apple-plist+xml",
                &make_server_info_plist(),
            ))
        }

        ("GET", "/playback-info") => {
            Ok(text_response(
                StatusCode::OK,
                "text/x-apple-plist+xml",
                PLAYBACK_INFO_PLIST,
            ))
        }

        ("POST", "/reverse") => {
            info!("Reverse event connection requested");
            Ok(Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header("upgrade", "PTTH/1.0")
                .header("connection", "upgrade")
                .body(Full::new(Bytes::new()))?)
        }

        ("POST", "/play") => {
            // Read the request body for Content-Location
            let body_bytes = collect_body(req).await;
            let body_str = String::from_utf8_lossy(&body_bytes);

            // Parse Content-Location from the body (plist or text)
            let content_location = body_str.lines()
                .find(|l| l.trim().starts_with("Content-Location:") || l.contains("<key>Content-Location</key>"))
                .map(|l| l.trim().to_string());

            if let Some(loc) = &content_location {
                info!(location = %loc, "Play request — media URL");
            } else {
                info!(body_len = body_bytes.len(), "Play request received");
            }

            Ok(empty_response(StatusCode::OK))
        }

        ("POST", "/stop") => {
            info!("Stop request received");
            Ok(empty_response(StatusCode::OK))
        }

        ("POST", "/scrub") | ("GET", "/scrub") => {
            // GET /scrub returns current position; POST /scrub seeks
            if method.as_str() == "GET" {
                Ok(text_response(
                    StatusCode::OK,
                    "text/parameters",
                    "duration: 0.000000\nposition: 0.000000\n",
                ))
            } else {
                // Parse position from query string
                let position = req.uri().query()
                    .and_then(|q| q.split('&').find(|p| p.starts_with("position=")))
                    .and_then(|p| p.strip_prefix("position="))
                    .unwrap_or("0");
                info!(position = position, "Scrub request");
                Ok(empty_response(StatusCode::OK))
            }
        }

        ("POST", "/rate") => {
            let rate = req.uri().query()
                .and_then(|q| q.split('&').find(|p| p.starts_with("value=")))
                .and_then(|p| p.strip_prefix("value="))
                .unwrap_or("1");
            info!(rate = rate, "Rate request");
            Ok(empty_response(StatusCode::OK))
        }

        ("PUT", "/photo") => {
            let body_bytes = collect_body(req).await;
            info!(size = body_bytes.len(), "Photo received");
            // TODO: display or save the photo
            Ok(empty_response(StatusCode::OK))
        }

        ("POST", "/pair-setup") => {
            // Pairing setup — returns server's public key (32 bytes)
            let body_bytes = collect_body(req).await;
            info!(body_len = body_bytes.len(), "HTTP POST /pair-setup received");

            // Return a dummy 32-byte public key for now
            // In a full implementation, this would be the actual Ed25519 public key
            let public_key = [0u8; 32];
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/octet-stream")
                .body(Full::new(Bytes::from(public_key.to_vec())))?)
        }

        ("POST", "/pair-verify") => {
            // Pairing verify — completes the ECDH handshake
            let body_bytes = collect_body(req).await;
            info!(body_len = body_bytes.len(), "HTTP POST /pair-verify received");

            if body_bytes.len() < 4 {
                return Ok(empty_response(StatusCode::BAD_REQUEST));
            }

            let step = body_bytes[0];
            match step {
                1 => {
                    // Step 1: Client sends X25519 public key (32 bytes) + Ed25519 public key (32 bytes)
                    // Expected: [0x01, 0x00, 0x00, 0x00] + 32 bytes X25519 + 32 bytes Ed25519 = 68 bytes
                    if body_bytes.len() != 68 {
                        warn!(len = body_bytes.len(), "Invalid pair-verify step 1 length");
                        return Ok(empty_response(StatusCode::BAD_REQUEST));
                    }

                    let client_x25519_public = &body_bytes[4..36];
                    info!(client_pk = ?&client_x25519_public[..8], "Received client X25519 public key");

                    // Generate our ephemeral X25519 keypair
                    use rand::rngs::OsRng;
                    use x25519_dalek::{EphemeralSecret, PublicKey};
                    let server_secret = EphemeralSecret::random_from_rng(OsRng);
                    let server_public = PublicKey::from(&server_secret);

                    // Compute ECDH shared secret
                    let client_public_key = x25519_dalek::PublicKey::from(<[u8; 32]>::try_from(client_x25519_public).unwrap());
                    let shared_secret = server_secret.diffie_hellman(&client_public_key);

                    info!(
                        server_pk = ?&server_public.as_bytes()[..8],
                        shared_secret = ?&shared_secret.as_bytes()[..8],
                        "Generated ECDH shared secret"
                    );

                    // Store the shared secret for later use in SETUP
                    if let Ok(mut secret_guard) = state.ecdh_secret.lock() {
                        *secret_guard = Some(*shared_secret.as_bytes());
                        info!("ECDH shared secret stored for key hashing in SETUP");
                    }

                    // Return server public key + dummy signature
                    let mut response_data = Vec::with_capacity(64);
                    response_data.extend_from_slice(server_public.as_bytes()); // 32 bytes
                    response_data.extend_from_slice(&[0u8; 32]); // 32 bytes dummy Ed25519 signature

                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/octet-stream")
                        .body(Full::new(Bytes::from(response_data)))?)
                }
                0 => {
                    // Step 2: Client sends signature verification
                    info!("Pair-verify step 2: signature verification (skipped)");
                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/octet-stream")
                        .body(Full::new(Bytes::new()))?)
                }
                _ => {
                    warn!(step = step, "Unknown pair-verify step");
                    Ok(empty_response(StatusCode::BAD_REQUEST))
                }
            }
        }

        ("POST", "/fp-setup") => {
            // FairPlay setup — required for AirPlay 2
            let body_bytes = collect_body(req).await;
            let datalen = body_bytes.len();
            info!(body_len = datalen, "HTTP POST /fp-setup received");

            if datalen == 16 {
                if body_bytes[4] != 0x03 {
                    Ok(empty_response(StatusCode::INTERNAL_SERVER_ERROR))
                } else {
                    let mode = body_bytes[14] as usize;
                    let reply_message: [&[u8]; 4] = [
                        &[0x46,0x50,0x4c,0x59,0x03,0x01,0x02,0x00,0x00,0x00,0x00,0x82,0x02,0x00,0x0f,0x9f,0x3f,0x9e,0x0a,0x25,0x21,0xdb,0xdf,0x31,0x2a,0xb2,0xbf,0xb2,0x9e,0x8d,0x23,0x2b,0x63,0x76,0xa8,0xc8,0x18,0x70,0x1d,0x22,0xae,0x93,0xd8,0x27,0x37,0xfe,0xaf,0x9d,0xb4,0xfd,0xf4,0x1c,0x2d,0xba,0x9d,0x1f,0x49,0xca,0xaa,0xbf,0x65,0x91,0xac,0x1f,0x7b,0xc6,0xf7,0xe0,0x66,0x3d,0x21,0xaf,0xe0,0x15,0x65,0x95,0x3e,0xab,0x81,0xf4,0x18,0xce,0xed,0x09,0x5a,0xdb,0x7c,0x3d,0x0e,0x25,0x49,0x09,0xa7,0x98,0x31,0xd4,0x9c,0x39,0x82,0x97,0x34,0x34,0xfa,0xcb,0x42,0xc6,0x3a,0x1c,0xd9,0x11,0xa6,0xfe,0x94,0x1a,0x8a,0x6d,0x4a,0x74,0x3b,0x46,0xc3,0xa7,0x64,0x9e,0x44,0xc7,0x89,0x55,0xe4,0x9d,0x81,0x55,0x00,0x95,0x49,0xc4,0xe2,0xf7,0xa3,0xf6,0xd5,0xba],
                        &[0x46,0x50,0x4c,0x59,0x03,0x01,0x02,0x00,0x00,0x00,0x00,0x82,0x02,0x01,0xcf,0x32,0xa2,0x57,0x14,0xb2,0x52,0x4f,0x8a,0xa0,0xad,0x7a,0xf1,0x64,0xe3,0x7b,0xcf,0x44,0x24,0xe2,0x00,0x04,0x7e,0xfc,0x0a,0xd6,0x7a,0xfc,0xd9,0x5d,0xed,0x1c,0x27,0x30,0xbb,0x59,0x1b,0x96,0x2e,0xd6,0x3a,0x9c,0x4d,0xed,0x88,0xba,0x8f,0xc7,0x8d,0xe6,0x4d,0x91,0xcc,0xfd,0x5c,0x7b,0x56,0xda,0x88,0xe3,0x1f,0x5c,0xce,0xaf,0xc7,0x43,0x19,0x95,0xa0,0x16,0x65,0xa5,0x4e,0x19,0x39,0xd2,0x5b,0x94,0xdb,0x64,0xb9,0xe4,0x5d,0x8d,0x06,0x3e,0x1e,0x6a,0xf0,0x7e,0x96,0x56,0x16,0x2b,0x0e,0xfa,0x40,0x42,0x75,0xea,0x5a,0x44,0xd9,0x59,0x1c,0x72,0x56,0xb9,0xfb,0xe6,0x51,0x38,0x98,0xb8,0x02,0x27,0x72,0x19,0x88,0x57,0x16,0x50,0x94,0x2a,0xd9,0x46,0x68,0x8a],
                        &[0x46,0x50,0x4c,0x59,0x03,0x01,0x02,0x00,0x00,0x00,0x00,0x82,0x02,0x02,0xc1,0x69,0xa3,0x52,0xee,0xed,0x35,0xb1,0x8c,0xdd,0x9c,0x58,0xd6,0x4f,0x16,0xc1,0x51,0x9a,0x89,0xeb,0x53,0x17,0xbd,0x0d,0x43,0x36,0xcd,0x68,0xf6,0x38,0xff,0x9d,0x01,0x6a,0x5b,0x52,0xb7,0xfa,0x92,0x16,0xb2,0xb6,0x54,0x82,0xc7,0x84,0x44,0x11,0x81,0x21,0xa2,0xc7,0xfe,0xd8,0x3d,0xb7,0x11,0x9e,0x91,0x82,0xaa,0xd7,0xd1,0x8c,0x70,0x63,0xe2,0xa4,0x57,0x55,0x59,0x10,0xaf,0x9e,0x0e,0xfc,0x76,0x34,0x7d,0x16,0x40,0x43,0x80,0x7f,0x58,0x1e,0xe4,0xfb,0xe4,0x2c,0xa9,0xde,0xdc,0x1b,0x5e,0xb2,0xa3,0xaa,0x3d,0x2e,0xcd,0x59,0xe7,0xee,0xe7,0x0b,0x36,0x29,0xf2,0x2a,0xfd,0x16,0x1d,0x87,0x73,0x53,0xdd,0xb9,0x9a,0xdc,0x8e,0x07,0x00,0x6e,0x56,0xf8,0x50,0xce],
                        &[0x46,0x50,0x4c,0x59,0x03,0x01,0x02,0x00,0x00,0x00,0x00,0x82,0x02,0x03,0x90,0x01,0xe1,0x72,0x7e,0x0f,0x57,0xf9,0xf5,0x88,0x0d,0xb1,0x04,0xa6,0x25,0x7a,0x23,0xf5,0xcf,0xff,0x1a,0xbb,0xe1,0xe9,0x30,0x45,0x25,0x1a,0xfb,0x97,0xeb,0x9f,0xc0,0x01,0x1e,0xbe,0x0f,0x3a,0x81,0xdf,0x5b,0x69,0x1d,0x76,0xac,0xb2,0xf7,0xa5,0xc7,0x08,0xe3,0xd3,0x28,0xf5,0x6b,0xb3,0x9d,0xbd,0xe5,0xf2,0x9c,0x8a,0x17,0xf4,0x81,0x48,0x7e,0x3a,0xe8,0x63,0xc6,0x78,0x32,0x54,0x22,0xe6,0xf7,0x8e,0x16,0x6d,0x18,0xaa,0x7f,0xd6,0x36,0x25,0x8b,0xce,0x28,0x72,0x6f,0x66,0x1f,0x73,0x88,0x93,0xce,0x44,0x31,0x1e,0x4b,0xe6,0xc0,0x53,0x51,0x93,0xe5,0xef,0x72,0xe8,0x68,0x62,0x33,0x72,0x9c,0x22,0x7d,0x82,0x0c,0x99,0x94,0x45,0xd8,0x92,0x46,0xc8,0xc3,0x59]
                    ];
                    let payload = if mode < 4 { reply_message[mode] } else { reply_message[0] };
                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/octet-stream")
                        .body(Full::new(Bytes::from(payload.to_vec())))?)
                }
            } else if datalen == 164 {
                if body_bytes[4] != 0x03 {
                    Ok(empty_response(StatusCode::INTERNAL_SERVER_ERROR))
                } else {
                    // Save 164-byte body for PlayFair decryption later
                    if let Ok(mut msg_guard) = state.fairplay_msg.lock() {
                        *msg_guard = Some(body_bytes.to_vec());
                        info!("HTTP /fp-setup: Saved 164-byte FairPlay message for later decryption");
                    } else {
                        warn!("HTTP /fp-setup: Failed to acquire lock on fairplay_msg");
                    }

                    let fp_header = &[0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x14];
                    let mut payload = Vec::with_capacity(32);
                    payload.extend_from_slice(fp_header);
                    payload.extend_from_slice(&body_bytes[144..164]);

                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/octet-stream")
                        .body(Full::new(Bytes::from(payload)))?)
                }
            } else {
                Ok(empty_response(StatusCode::BAD_REQUEST))
            }
        }

        _ => {
            info!(method = %method, path = %path, "Unknown HTTP endpoint");
            Ok(empty_response(StatusCode::NOT_FOUND))
        }
    }
}

/// Collect the full body of an incoming HTTP request.
async fn collect_body(req: Request<hyper::body::Incoming>) -> Vec<u8> {
    use http_body_util::BodyExt;
    match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to read request body");
            Vec::new()
        }
    }
}

pub async fn start_http_server(port: u16, state: Arc<HttpState>) -> Result<()> {
    let addr: SocketAddr = format!("[::]:{}", port).parse()?;

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
                "Failed to bind HTTP server socket - Address already in use"
            );
            return Err(e.into());
        }
    }

    socket.listen(128)?;
    let listener = TcpListener::from_std(socket.into())?;

    info!(addr = %addr, "HTTP server listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let io = TokioIo::new(stream);

        info!(peer = %peer, "HTTP server accepted connection");

        let state_clone = state.clone();
        tokio::task::spawn(async move {
            let service = service_fn(move |req| handle_request(req, state_clone.clone()));
            if let Err(err) = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, service)
                .await
            {
                // Connection reset by peer is normal
                if !err.is_incomplete_message() {
                    tracing::error!(peer = %peer, error = %err, "HTTP connection error");
                }
            }
        });
    }
}
