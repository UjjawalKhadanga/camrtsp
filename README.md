<div align="center">

<img src="website/public/brand/banner.svg" alt="camrtsp — Keep the camera. Open the signal. Native camera to H.264 to RTSP." width="100%" />

### Keep the camera. Open the signal.

Turn your Mac, Windows PC, or Android phone into a native RTSP source.

Native capture. Hardware H.264. An RTSP server in the same process.  
No FFmpeg. No GStreamer. No sidecar media daemon.

[![Website](https://img.shields.io/badge/website-ujjawalkhadanga.github.io%2Fcamrtsp-b7ff5b)](https://ujjawalkhadanga.github.io/camrtsp/)
[![CI](https://github.com/UjjawalKhadanga/camrtsp/actions/workflows/ci.yml/badge.svg)](https://github.com/UjjawalKhadanga/camrtsp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/badge/release-v0.1.0--beta.1-91be65)](https://github.com/UjjawalKhadanga/camrtsp)
[![Rust](https://img.shields.io/badge/rust-1.91-orange?logo=rust)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)](LICENSE-MIT)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Windows%20%7C%20Android-informational)](#the-suite)

<p>
  <a href="#why-camrtsp"><strong>Why</strong></a> ·
  <a href="#the-suite"><strong>Suite</strong></a> ·
  <a href="#quick-start"><strong>Quick start</strong></a> ·
  <a href="#play-the-stream"><strong>Play</strong></a> ·
  <a href="#architecture"><strong>Architecture</strong></a>
</p>

`rtsp://127.0.0.1:8554/camera`

</div>

---

## Why camrtsp

IP cameras speak RTSP. Almost everything that records or displays live video already knows how to pull that URL: VLC, ffplay, OBS, NVRs, Home Assistant, and a long list of industrial viewers.

A laptop webcam or a phone camera does not. Getting one onto the LAN usually means:

- piping frames through **FFmpeg or GStreamer** into a separate media server
- buying a **dedicated IP camera** for a job a device you already own can do
- running a **helper process** just to encode H.264

camrtsp collapses that into one native program. It opens the OS camera API, encodes with the OS hardware encoder, and serves RTSP from the same process. Viewers connect with the URL they already use.

| You have | You need | camrtsp gives you |
| --- | --- | --- |
| A Mac, Windows PC, or Android phone with a camera | A URL other software can subscribe to | `rtsp://host:8554/camera` |
| An NVR, VLC, OBS, or ffplay | H.264 over RTSP (TCP or UDP) | In-process RTP packetization, SDP, optional auth |
| No desire to install a multimedia runtime | Capture + encode + serve | AVFoundation / Media Foundation / Camera2 + VideoToolbox / MFT / MediaCodec |

It is a **camera publisher**, not a media router. It does not transcode foreign streams, record to disk, or speak WebRTC. Those jobs belong to tools like [MediaMTX](https://github.com/bluenviron/mediamtx) and [go2rtc](https://github.com/AlexxIT/go2rtc). Point them at camrtsp when you want the webcam itself to be the source.

## The suite

One RTSP core, three ways to run it.

| | **Desktop CLI** | **Android app** | **Shared core** |
| --- | --- | --- | --- |
| **What** | `camrtsp` binary | Foreground-service APK | Rust crates |
| **Where** | macOS 12+, Windows 10/11 x64 | API 26+ | This workspace |
| **Capture** | AVFoundation / Media Foundation | Camera2 | `camrtsp-capture` |
| **Encode** | VideoToolbox / OS H.264 MFT | MediaCodec AVC | Access units into the server |
| **Serve** | Embedded RTSP on `:8554` | Same server via JNI | `camrtsp-server` + `camrtsp-rtp` |

```mermaid
flowchart LR
  cam[Camera]
  enc[OS H.264 encoder]
  core[camrtsp-server]
  vlc[VLC / ffplay]
  nvr[NVR / OBS]
  ha[Home Assistant]

  cam --> enc --> core
  core -->|RTSP TCP or UDP| vlc
  core --> nvr
  core --> ha
```

Linux capture is a stub in this beta. iOS, RTSP-over-TLS, and a desktop GUI are out of scope for v0.

## Features

- **Native only** — no FFmpeg, GStreamer, or bundled encoder binary
- **Hardware H.264** where the OS provides it, with a software encoder fallback on macOS
- **Multiple viewers** — late joiners wait at most one GOP (default 2 seconds)
- **TCP interleaved and UDP RTP/RTCP**, or lock the server to one transport
- **Dynamic SDP** from live SPS/PPS (`profile-level-id`, `sprop-parameter-sets`)
- **Optional Basic and Digest auth** (MD5 and SHA-256)
- **Phone as a camera** — Android keeps streaming under a camera foreground service with a wake lock

## Quick start

### macOS

```bash
cargo build --release -p camrtsp
./target/release/camrtsp devices
./target/release/camrtsp --camera 0
```

The first streaming run requests camera access from macOS. If access was
previously denied, enable camrtsp (or the terminal that launched it) in
**System Settings → Privacy & Security → Camera**.

On success:

```
Camera: …
Streaming: rtsp://127.0.0.1:8554/camera
Resolution: 1280x720 @ 30 FPS
Codec: H.264
```

### Windows

Build on Windows 10/11 x64 with the MSVC toolchain:

```powershell
cargo test --workspace
cargo run -p camrtsp -- devices --json
cargo run -p camrtsp -- --camera 0 --transport tcp
```

Package a zip and SHA-256 checksum:

```powershell
./scripts/package-windows.ps1
```

Set `CAMRTSP_CERTIFICATE_THUMBPRINT` (and optionally `CAMRTSP_TIMESTAMP_URL`) to Authenticode-sign the exe.

Windows capture and encode are implemented. A live camera → player pass is **not** part of CI; until you run the [Windows live check](#windows-live-check), treat Windows as unverified.

### Android

```bash
scripts/build-android.sh          # cargo-ndk, NDK 27.1.12297006
cd apps/android && ./gradlew assembleDebug
```

Grant camera access (and notifications on Android 13+), tap **Start streaming**, then open `rtsp://<phone-lan-ip>:8554/camera` from another device. Details: [`apps/android/README.md`](apps/android/README.md).

## Play the stream

Any RTSP client that understands H.264. TCP is the reliable default on messy networks:

```bash
ffplay -rtsp_transport tcp rtsp://127.0.0.1:8554/camera
```

VLC: **Media → Open Network Stream** → the same URL.  
OBS: **Media Source** → input `rtsp://127.0.0.1:8554/camera`.

`ffplay` / VLC / OBS are viewers only. camrtsp does not call them at build or run time.

With authentication:

```bash
./target/release/camrtsp --camera 0 --username admin --password secret
ffplay -rtsp_transport tcp rtsp://admin:secret@127.0.0.1:8554/camera
```

Password may also come from `CAMRTSP_PASSWORD` when `--username` is set.

## CLI

```
camrtsp devices [--json]
camrtsp --camera INDEX_OR_ID [options]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--camera` | `0` | Index from `devices`, or the camera id |
| `--resolution` | `1280x720` | Requested size; the OS picks the nearest preset |
| `--fps` | `30` | Requested frame rate |
| `--bitrate` | `auto` | `auto` or bits per second |
| `--gop` | `2` | Keyframe interval in seconds |
| `--bind` | `0.0.0.0:8554` | Listen address |
| `--path` | `/camera` | Must start with `/` |
| `--transport` | `both` | `both`, `tcp`, or `udp` |
| `--username` / `--password` | off | Both required if either is set |

## Architecture

```
apps/cli          camrtsp executable
apps/android      Camera2 UI + foreground service
        │
        ▼
camrtsp-android   JNI bridge (Android only)
camrtsp-capture   platform camera + encoder
camrtsp-server    RTSP, RTP, auth, fan-out
camrtsp-rtp       H.264 packetization
camrtsp-core      shared types
```

| Crate | Role |
| --- | --- |
| `camrtsp-core` | Camera ids, stream config, access units |
| `camrtsp-capture` | macOS / Windows pipelines |
| `camrtsp-codec` | Encoder contract (unused by the v0 path) |
| `camrtsp-rtp` | FU-A fragmentation and RTP headers |
| `camrtsp-server` | OPTIONS through TEARDOWN, interleaved TCP, UDP, Digest |
| `camrtsp-android` | `cdylib` loaded by the APK |

## Windows live check

Not run in CI. On a Windows machine with a camera:

```text
cargo test --workspace
cargo run -p camrtsp -- --camera 0 --transport tcp
ffplay -rtsp_transport tcp rtsp://127.0.0.1:8554/camera
```

Pass means a picture in the player, or a stable TCP PLAY that delivers RTP.

## Beta notes

This is **v0.1.0-beta.1**. Crate version is `0.1.0`.

- Late joiners trigger a platform keyframe request and recover on a clean H.264
  keyframe with current SPS/PPS
- Stream status reports live viewer, frame, readiness, and frame-age data on
  desktop and Android
- Live camera-to-player checks remain hardware-dependent and are not run in CI
- Linux, iOS, and RTSPS are not in this release

See [CHANGELOG.md](CHANGELOG.md).

## Website

Live at [ujjawalkhadanga.github.io/camrtsp](https://ujjawalkhadanga.github.io/camrtsp/). Source is in [`website/`](website/) (Astro + Tailwind). Explore the interactive signal playground, switch source devices and transports, and get platform-specific setup commands. The playground is a simulation; it does not access your camera.

The [brand assets](website/public/brand/) include the vector logo, repository banner, and social preview. The Android control panel uses the same charcoal and electric mint palette.

```bash
cd website
npm install
npm run dev
```

GitHub Pages can publish `website/dist` via [`.github/workflows/website.yml`](.github/workflows/website.yml) after Pages is enabled on the repo.

## License

[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
