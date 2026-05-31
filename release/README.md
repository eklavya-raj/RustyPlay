# AirPlay Receiver for OBS - Final Release

## Quick Start

This is a fully functional AirPlay receiver that works with OBS Studio via window capture.

### Usage

1. **Run the receiver:**
   ```bash
   ./rusty-play
   ```

2. **Connect from iOS device:**
   - Open Control Center on your iOS device
   - Tap Screen Mirroring
   - Select "RustyPlay" from the list

3. **Add to OBS:**
   - In OBS, add "Window Capture" source
   - Select the AirPlay receiver window
   - Add "Application Audio Capture" for audio

## Features

- ✅ Full AirPlay 2 support
- ✅ Screen mirroring (video)
- ✅ Audio streaming
- ✅ Hardware-accelerated decoding (macOS)
- ✅ Low latency (~200-300ms)
- ✅ mDNS/Bonjour advertising
- ✅ AES-CTR decryption support

## Ports Used

- HTTP: 7000 (control)
- RTSP: 5000 (audio)
- RTP: 6000 (audio data)
- Mirroring: 7100 (video)

## System Requirements

- macOS 12+ (Apple Silicon recommended)
- OBS Studio 28+
- iOS 12+ device

## Troubleshooting

### Device not appearing in AirPlay menu
- Ensure both devices are on the same network
- Check firewall settings (allow ports 7000, 5000, 6000, 7100)
- Restart the receiver

### No audio in OBS
- Add "Application Audio Capture" source in OBS
- Select the rusty-play application
- Check OBS audio settings

### Video lag
- Ensure hardware acceleration is enabled
- Check network bandwidth
- Reduce video resolution on iOS device

## Advanced Options

The receiver can be configured by modifying the source code in the main project. Default ports can be changed in `src/lib.rs`.

## License

Same as the main ux-play-rust project.

## Support

For issues or questions, refer to the main project documentation.
