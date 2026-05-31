# OBS AirPlay Plugin

An OBS Studio plugin that allows you to receive AirPlay audio and video streams directly in OBS.

## Features

- **AirPlay Audio Source**: Receive audio from iOS devices
- **AirPlay Video Source**: Receive screen mirroring from iOS devices
- **AirPlay Server Dock**: Control the AirPlay server from OBS UI

## Building

### Prerequisites

- Rust (latest stable)
- CMake (3.10+)
- OBS Studio development files
- C compiler (gcc/clang or MSVC)

### Build Steps

1. Clone the repository:
```bash
git clone https://github.com/yourusername/ux-play-rust.git
cd ux-play-rust
```

2. Build the Rust library:
```bash
cd obs-plugin
cargo build --release
```

3. Build the OBS plugin:
```bash
mkdir build
cd build
cmake ..
cmake --build . --config Release
```

### Installation

Copy the built plugin to your OBS plugins directory:

- **Linux**: `~/.config/obs-studio/plugins/obs-airplay/`
- **macOS**: `~/Library/Application Support/obs-studio/plugins/obs-airplay/`
- **Windows**: `%APPDATA%\obs-studio\plugins\obs-airplay\`

## Usage

1. Open OBS Studio
2. Add a new source: "AirPlay Audio" or "AirPlay Video"
3. The AirPlay server will start automatically on ports 7000 (HTTP) and 5000 (RTSP)
4. Connect your iOS device via AirPlay
5. The audio/video will appear in OBS

## Architecture

The plugin consists of:

- **Rust Core**: The AirPlay receiver implementation (from the main ux-play-rust project)
- **FFI Layer**: C-compatible interface between Rust and OBS
- **OBS Plugin**: C plugin that registers sources and docks with OBS

## Development

### Project Structure

```
obs-plugin/
├── Cargo.toml              # Rust library configuration
├── CMakeLists.txt          # CMake build configuration
├── src/
│   ├── lib.rs             # Rust FFI layer
│   └── obs-airplay.c      # OBS plugin C code
├── data/
│   └── locale/
│       └── en-US.ini      # UI strings
└── README.md
```

### Adding New Features

1. Add functionality to the Rust FFI layer in `src/lib.rs`
2. Expose it via `extern "C"` functions
3. Call the functions from the C plugin in `src/obs-airplay.c`
4. Rebuild both the Rust library and the plugin

## License

This plugin uses the same license as the main ux-play-rust project.

## Credits

Based on the ux-play-rust AirPlay receiver implementation.
