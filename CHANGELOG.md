# Changelog

## 0.1.0-beta.1

First beta of the native camera-to-RTSP server. Crate version remains `0.1.0`;
this release is the `v0.1.0-beta.1` git tag.

### Supported

- macOS 12+ CLI (AVFoundation capture, VideoToolbox H.264)
- Windows 10/11 x64 CLI (Media Foundation capture, OS H.264 encoder)
- Android API 26+ (Camera2, MediaCodec, in-process Rust RTSP server)

### Unsupported in this beta

- Linux camera capture (the capture crate is a stub on that OS)
- iOS, RTSP over TLS, and a desktop GUI

### Known limits

- macOS `request_keyframe` is not wired; new viewers wait up to the GOP
  interval (default 2 seconds)
- Android `nativeGetStats` always reports `viewers:0`

### Play the stream

```bash
ffplay -rtsp_transport tcp rtsp://127.0.0.1:8554/camera
```

### Windows live check

This pass is not run in CI. On a Windows 10/11 x64 machine with a camera:

```text
cargo test --workspace
cargo run -p camrtsp -- --camera 0 --transport tcp
ffplay -rtsp_transport tcp rtsp://127.0.0.1:8554/camera
```

Pass means visible video, or a stable TCP PLAY that delivers RTP the same way
the macOS smoke test does. Until that pass is recorded, Windows is implemented
but not called verified.
