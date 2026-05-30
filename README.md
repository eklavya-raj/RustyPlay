# RustyPlay

A 100% Rust AirPlay 2 receiver optimized for ultra-low latency audio and video mirroring.

## Features & Status

RustyPlay has evolved into a feature-complete AirPlay receiver:

- **mDNS Service Discovery**: Advertises `_airplay._tcp` and `_raop._tcp` dynamically using Bonjour on macOS and `libmdns` on Linux as "RustyPlay".
- **HTTP Server**: Supports AirPlay control endpoints.
- **RTSP Server**: Handles `OPTIONS`, `ANNOUNCE`, `SETUP`, `RECORD`, `FLUSH`, and `TEARDOWN` requests with dynamic audio stream codec negotiation (`ALAC` vs `AAC-ELD`).
- **AirPlay 2 Video & Screen Mirroring**: Supports dynamic screen mirroring on port 7100.
- **Video Processing**: Annex-B formatting (AVCC conversion), stateful AES-128-CTR decryption, and H.264 NAL slice header parsing.
- **Audio Output**: Dynamic playout using standard `cpal`.
- **High-Performance Audio Decoding**:
  - `ALAC` (Apple Lossless) decoding using `symphonia` for standard audio-only streams (AirPlay 1).
  - `AAC-ELD` (Enhanced Low Delay) decoding using native Fraunhofer `libfdk-aac` for real-time low-latency mirroring audio (AirPlay 2).
- **RTP and Jitter Buffer**: Resilient packet sequencing, clock synchronization, and out-of-order packet reassembly.

## Building

### Prerequisites

You must have `fdk-aac` installed on your host system:
- **macOS**: `brew install fdk-aac pkg-config`
- **Linux**: `sudo apt-get install libfdk-aac-dev pkg-config`

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

## Architecture

The project has a modular, high-performance architecture:
- **mDNS Service Discovery**: Dynamic network advertisement.
- **HTTP Control Loop**: Handles basic protocol pairing.
- **RTSP Session Handler**: Manages stream setup, encryption keys, and control.
- **RTP & Jitter Buffer**: Real-time packet buffering and reordering.
- **Audio Pipeline**: Real-time output utilizing `cpal`, `symphonia` (ALAC), and `fdk-aac` (AAC-ELD).
- **Mirroring Server**: TCP server managing counter-mode AES-CTR decryption, AVCC-to-Annex-B translation, and H.264 slice validation.

## Legal Note

This project implements reverse-engineered AirPlay protocols. Commercial use may require Apple's MFi program due to FairPlay encryption and patented technologies.
