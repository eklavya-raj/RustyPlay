use anyhow::Result;
use std::sync::{Arc, Mutex, RwLock};
use tracing::info;
use std::env;

mod audio;
mod codec;
mod http;
mod mdns;
mod ntp;
mod rtp;
mod rtsp;
mod mirror;
mod video;

use codec::SessionInfo;
use ntp::ClockSync;
use rtp::JitterBuffer;

/// Default ports for the AirPlay receiver
const DEFAULT_HTTP_PORT: u16 = 7000;
const DEFAULT_RTSP_PORT: u16 = 5000;
const DEFAULT_RTP_PORT: u16 = 6000;
const DEFAULT_CONTROL_PORT: u16 = 6001;
const DEFAULT_TIMING_PORT: u16 = 6002;

struct Config {
    http_port: u16,
    rtsp_port: u16,
    rtp_port: u16,
    control_port: u16,
    timing_port: u16,
    verbose: bool,
    force_sw_dec: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_port: DEFAULT_HTTP_PORT,
            rtsp_port: DEFAULT_RTSP_PORT,
            rtp_port: DEFAULT_RTP_PORT,
            control_port: DEFAULT_CONTROL_PORT,
            timing_port: DEFAULT_TIMING_PORT,
            verbose: false,
            force_sw_dec: false,
        }
    }
}

fn parse_args() -> Config {
    let args: Vec<String> = env::args().collect();
    let mut config = Config::default();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" | "--port" => {
                if i + 1 < args.len() {
                    if let Ok(port) = args[i + 1].parse::<u16>() {
                        config.http_port = port;
                        config.rtsp_port = port;
                    }
                    i += 1;
                }
            }
            "--http-port" => {
                if i + 1 < args.len() {
                    if let Ok(port) = args[i + 1].parse::<u16>() {
                        config.http_port = port;
                    }
                    i += 1;
                }
            }
            "--rtsp-port" => {
                if i + 1 < args.len() {
                    if let Ok(port) = args[i + 1].parse::<u16>() {
                        config.rtsp_port = port;
                    }
                    i += 1;
                }
            }
            "--rtp-port" => {
                if i + 1 < args.len() {
                    if let Ok(port) = args[i + 1].parse::<u16>() {
                        config.rtp_port = port;
                    }
                    i += 1;
                }
            }
            "--control-port" => {
                if i + 1 < args.len() {
                    if let Ok(port) = args[i + 1].parse::<u16>() {
                        config.control_port = port;
                    }
                    i += 1;
                }
            }
            "--timing-port" => {
                if i + 1 < args.len() {
                    if let Ok(port) = args[i + 1].parse::<u16>() {
                        config.timing_port = port;
                    }
                    i += 1;
                }
            }
            "-v" | "--verbose" => {
                config.verbose = true;
            }
            "-avdec" | "--software-decoding" => {
                config.force_sw_dec = true;
            }
            "-h" | "--help" => {
                println!("Usage: rusty-play [OPTIONS]");
                println!();
                println!("Options:");
                println!("  -p, --port <PORT>           Set both HTTP and RTSP port (default: 7000/5000)");
                println!("      --http-port <PORT>      Set HTTP port (default: 7000)");
                println!("      --rtsp-port <PORT>      Set RTSP port (default: 5000)");
                println!("      --rtp-port <PORT>       Set RTP port (default: 6000)");
                println!("      --control-port <PORT>   Set control port (default: 6001)");
                println!("      --timing-port <PORT>    Set timing port (default: 6002)");
                println!("  -v, --verbose               Enable logging");
                println!("  -avdec, --software-decoding Force software H.264 decoding (default: hardware-accelerated via FFmpeg)");
                println!("  -h, --help                  Print this help");
                println!();
                println!("Video Pipeline:");
                println!("  Uses FFmpeg for H.264 decoding with hardware acceleration (VideoToolbox on macOS)");
                println!("  Native macOS rendering via AVFoundation and Metal for low-latency display");
                std::process::exit(0);
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                eprintln!("Use -h or --help for usage information");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    config
}

