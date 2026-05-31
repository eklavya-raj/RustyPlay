# RustyPlay

A 100% Rust AirPlay 2 receiver optimized for ultra-low latency audio and video mirroring.

## Features & Status

RustyPlay has evolved into a feature-complete AirPlay receiver:

- **mDNS Service Discovery**: Advertises `_airplay._tcp` and `_raop._tcp` dynamically using Bonjour on macOS and `libmdns` on Linux as "RustyPlay".
- **HTTP Server**: Supports AirPlay control endpoints.
- **RTSP Server**: Handles `OPTIONS`, `ANNOUNCE`, `SETUP`, `RECORD`, `FLUSH`, and `TEARDOWN` requests with dynamic audio stream codec negotiation (`ALAC` vs `AAC-ELD`).
- **AirPlay 2 Video & Screen Mirroring**: Supports dynamic screen mirroring on port 7100 with native FFmpeg-based video pipeline.
- **Native Video Pipeline**:
  - **FFmpeg H.264 Decoding**: Hardware-accelerated decoding via VideoToolbox on macOS with automatic software fallback.
  - **macOS Native Rendering**: Native NSWindow display using AVFoundation and Metal for ultra-low latency (<100ms glass-to-glass).
  - **Dynamic Resolution Support**: Seamless handling of orientation changes and resolution updates.
  - **No External Dependencies**: Eliminates the need for GStreamer installation.
- **Video Processing**: Annex-B formatting (AVCC conversion), stateful AES-128-CTR decryption, and H.264 NAL slice header parsing.
- **Audio Output**: Dynamic playout using standard `cpal`.
- **High-Performance Audio Decoding**:
  - `ALAC` (Apple Lossless) decoding using `symphonia` for standard audio-only streams (AirPlay 1).
  - `AAC-ELD` (Enhanced Low Delay) decoding using native Fraunhofer `libfdk-aac` for real-time low-latency mirroring audio (AirPlay 2).
- **RTP and Jitter Buffer**: Resilient packet sequencing, clock synchronization, and out-of-order packet reassembly.

## Building

### Prerequisites

You must have `fdk-aac` and `ffmpeg` installed on your host system:
- **macOS**: `brew install fdk-aac ffmpeg pkg-config`
- **Linux**: `sudo apt-get install libfdk-aac-dev libavcodec-dev libavformat-dev libavutil-dev pkg-config`

### Compile

```bash
cargo build --release
```

## Running

```bash
cargo run --release -- --http-port 7000 --rtsp-port 5000
```

The server will:
- Advertise via mDNS as "RustyPlay"
- Listen for HTTP requests on port 7000 (AirPlay control)
- Listen for RTSP requests on port 5000 (RAOP audio/video control)
- Listen for TCP video mirroring on port 7100
- Play real-time decoded audio output via your system speakers
- Display video in a native macOS window with hardware-accelerated rendering

### Command-Line Options

```
Usage: rusty-play [OPTIONS]

Options:
  -p, --port <PORT>           Set both HTTP and RTSP port (default: 7000/5000)
      --http-port <PORT>      Set HTTP port (default: 7000)
      --rtsp-port <PORT>      Set RTSP port (default: 5000)
      --rtp-port <PORT>       Set RTP port (default: 6000)
      --control-port <PORT>   Set control port (default: 6001)
      --timing-port <PORT>    Set timing port (default: 6002)
  -v, --verbose               Enable logging
  -avdec, --software-decoding Force software H.264 decoding (default: hardware-accelerated via FFmpeg)
  -h, --help                  Print help

Video Pipeline:
  Uses FFmpeg for H.264 decoding with hardware acceleration (VideoToolbox on macOS)
  Native macOS rendering via AVFoundation and Metal for low-latency display
```

## Architecture

The project has a modular, high-performance architecture:
- **mDNS Service Discovery**: Dynamic network advertisement.
- **HTTP Control Loop**: Handles basic protocol pairing.
- **RTSP Session Handler**: Manages stream setup, encryption keys, and control.
- **RTP & Jitter Buffer**: Real-time packet buffering and reordering.
- **Audio Pipeline**: Real-time output utilizing `cpal`, `symphonia` (ALAC), and `fdk-aac` (AAC-ELD).
- **Video Pipeline**: Native FFmpeg-based H.264 decoding and macOS rendering:
  - **FFmpeg Decoder**: Hardware-accelerated H.264 decoding via VideoToolbox with software fallback.
  - **macOS Renderer**: Native NSWindow display using AVFoundation and Metal/Core Animation.
  - **Pipeline Coordinator**: Manages frame flow, resolution changes, and error recovery.
- **Mirroring Server**: TCP server managing counter-mode AES-CTR decryption, AVCC-to-Annex-B translation, and H.264 slice validation.

### Video Pipeline Details

The video pipeline eliminates external process dependencies by using native Rust FFmpeg bindings:

1. **Decryption & Format Conversion**: Encrypted H.264 frames are decrypted using AES-128-CTR and converted from AVCC to Annex-B format.
2. **FFmpeg Decoding**: NAL units are decoded using FFmpeg with VideoToolbox hardware acceleration on macOS (automatic software fallback if unavailable).
3. **Native Rendering**: Decoded frames are displayed in a native NSWindow using AVFoundation's AVSampleBufferDisplayLayer and Metal for GPU-accelerated rendering.
4. **Dynamic Resolution**: Seamlessly handles orientation changes and resolution updates without dropping frames.
5. **Low Latency**: Maintains <100ms glass-to-glass latency through zero-copy techniques and real-time frame prioritization.

## Legal Note

This project implements reverse-engineered AirPlay protocols. Commercial use may require Apple's MFi program due to FairPlay encryption and patented technologies.
