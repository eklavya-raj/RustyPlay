//! RustyPlay - AirPlay Receiver Library
//!
//! This library provides the core functionality for the RustyPlay AirPlay receiver,
//! including video/audio decoding, network protocols, and rendering.

pub mod audio;
pub mod codec;
pub mod http;
pub mod mdns;
pub mod mirror;
pub mod ntp;
pub mod rtp;
pub mod rtsp;
pub mod video;

use std::sync::{Arc, Mutex, RwLock};
use anyhow::Result;
use ntp::ClockSync;
use rtp::JitterBuffer;

// Re-export commonly used types
pub use codec::{SessionInfo, AudioCodec, StreamType, derive_video_keys};

/// AirPlay server configuration
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub http_port: u16,
    pub rtsp_port: u16,
    pub rtp_port: u16,
    pub control_port: u16,
    pub timing_port: u16,
    pub verbose: bool,
    pub force_sw_dec: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            http_port: 7000,
            rtsp_port: 5000,
            rtp_port: 6000,
            control_port: 6001,
            timing_port: 6002,
            verbose: false,
            force_sw_dec: false,
        }
    }
}

/// AirPlay server instance
pub struct AirPlayServer {
    pub config: ServerConfig,
    runtime: Option<tokio::runtime::Runtime>,
    handles: Vec<tokio::task::JoinHandle<Result<()>>>,
}

impl AirPlayServer {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            runtime: None,
            handles: Vec::new(),
        }
    }

    pub fn start(&mut self) -> Result<()> {
        if self.runtime.is_some() {
            return Ok(()); // Already running
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        let config = self.config.clone();

        let handles = runtime.block_on(async {
            let mut handles = Vec::new();

            // Shared state
            let jitter_buffer = Arc::new(Mutex::new(JitterBuffer::new(64)));
            let session_info: Arc<RwLock<Option<SessionInfo>>> = Arc::new(RwLock::new(None));
            let clock_sync = Arc::new(RwLock::new(ClockSync::new(44100)));

            let client_timing_addr = Arc::new(Mutex::new(None));
            let fairplay_msg = Arc::new(Mutex::new(None));
            let ecdh_secret = Arc::new(Mutex::new(None));

            // RTSP shared state
            let rtsp_state = Arc::new(rtsp::RtspState {
                jitter_buffer: jitter_buffer.clone(),
                session_info: session_info.clone(),
                rtp_port: config.rtp_port,
                control_port: config.control_port,
                timing_port: config.timing_port,
                client_timing_addr: client_timing_addr.clone(),
                fairplay_msg: fairplay_msg.clone(),
                ecdh_secret: ecdh_secret.clone(),
                clock_sync: clock_sync.clone(),
            });

            // HTTP shared state
            let http_state = Arc::new(http::HttpState {
                fairplay_msg: fairplay_msg.clone(),
                session_info: session_info.clone(),
                ecdh_secret: ecdh_secret.clone(),
            });

            // Sender address for retransmit requests
            let sender_addr: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>> = Arc::new(std::sync::Mutex::new(None));

            // Spawn servers
            handles.push(tokio::spawn(http::start_http_server(config.http_port, http_state)));
            handles.push(tokio::spawn(rtsp::start_rtsp_server(config.rtsp_port, rtsp_state)));
            handles.push(tokio::spawn(rtp::start_rtp_receiver(config.rtp_port, jitter_buffer.clone())));
            handles.push(tokio::spawn(rtp::start_rtp_control(
                config.control_port,
                clock_sync.clone(),
                jitter_buffer.clone(),
                sender_addr.clone(),
            )));
            handles.push(tokio::spawn(ntp::start_timing_server(config.timing_port, clock_sync.clone(), client_timing_addr.clone())));
            handles.push(tokio::spawn(audio::run_audio_pipeline(
                jitter_buffer,
                session_info.clone(),
                clock_sync,
            )));
            handles.push(tokio::spawn(mirror::start_mirroring_server(7100, session_info.clone())));

            // Wait a moment before mDNS
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            // Start mDNS
            let http_running = !handles[0].is_finished();
            let rtsp_running = !handles[1].is_finished();

            if http_running || rtsp_running {
                let _ = mdns::start_mdns(http_running, rtsp_running, config.http_port, config.rtsp_port).await;
            }

            handles
        });

        self.runtime = Some(runtime);
        self.handles = handles;
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
            self.handles.clear();
        }
    }

    pub fn is_running(&self) -> bool {
        self.runtime.is_some()
    }
}
