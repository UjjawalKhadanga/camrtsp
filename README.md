# camrtsp

Camera-to-RTSP server written in Rust. Version `0.1.0` (`v0.1.0-beta.1`).

Licensed under MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).

## Supported platforms

- **macOS 12+ CLI:** in-process AVFoundation capture and VideoToolbox H.264
- **Windows 10/11 x64 CLI:** Media Foundation Source Reader capture and the OS
  H.264 encoder transform (no helper multimedia process)
- **Android API 26+:** Camera2 and MediaCodec in a foreground service; the
  Rust RTSP server runs in-process through JNI (`apps/android`)

Linux is not supported in this beta. The capture crate returns a stub error
on that OS. iOS, RTSP over TLS, and a desktop GUI are also out of scope.

The RTSP core has H.264 RTP packetization, multiple viewers, TCP
interleaving, UDP RTP/RTCP, dynamic SDP, and optional Basic or Digest
authentication.

## Known limits

- macOS keyframe requests are not wired; a late joiner waits up to the GOP
  interval (default 2 seconds)
- Android `nativeGetStats` always reports `viewers:0`

## Local setup

```bash
source "$HOME/.cargo/env"
cargo build
cargo test --workspace
cargo run -p camrtsp -- devices
cargo run -p camrtsp -- --camera 0
cargo run -p camrtsp -- --username admin --password secret
```

Play the stream with:

```bash
ffplay -rtsp_transport tcp rtsp://127.0.0.1:8554/camera
```

`ffplay` is an optional external client. It is not used at build time or
runtime.

## Windows build, packaging, and live check

Build and test on a Windows 10/11 x64 machine using the MSVC target:

```powershell
cargo test --workspace
cargo run -p camrtsp -- devices --json
./scripts/package-windows.ps1
```

The packaging script produces `dist/camrtsp-<version>-windows-x64.zip` and its
SHA-256 checksum. Set `CAMRTSP_CERTIFICATE_THUMBPRINT` (and optionally
`CAMRTSP_TIMESTAMP_URL`) to sign and verify the executable with `signtool`.

Live playback is not run in CI. On a machine with a camera:

```text
cargo run -p camrtsp -- --camera 0 --transport tcp
ffplay -rtsp_transport tcp rtsp://127.0.0.1:8554/camera
```

Until that pass is recorded, Windows is implemented but not called verified.

## Workspace

- `camrtsp-core`: shared camera and video types
- `camrtsp-capture`: platform capture abstraction
- `camrtsp-codec`: encoded-frame types and encoder contract
- `camrtsp-rtp`: H.264 RTP packetization
- `camrtsp-server`: embedded RTSP server
- `camrtsp-android`: Android JNI shared library
- `apps/cli`: `camrtsp` executable
- `apps/android`: Android APK project