fn main() -> Result<()> {
    // On macOS, the main thread MUST run the Cocoa event loop (`[NSApp run]`) for
    // GUI windows to draw and for GCD main-queue closures to execute. Therefore we
    // run the Tokio runtime on a background thread and keep the main thread free to
    // pump the AppKit run loop.
    #[cfg(target_os = "macos")]
    {
        // Build the Tokio runtime and run all async work on a dedicated background thread.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        std::thread::Builder::new()
            .name("tokio-runtime".to_string())
            .spawn(move || {
                runtime.block_on(async {
                    if let Err(e) = async_main().await {
                        tracing::error!("async_main exited with error: {:?}", e);
                    }
                });
            })?;

        // Initialize NSApplication on the main thread and run the Cocoa event loop.
        // This blocks forever, pumping GUI events and executing main-queue dispatches.
        unsafe {
            use cocoa::appkit::{NSApplication, NSApplicationActivationPolicy};
            use cocoa::base::nil;
            use objc::{msg_send, sel, sel_impl};

            let app = NSApplication::sharedApplication(nil);
            app.setActivationPolicy_(NSApplicationActivationPolicy::NSApplicationActivationPolicyRegular);

            // Run the AppKit event loop on the main thread (never returns).
            let _: () = msg_send![app, run];
        }

        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async_main())
    }
}

