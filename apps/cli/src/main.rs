use camrtsp_capture::NativeCamera;
use camrtsp_core::{AccessUnitSink, BitrateMode, CameraId, NativeVideoPipeline, StreamConfig};
use camrtsp_server::{Broadcaster, RtspServer, TransportPolicy};
use std::{env, process};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("devices") {
        let camera = NativeCamera::default();
        match camera.enumerate() {
            Ok(devices) if args.iter().any(|arg| arg == "--json") => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&devices)
                        .expect("camera descriptors are serializable")
                );
            }
            Ok(devices) => {
                for device in devices {
                    println!("{}: {}", device.id, device.name);
                }
            }
            Err(e) => {
                eprintln!("Unable to enumerate cameras: {e}");
                process::exit(1);
            }
        }
        return;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "camrtsp 0.1.0\n\nUsage: camrtsp devices [--json]\n       camrtsp --camera INDEX_OR_ID [--resolution WIDTHxHEIGHT] [--fps FPS] [--bitrate auto|BPS] [--gop SECONDS] [--bind ADDRESS] [--path /camera] [--transport both|tcp|udp] [--username USER --password PASS]\n\nStreams H.264 using native platform camera and encoder APIs."
        );
        return;
    }
    let bind = value_after(&args, "--bind").unwrap_or("0.0.0.0:8554");
    let requested_camera = value_after(&args, "--camera").unwrap_or("0");
    let devices = NativeCamera::default().enumerate().unwrap_or_else(|e| {
        eprintln!("Unable to enumerate cameras: {e}");
        process::exit(1)
    });
    let camera = requested_camera
        .parse::<usize>()
        .ok()
        .and_then(|index| devices.get(index))
        .map(|device| device.id.0.clone())
        .or_else(|| devices.iter().find(|device| device.id.0 == requested_camera).map(|device| device.id.0.clone()))
        .unwrap_or_else(|| {
            eprintln!("Camera '{requested_camera}' was not found. Run 'camrtsp devices' to list available cameras.");
            process::exit(2)
        });
    let fps = parse(&args, "--fps", 30_u32);
    let (width, height) = resolution(value_after(&args, "--resolution").unwrap_or("1280x720"));
    let config = StreamConfig {
        camera: CameraId(camera.clone()),
        requested_width: width,
        requested_height: height,
        requested_fps: fps,
        bitrate: bitrate(value_after(&args, "--bitrate").unwrap_or("auto")),
        gop_seconds: parse(&args, "--gop", 2_u32).max(1),
    };
    let supplied_password = value_after(&args, "--password")
        .map(str::to_owned)
        .or_else(|| env::var("CAMRTSP_PASSWORD").ok());
    let credentials = match (
        value_after(&args, "--username"),
        supplied_password.as_deref(),
    ) {
        (Some(username), Some(password)) => Some((username.to_string(), password.to_string())),
        (None, None) => None,
        _ => {
            eprintln!("--username and --password must be supplied together");
            process::exit(2)
        }
    };
    let path = value_after(&args, "--path").unwrap_or("/camera");
    if !path.starts_with('/') {
        eprintln!("--path must start with '/'");
        process::exit(2);
    }
    let transport = match value_after(&args, "--transport").unwrap_or("both") {
        "both" => TransportPolicy::Both,
        "tcp" => TransportPolicy::Tcp,
        "udp" => TransportPolicy::Udp,
        value => {
            eprintln!("Invalid transport '{value}'; use both, tcp, or udp");
            process::exit(2)
        }
    };
    let broadcaster = Broadcaster::default();
    let server = RtspServer::bind_with_transport(
        bind,
        path,
        broadcaster.clone(),
        fps,
        credentials,
        transport,
    )
    .unwrap_or_else(|e| {
        eprintln!("Unable to bind {bind}: {e}");
        process::exit(1)
    });
    let publisher = broadcaster.clone();
    let config_publisher = broadcaster.clone();
    let sink = AccessUnitSink::new(
        move |access_unit| publisher.publish(access_unit),
        move |codec_config| config_publisher.set_codec_config(codec_config),
    );
    let mut capture = NativeCamera::default();
    let negotiated = capture.start(config, sink).unwrap_or_else(|e| {
        eprintln!("Unable to start camera: {e}");
        process::exit(1)
    });
    println!("Camera: {camera}");
    println!(
        "Streaming: rtsp://127.0.0.1:{}{}",
        server.local_addr().unwrap().port(),
        path
    );
    println!(
        "Resolution: {}x{} @ {} FPS",
        negotiated.width, negotiated.height, negotiated.fps_num
    );
    println!("Codec: H.264");
    println!("Press Ctrl+C to stop");
    server.run().unwrap_or_else(|e| {
        eprintln!("Server error: {e}");
        process::exit(1)
    });
}

fn resolution(value: &str) -> (u32, u32) {
    value
        .split_once('x')
        .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)))
        .unwrap_or_else(|| {
            eprintln!("Invalid resolution '{value}'; expected WIDTHxHEIGHT");
            process::exit(2)
        })
}

fn parse<T: std::str::FromStr>(args: &[String], flag: &str, default: T) -> T {
    value_after(args, flag)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn bitrate(value: &str) -> BitrateMode {
    if value.eq_ignore_ascii_case("auto") {
        return BitrateMode::Auto;
    }
    value
        .parse::<u32>()
        .map(BitrateMode::BitsPerSecond)
        .unwrap_or_else(|_| {
            eprintln!("Invalid bitrate '{value}'; use auto or bits per second");
            process::exit(2)
        })
}

fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}
