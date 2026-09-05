use camrtsp_capture::NativeCamera;
use camrtsp_core::{AccessUnitSink, BitrateMode, CameraId, NativeVideoPipeline, StreamConfig};
use camrtsp_server::{Broadcaster, RtspServer, TransportPolicy};
use clap::{Parser, Subcommand, ValueEnum};
use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    process,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Parser, Debug)]
#[command(version, about = "Publish a native camera as an H.264 RTSP stream")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, default_value = "0")]
    camera: String,
    #[arg(long, default_value = "1280x720", value_parser = resolution)]
    resolution: (u32, u32),
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,
    #[arg(long, default_value = "auto", value_parser = bitrate)]
    bitrate: BitrateMode,
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=60))]
    gop: u32,
    #[arg(long, default_value = "0.0.0.0:8554")]
    bind: SocketAddr,
    #[arg(long, default_value = "/camera", value_parser = stream_path)]
    path: String,
    #[arg(long, value_enum, default_value_t = Transport::Both)]
    transport: Transport,
    #[arg(long, requires = "password")]
    username: Option<String>,
    #[arg(
        long,
        env = "CAMRTSP_PASSWORD",
        hide_env_values = true,
        requires = "username"
    )]
    password: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    Devices {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Transport {
    Both,
    Tcp,
    Udp,
}

fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("camrtsp: {error}");
        process::exit(1);
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let devices = NativeCamera::default().enumerate()?;
    if let Some(Command::Devices { json }) = args.command {
        if json {
            println!("{}", serde_json::to_string_pretty(&devices)?);
        } else {
            for (index, device) in devices.iter().enumerate() {
                println!("{index}: {} ({})", device.name, device.id);
            }
        }
        return Ok(());
    }
    let camera = devices
        .iter()
        .find(|device| device.id.0 == args.camera)
        .or_else(|| {
            args.camera
                .parse::<usize>()
                .ok()
                .and_then(|index| devices.get(index))
        })
        .ok_or("Camera not found. Run 'camrtsp devices' to list available cameras.")?;
    let config = StreamConfig {
        camera: CameraId(camera.id.0.clone()),
        requested_width: args.resolution.0,
        requested_height: args.resolution.1,
        requested_fps: args.fps,
        bitrate: args.bitrate,
        gop_seconds: args.gop,
    };
    config.validate()?;
    let credentials = args.username.zip(args.password);
    let broadcaster = Broadcaster::default();
    let server = RtspServer::bind_with_transport(
        &args.bind.to_string(),
        &args.path,
        broadcaster.clone(),
        args.fps,
        credentials,
        match args.transport {
            Transport::Both => TransportPolicy::Both,
            Transport::Tcp => TransportPolicy::Tcp,
            Transport::Udp => TransportPolicy::Udp,
        },
    )?;
    let address = server.local_addr()?;
    let publisher = broadcaster.clone();
    let config_publisher = broadcaster.clone();
    let sink = AccessUnitSink::new(
        move |unit| publisher.publish(unit),
        move |codec| config_publisher.set_codec_config(codec),
    );
    let mut capture = NativeCamera::default();
    let negotiated = capture.start(config, sink)?;
    broadcaster.set_frame_rate(negotiated.fps_num, negotiated.fps_den);
    let stopping = Arc::new(AtomicBool::new(false));
    let server_stop = stopping.clone();
    // Keep platform capture objects on their owning thread; only RTSP runs here.
    let worker = thread::spawn(move || server.run_until(server_stop));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let shutdown = tokio::signal::ctrl_c();
            tokio::pin!(shutdown);
            let started = Instant::now();
            let mut announced = false;
            loop {
                tokio::select! {
                    result = &mut shutdown => { result?; return Ok::<_, Box<dyn std::error::Error>>(()); }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
                if worker.is_finished() { return Err("RTSP server stopped unexpectedly".into()); }
                if broadcaster.take_keyframe_request() && let Err(error) = capture.request_keyframe() {
                    eprintln!("Keyframe request unavailable; waiting for the camera's next keyframe: {error}");
                }
                let stats = broadcaster.stats();
                if !announced && stats.ready {
                    println!("Camera: {}", camera.name);
                    for address in stream_addresses(address) { println!("Streaming: rtsp://{address}{}", args.path); }
                    println!("Resolution: {}x{} @ {:.3} FPS (configured)", negotiated.width, negotiated.height,
                        negotiated.fps_num as f64 / negotiated.fps_den as f64);
                    println!("Encoder: {} (hardware: {})", negotiated.encoder_name, negotiated.hardware_encoder);
                    println!("Press Ctrl+C to stop");
                    announced = true;
                }
                if (!announced && started.elapsed() > Duration::from_secs(15)) || stats.last_frame_age_ms.is_some_and(|age| age > 15_000) {
                    return Err("Camera stopped delivering H.264 frames. Check camera permissions, connection, and encoder availability.".into());
                }
            }
        })
    })();
    stopping.store(true, Ordering::Relaxed);
    let capture_result = capture.stop();
    let server_result = worker.join().map_err(|_| "RTSP server thread panicked")?;
    result?;
    capture_result?;
    server_result?;
    Ok(())
}

fn stream_addresses(bound: SocketAddr) -> Vec<SocketAddr> {
    if !bound.ip().is_unspecified() {
        return vec![bound];
    }
    let mut addresses = vec![SocketAddr::new(
        if bound.is_ipv4() {
            IpAddr::from([127, 0, 0, 1])
        } else {
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        },
        bound.port(),
    )];
    // UDP connect selects a local interface without sending traffic.
    let destination = if bound.is_ipv4() {
        "192.0.2.1:9"
    } else {
        "[2001:db8::1]:9"
    };
    if let Ok(socket) = UdpSocket::bind(SocketAddr::new(bound.ip(), 0))
        && socket.connect(destination).is_ok()
        && let Ok(local) = socket.local_addr()
        && !local.ip().is_loopback()
        && !local.ip().is_unspecified()
    {
        addresses.push(SocketAddr::new(local.ip(), bound.port()));
    }
    addresses
}

fn resolution(value: &str) -> Result<(u32, u32), String> {
    value
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .filter(|(w, h)| (1..=16384).contains(w) && (1..=16384).contains(h))
        .ok_or_else(|| "expected WIDTHxHEIGHT with each dimension in 1–16384".into())
}

fn bitrate(value: &str) -> Result<BitrateMode, String> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(BitrateMode::Auto);
    }
    value
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0 && *n <= i32::MAX as u32)
        .map(BitrateMode::BitsPerSecond)
        .ok_or_else(|| "expected auto or bitrate in 1–2147483647".into())
}

fn stream_path(value: &str) -> Result<String, String> {
    if camrtsp_server::valid_path(value) {
        Ok(value.to_string())
    } else {
        Err("expected an absolute RTSP path without whitespace, query, or fragment".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_arguments_are_rejected_before_camera_access() {
        for args in [
            vec!["camrtsp", "--fps", "oops"],
            vec!["camrtsp", "--fps", "0"],
            vec!["camrtsp", "--resolution", "0x720"],
            vec!["camrtsp", "--bitrate", "0"],
            vec!["camrtsp", "--gop", "61"],
            vec!["camrtsp", "--unknown"],
            vec!["camrtsp", "--path", "/bad path"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
        assert!(Args::try_parse_from(["camrtsp", "devices", "--json"]).is_ok());
    }
}