async fn async_main() -> Result<()> {
    let config = parse_args();


    if config.force_sw_dec {
        mirror::FORCE_SOFTWARE_DECODER.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // Note: videosink option is no longer used with native video pipeline

    let level_filter = if config.verbose {
        tracing_subscriber::filter::LevelFilter::INFO
    } else {
        tracing_subscriber::filter::LevelFilter::OFF
    };

    tracing_subscriber::fmt()
        .with_max_level(level_filter)
        .with_target(false)
        .with_thread_ids(true)
        .init();

    info!("Starting RustyPlay AirPlay Receiver");
    info!("  HTTP (AirPlay control):  port {}", config.http_port);
    info!("  RTSP (RAOP audio):       port {}", config.rtsp_port);
    info!("  RTP  (audio data):       port {}", config.rtp_port);
    info!("  RTP  (control):          port {}", config.control_port);
    info!("  NTP  (timing):           port {}", config.timing_port);

    // Shared state
    let jitter_buffer = Arc::new(Mutex::new(JitterBuffer::new(100)));
    let session_info: Arc<RwLock<Option<SessionInfo>>> = Arc::new(RwLock::new(None));
    let clock_sync = Arc::new(RwLock::new(ClockSync::new(44100)));

    let client_timing_addr = Arc::new(Mutex::new(None));
    let fairplay_msg = Arc::new(Mutex::new(None));

    // Shared ECDH secret (used by both HTTP and RTSP)
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
    });

    // HTTP shared state
    let http_state = Arc::new(http::HttpState {
        fairplay_msg: fairplay_msg.clone(),
        session_info: session_info.clone(),
        ecdh_secret: ecdh_secret.clone(),
    });

    // Spawn all server tasks first to check if they can bind
    let mut http_handle = tokio::spawn(http::start_http_server(config.http_port, http_state));
    let mut rtsp_handle = tokio::spawn(rtsp::start_rtsp_server(config.rtsp_port, rtsp_state));

    // Wait a moment to see if HTTP and RTSP servers can bind
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Check if servers started successfully before registering mDNS
    let http_running = !http_handle.is_finished();
    let rtsp_running = !rtsp_handle.is_finished();

    if !http_running {
        tracing::warn!("HTTP server failed to start, will not register _airplay._tcp mDNS service");
    }
    if !rtsp_running {
        tracing::warn!("RTSP server failed to start, will not register _raop._tcp mDNS service");
    }

    // Only start mDNS if at least one server is running
    if http_running || rtsp_running {
        mdns::start_mdns(http_running, rtsp_running, config.http_port, config.rtsp_port).await?;
    } else {
        tracing::error!("Neither HTTP nor RTSP server could start - skipping mDNS registration");
    }

    let mut rtp_handle = tokio::spawn(rtp::start_rtp_receiver(config.rtp_port, jitter_buffer.clone()));
    let mut control_handle = tokio::spawn(rtp::start_rtp_control(config.control_port));
    let mut timing_handle = tokio::spawn(ntp::start_timing_server(config.timing_port, clock_sync.clone(), client_timing_addr.clone()));
    let mut audio_handle = tokio::spawn(audio::run_audio_pipeline(
        jitter_buffer,
        session_info.clone(),
        clock_sync,
    ));
    let _mirror_handle = tokio::spawn(mirror::start_mirroring_server(7100, session_info.clone()));

    info!("All services started — ready to receive AirPlay connections");

    // Wait for any task to finish (they should all run forever)
    // Note: HTTP and RTSP server failures are non-fatal (like C++ version)
    loop {
        tokio::select! {
            res = &mut http_handle => {
                if let Ok(Err(e)) = res {
                    tracing::error!("HTTP server exited: {:?}", e);
                    tracing::warn!("Continuing without HTTP server (AirPlay control)");
                }
                // HTTP server failed, continue waiting for other tasks
                tokio::select! {
                    res = &mut rtsp_handle => {
                        if let Ok(Err(e)) = res {
                            tracing::error!("RTSP server exited: {:?}", e);
                            tracing::warn!("Continuing without RTSP server (RAOP audio)");
                        }
                        // RTSP server failed, continue waiting for remaining tasks
                        tokio::select! {
                            res = &mut rtp_handle => {
                                if let Ok(Err(e)) = res {
                                    tracing::error!("RTP receiver exited: {:?}", e);
                                }
                            }
                            res = &mut control_handle => {
                                if let Ok(Err(e)) = res {
                                    tracing::error!("RTP control exited: {:?}", e);
                                }
                            }
                            res = &mut timing_handle => {
                                if let Ok(Err(e)) = res {
                                    tracing::error!("NTP timing exited: {:?}", e);
                                }
                            }
                            res = &mut audio_handle => {
                                if let Ok(Err(e)) = res {
                                    tracing::error!("Audio pipeline exited: {:?}", e);
                                }
                            }
                        }
                    }
                    res = &mut rtp_handle => {
                        if let Ok(Err(e)) = res {
                            tracing::error!("RTP receiver exited: {:?}", e);
                        }
                    }
                    res = &mut control_handle => {
                        if let Ok(Err(e)) = res {
                            tracing::error!("RTP control exited: {:?}", e);
                        }
                    }
                    res = &mut timing_handle => {
                        if let Ok(Err(e)) = res {
                            tracing::error!("NTP timing exited: {:?}", e);
                        }
                    }
                    res = &mut audio_handle => {
                        if let Ok(Err(e)) = res {
                            tracing::error!("Audio pipeline exited: {:?}", e);
                        }
                    }
                }
                break;
            }
            res = &mut rtsp_handle => {
                if let Ok(Err(e)) = res {
                    tracing::error!("RTSP server exited: {:?}", e);
                    tracing::warn!("Continuing without RTSP server (RAOP audio)");
                }
                // RTSP server failed, continue waiting for remaining tasks
                tokio::select! {
                    res = &mut rtp_handle => {
                        if let Ok(Err(e)) = res {
                            tracing::error!("RTP receiver exited: {:?}", e);
                        }
                    }
                    res = &mut control_handle => {
                        if let Ok(Err(e)) = res {
                            tracing::error!("RTP control exited: {:?}", e);
                        }
                    }
                    res = &mut timing_handle => {
                        if let Ok(Err(e)) = res {
                            tracing::error!("NTP timing exited: {:?}", e);
                        }
                    }
                    res = &mut audio_handle => {
                        if let Ok(Err(e)) = res {
                            tracing::error!("Audio pipeline exited: {:?}", e);
                        }
                    }
                }
                break;
            }
            res = &mut rtp_handle => {
                if let Ok(Err(e)) = res {
                    tracing::error!("RTP receiver exited: {:?}", e);
                }
            }
            res = &mut control_handle => {
                if let Ok(Err(e)) = res {
                    tracing::error!("RTP control exited: {:?}", e);
                }
            }
            res = &mut timing_handle => {
                if let Ok(Err(e)) = res {
                    tracing::error!("NTP timing exited: {:?}", e);
                }
            }
            res = &mut audio_handle => {
                if let Ok(Err(e)) = res {
                    tracing::error!("Audio pipeline exited: {:?}", e);
                }
            }
        }
        break;
    }

    Ok(())
}
