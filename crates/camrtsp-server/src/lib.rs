use base64::{Engine, engine::general_purpose::STANDARD};
use camrtsp_core::{CodecConfig, EncodedAccessUnit};
use camrtsp_rtp::{packetize_h264, rtp_packet};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

const MAX_QUEUED_ACCESS_UNITS: usize = 16;
const RTP_MTU: usize = 1_400;
const MAX_REQUEST_BYTES: usize = 65_536;
const MAX_CONNECTIONS: usize = 128;
const NONCE_TTL: Duration = Duration::from_secs(300);

#[derive(Default)]
struct FrameQueue {
    frames: VecDeque<EncodedAccessUnit>,
    waiting_for_keyframe: bool,
}

struct Subscriber {
    queue: Arc<(Mutex<FrameQueue>, Condvar)>,
    alive: Arc<AtomicBool>,
}

#[derive(Default)]
struct BroadcastState {
    clients: Vec<Subscriber>,
    codec: Option<CodecConfig>,
    frame_rate: Option<(u32, u32)>,
    keyframe_requested: bool,
    frames: u64,
    last_frame: Option<Instant>,
}

#[derive(Clone, Default)]
pub struct Broadcaster(Arc<Mutex<BroadcastState>>);

pub struct Subscription {
    queue: Arc<(Mutex<FrameQueue>, Condvar)>,
    alive: Arc<AtomicBool>,
}

impl Subscription {
    fn recv_timeout(&self, timeout: Duration) -> Option<EncodedAccessUnit> {
        let (queue, changed) = &*self.queue;
        let queue = queue.lock().expect("subscriber lock poisoned");
        let (mut queue, _) = changed
            .wait_timeout_while(queue, timeout, |queue| queue.frames.is_empty())
            .expect("subscriber lock poisoned");
        queue.frames.pop_front()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

impl Broadcaster {
    pub fn subscribe(&self) -> Subscription {
        let queue = Arc::new((
            Mutex::new(FrameQueue {
                frames: VecDeque::with_capacity(MAX_QUEUED_ACCESS_UNITS),
                waiting_for_keyframe: true,
            }),
            Condvar::new(),
        ));
        let alive = Arc::new(AtomicBool::new(true));
        let mut state = self.0.lock().expect("broadcaster lock poisoned");
        state
            .clients
            .retain(|client| client.alive.load(Ordering::Relaxed));
        state.keyframe_requested = true;
        state.clients.push(Subscriber {
            queue: queue.clone(),
            alive: alive.clone(),
        });
        Subscription { queue, alive }
    }

    pub fn set_codec_config(&self, codec: CodecConfig) {
        let mut state = self.0.lock().expect("broadcaster lock poisoned");
        if state
            .codec
            .as_ref()
            .is_some_and(|old| old.sps != codec.sps || old.pps != codec.pps)
        {
            for client in &state.clients {
                let mut queue = client.queue.0.lock().expect("subscriber lock poisoned");
                queue.frames.clear();
                queue.waiting_for_keyframe = true;
            }
            state.keyframe_requested = true;
        }
        state.codec = Some(codec);
    }

    pub fn codec_config(&self) -> Option<CodecConfig> {
        self.0
            .lock()
            .expect("broadcaster lock poisoned")
            .codec
            .clone()
    }

    pub fn set_frame_rate(&self, numerator: u32, denominator: u32) {
        self.0.lock().expect("broadcaster lock poisoned").frame_rate =
            (numerator > 0 && denominator > 0).then_some((numerator, denominator));
    }

    pub fn take_keyframe_request(&self) -> bool {
        std::mem::take(
            &mut self
                .0
                .lock()
                .expect("broadcaster lock poisoned")
                .keyframe_requested,
        )
    }

    pub fn stats(&self) -> StreamStats {
        let state = self.0.lock().expect("broadcaster lock poisoned");
        StreamStats {
            viewers: state
                .clients
                .iter()
                .filter(|client| client.alive.load(Ordering::Relaxed))
                .count(),
            frames: state.frames,
            ready: state.codec.is_some() && state.last_frame.is_some(),
            last_frame_age_ms: state
                .last_frame
                .map(|time| time.elapsed().as_millis() as u64),
        }
    }

    pub fn publish(&self, mut access_unit: EncodedAccessUnit) {
        let mut state = self.0.lock().expect("broadcaster lock poisoned");
        state.frames = state.frames.saturating_add(1);
        state.last_frame = Some(Instant::now());
        access_unit.keyframe |= access_unit
            .nal_units
            .iter()
            .any(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 5));
        if access_unit.keyframe
            && let Some(codec) = &state.codec
        {
            // Parameter sets accompany every recovery point, including late joins.
            if !access_unit
                .nal_units
                .iter()
                .any(|nal| nal.first().is_some_and(|b| b & 0x1f == 8))
            {
                access_unit.nal_units.insert(0, codec.pps.clone());
            }
            if !access_unit
                .nal_units
                .iter()
                .any(|nal| nal.first().is_some_and(|b| b & 0x1f == 7))
            {
                access_unit.nal_units.insert(0, codec.sps.clone());
            }
        }
        let mut request_keyframe = false;
        state.clients.retain(|client| {
            if !client.alive.load(Ordering::Relaxed) {
                return false;
            }
            let mut queue = client.queue.0.lock().expect("subscriber lock poisoned");
            if queue.frames.len() == MAX_QUEUED_ACCESS_UNITS {
                queue.frames.clear();
                queue.waiting_for_keyframe = true;
                request_keyframe = true;
            }
            if access_unit.keyframe {
                queue.waiting_for_keyframe = false;
            }
            if !queue.waiting_for_keyframe {
                queue.frames.push_back(access_unit.clone());
                client.queue.1.notify_one();
            }
            true
        });
        state.keyframe_requested |= request_keyframe;
    }
}

#[derive(Debug, serde::Serialize)]
pub struct StreamStats {
    pub viewers: usize,
    pub frames: u64,
    pub ready: bool,
    pub last_frame_age_ms: Option<u64>,
}

pub struct RtspServer {
    listener: TcpListener,
    path: String,
    source: Broadcaster,
    credentials: Option<(String, String)>,
    transport: TransportPolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransportPolicy {
    Tcp,
    Udp,
    #[default]
    Both,
}

impl RtspServer {
    pub fn bind(
        address: &str,
        path: impl Into<String>,
        source: Broadcaster,
        fps: u32,
        credentials: Option<(String, String)>,
    ) -> io::Result<Self> {
        Self::bind_with_transport(
            address,
            path,
            source,
            fps,
            credentials,
            TransportPolicy::Both,
        )
    }

    pub fn bind_with_transport(
        address: &str,
        path: impl Into<String>,
        source: Broadcaster,
        fps: u32,
        credentials: Option<(String, String)>,
        transport: TransportPolicy,
    ) -> io::Result<Self> {
        let path = path.into();
        if !valid_path(&path)
            || credentials
                .as_ref()
                .is_some_and(|(u, p)| u.is_empty() || p.is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid stream path or incomplete credentials",
            ));
        }
        source.set_frame_rate(fps, 1);
        Ok(Self {
            listener: TcpListener::bind(address)?,
            path,
            source,
            credentials,
            transport,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn run(self) -> io::Result<()> {
        self.run_until(Arc::new(AtomicBool::new(false)))
    }

    pub fn run_until(self, stopping: Arc<AtomicBool>) -> io::Result<()> {
        self.listener.set_nonblocking(true)?;
        let mut sessions: Vec<(TcpStream, thread::JoinHandle<()>)> = Vec::new();
        let result = loop {
            if stopping.load(Ordering::Relaxed) {
                break Ok(());
            }
            let mut index = 0;
            while index < sessions.len() {
                if sessions[index].1.is_finished() {
                    let (_, worker) = sessions.swap_remove(index);
                    let _ = worker.join();
                } else {
                    index += 1;
                }
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if sessions.len() >= MAX_CONNECTIONS {
                        continue;
                    }
                    let socket = match stream.try_clone() {
                        Ok(socket) => socket,
                        Err(error) => break Err(error),
                    };
                    let path = self.path.clone();
                    let source = self.source.clone();
                    let credentials = self.credentials.clone();
                    let transport = self.transport;
                    match thread::Builder::new()
                        .name("camrtsp-client".into())
                        .spawn(move || {
                            if let Err(error) =
                                handle(stream, &path, source, credentials.as_ref(), transport)
                                && !matches!(
                                    error.kind(),
                                    io::ErrorKind::UnexpectedEof
                                        | io::ErrorKind::ConnectionReset
                                        | io::ErrorKind::BrokenPipe
                                )
                            {
                                eprintln!("RTSP session error: {error}");
                            }
                        }) {
                        Ok(worker) => sessions.push((socket, worker)),
                        Err(error) => break Err(error),
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => break Err(error),
            }
        };
        for (socket, _) in &sessions {
            let _ = socket.shutdown(Shutdown::Both);
        }
        for (_, worker) in sessions {
            let _ = worker.join();
        }
        result
    }

    pub fn run_blocking(self) -> io::Result<()> {
        self.run()
    }
}

pub fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains(['?', '#'])
        && path.bytes().all(|byte| byte.is_ascii_graphic())
        && !path.contains("//")
}

fn request_path(uri: &str) -> Option<&str> {
    let path = if let Some(rest) = uri.strip_prefix("rtsp://") {
        &rest[rest.find('/')?..]
    } else if uri.starts_with('/') {
        uri
    } else {
        return None;
    };
    Some(path.split('?').next().unwrap_or(path))
}

#[derive(Debug)]
struct Request {
    method: String,
    uri: String,
    headers: HashMap<String, String>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
    fn cseq(&self) -> &str {
        self.header("cseq").unwrap_or("1")
    }
}

struct AuthState {
    nonce: String,
    issued: Instant,
}

enum Delivery {
    Tcp {
        channels: (u8, u8),
    },
    Udp {
        rtp: UdpSocket,
        rtcp: UdpSocket,
        client_rtp: SocketAddr,
        client_rtcp: SocketAddr,
    },
}

impl Delivery {
    fn transport_header(&self) -> io::Result<String> {
        match self {
            Self::Tcp { channels } => Ok(format!(
                "RTP/AVP/TCP;unicast;interleaved={}-{}",
                channels.0, channels.1
            )),
            Self::Udp {
                rtp,
                rtcp,
                client_rtp,
                client_rtcp,
            } => Ok(format!(
                "RTP/AVP;unicast;client_port={}-{};server_port={}-{};source={}",
                client_rtp.port(),
                client_rtcp.port(),
                rtp.local_addr()?.port(),
                rtcp.local_addr()?.port(),
                rtp.local_addr()?.ip()
            )),
        }
    }

    fn send_rtp(&mut self, stream: &mut TcpStream, packet: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp { channels } => {
                stream.write_all(&[
                    b'$',
                    channels.0,
                    (packet.len() >> 8) as u8,
                    packet.len() as u8,
                ])?;
                stream.write_all(packet)
            }
            Self::Udp {
                rtp, client_rtp, ..
            } => {
                rtp.send_to(packet, *client_rtp)?;
                Ok(())
            }
        }
    }

    fn send_rtcp(&mut self, stream: &mut TcpStream, packet: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp { channels } => {
                stream.write_all(&[
                    b'$',
                    channels.1,
                    (packet.len() >> 8) as u8,
                    packet.len() as u8,
                ])?;
                stream.write_all(packet)
            }
            Self::Udp {
                rtcp, client_rtcp, ..
            } => {
                rtcp.send_to(packet, *client_rtcp)?;
                Ok(())
            }
        }
    }
}

fn handle(
    mut stream: TcpStream,
    path: &str,
    source: Broadcaster,
    credentials: Option<&(String, String)>,
    transport_policy: TransportPolicy,
) -> io::Result<()> {
    // Blocking writes preserve partially written interleaved frames. A slow reader
    // gets a bounded write deadline and its own queue, never a shared capture lock.
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.set_nodelay(true)?;
    let peer = stream.peer_addr()?;
    let local = stream.local_addr()?;
    let mut input = Vec::new();
    let mut delivery: Option<Delivery> = None;
    let mut subscription: Option<Subscription> = None;
    let mut auth = AuthState {
        nonce: random_hex(24),
        issued: Instant::now(),
    };
    let session_id = random_hex(12);
    let session_header = format!("Session: {session_id}\r\n");
    let mut sequence = rand::rng().random::<u16>();
    let ssrc = rand::rng().random::<u32>();
    let mut packets = 0_u32;
    let mut octets = 0_u32;
    let mut last_timestamp = None;
    let mut last_report = Instant::now();
    let mut last_control = Instant::now();
    loop {
        if let Some(request) = read_control(&mut stream, &mut input)? {
            last_control = Instant::now();
            if request.method != "OPTIONS" && !authorized(&request, credentials, &auth) {
                if auth.issued.elapsed() >= NONCE_TTL {
                    auth = AuthState {
                        nonce: random_hex(24),
                        issued: Instant::now(),
                    };
                }
                let challenge = format!(
                    "WWW-Authenticate: Digest realm=\"camrtsp\", nonce=\"{}\", algorithm=SHA-256, qop=\"auth\"\r\nWWW-Authenticate: Digest realm=\"camrtsp\", nonce=\"{}\", algorithm=MD5, qop=\"auth\"\r\nWWW-Authenticate: Basic realm=\"camrtsp\"\r\n",
                    auth.nonce, auth.nonce
                );
                respond(
                    &mut stream,
                    request.cseq(),
                    "401 Unauthorized",
                    &challenge,
                    "",
                )?;
                continue;
            }
            let aggregate = if path == "/" {
                "/"
            } else {
                path.trim_end_matches('/')
            };
            let aggregate_with_slash = if aggregate == "/" {
                "/".to_string()
            } else {
                format!("{aggregate}/")
            };
            let track = format!("{aggregate_with_slash}trackID=0");
            let target = request_path(&request.uri);
            if !(request.method == "OPTIONS" && request.uri == "*")
                && target != Some(aggregate)
                && target != Some(aggregate_with_slash.as_str())
                && target != Some(track.as_str())
            {
                respond(&mut stream, request.cseq(), "404 Not Found", "", "")?;
                continue;
            }
            let supplied_session = request
                .header("session")
                .map(|s| s.split(';').next().unwrap_or(s).trim());
            if supplied_session.is_some_and(|s| s != session_id)
                || (matches!(request.method.as_str(), "PLAY" | "PAUSE" | "TEARDOWN")
                    && delivery.is_some()
                    && supplied_session.is_none())
            {
                respond(&mut stream, request.cseq(), "454 Session Not Found", "", "")?;
                continue;
            }
            match request.method.as_str() {
                "OPTIONS" => respond(
                    &mut stream,
                    request.cseq(),
                    "200 OK",
                    "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, GET_PARAMETER, TEARDOWN\r\n",
                    "",
                )?,
                "DESCRIBE" => {
                    if let Some(codec) = source.codec_config() {
                        let rate = source
                            .0
                            .lock()
                            .expect("broadcaster lock poisoned")
                            .frame_rate;
                        let body = sdp(rate, &codec);
                        let base = if let Some(rest) = request.uri.strip_prefix("rtsp://") {
                            let authority = rest
                                .split('/')
                                .next()
                                .unwrap_or_default()
                                .rsplit('@')
                                .next()
                                .unwrap_or_default();
                            format!("rtsp://{authority}{}", path.trim_end_matches('/'))
                        } else {
                            format!("rtsp://{local}{}", path.trim_end_matches('/'))
                        };
                        respond(
                            &mut stream,
                            request.cseq(),
                            "200 OK",
                            &format!("Content-Type: application/sdp\r\nContent-Base: {base}/\r\n"),
                            &body,
                        )?;
                    } else {
                        respond(
                            &mut stream,
                            request.cseq(),
                            "503 Service Unavailable",
                            "Retry-After: 1\r\n",
                            "",
                        )?;
                    }
                }
                "SETUP" if subscription.is_none() => {
                    match request.header("transport").and_then(|v| {
                        negotiate_transport(v, peer.ip(), local.ip(), transport_policy).ok()
                    }) {
                        Some(selected) => {
                            let header = format!(
                                "Transport: {}\r\n{session_header}",
                                selected.transport_header()?
                            );
                            delivery = Some(selected);
                            respond(&mut stream, request.cseq(), "200 OK", &header, "")?;
                        }
                        None => respond(
                            &mut stream,
                            request.cseq(),
                            "461 Unsupported Transport",
                            "",
                            "",
                        )?,
                    }
                }
                "PLAY" if delivery.is_some() => {
                    if subscription.is_none() {
                        subscription = Some(source.subscribe());
                    }
                    respond(
                        &mut stream,
                        request.cseq(),
                        "200 OK",
                        &format!("{session_header}Range: npt=now-\r\n"),
                        "",
                    )?;
                }
                "PAUSE" if subscription.is_some() => {
                    subscription = None;
                    respond(&mut stream, request.cseq(), "200 OK", &session_header, "")?;
                }
                "GET_PARAMETER" => {
                    respond(&mut stream, request.cseq(), "200 OK", &session_header, "")?
                }
                "TEARDOWN" if delivery.is_some() => {
                    respond(&mut stream, request.cseq(), "200 OK", &session_header, "")?;
                    if let Some(selected) = &mut delivery {
                        let _ = selected.send_rtcp(&mut stream, &rtcp_bye(ssrc));
                    }
                    return Ok(());
                }
                _ => respond(
                    &mut stream,
                    request.cseq(),
                    "455 Method Not Valid in This State",
                    "",
                    "",
                )?,
            }
        }
        if subscription.is_none() {
            thread::sleep(Duration::from_millis(5));
        }
        if subscription.is_none() && last_control.elapsed() > Duration::from_secs(30) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "idle RTSP session"));
        }
        if let (Some(subscription), Some(delivery)) = (&subscription, &mut delivery) {
            if let Some(access_unit) = subscription.recv_timeout(Duration::from_millis(10)) {
                for (nal_index, nal) in access_unit.nal_units.iter().enumerate() {
                    let payloads = packetize_h264(nal, RTP_MTU);
                    for (index, payload) in payloads.iter().enumerate() {
                        let marker = nal_index + 1 == access_unit.nal_units.len()
                            && index + 1 == payloads.len();
                        let packet =
                            rtp_packet(payload, sequence, access_unit.pts_90khz, ssrc, marker);
                        delivery.send_rtp(&mut stream, &packet)?;
                        sequence = sequence.wrapping_add(1);
                        packets = packets.wrapping_add(1);
                        octets = octets.wrapping_add(payload.len() as u32);
                    }
                }
                last_timestamp = Some((access_unit.pts_90khz, Instant::now()));
            }
            if last_report.elapsed() >= Duration::from_secs(5)
                && let Some((timestamp, sent)) = last_timestamp
            {
                let timestamp =
                    timestamp.wrapping_add((sent.elapsed().as_micros() * 90 / 1000) as u32);
                delivery.send_rtcp(
                    &mut stream,
                    &rtcp_sender_report(ssrc, timestamp, packets, octets),
                )?;
                last_report = Instant::now();
            }
        }
    }
}

// TCP can contain partial/pipelined RTSP messages and interleaved binary RTCP.
fn take_request(input: &mut Vec<u8>) -> io::Result<Option<Request>> {
    loop {
        if input.first() == Some(&b'$') {
            if input.len() < 4 {
                return Ok(None);
            }
            let end = 4 + u16::from_be_bytes([input[2], input[3]]) as usize;
            if end > MAX_REQUEST_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "interleaved frame too large",
                ));
            }
            if input.len() < end {
                return Ok(None);
            }
            input.drain(..end);
            continue;
        }
        let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") else {
            if input.len() >= MAX_REQUEST_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RTSP headers too large",
                ));
            }
            return Ok(None);
        };
        let header_end = end + 4;
        let headers = std::str::from_utf8(&input[..header_end])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid RTSP headers"))?;
        let body_length = match header_value(headers, "content-length") {
            Some(value) => value.parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid content length")
            })?,
            None => 0,
        };
        let total = header_end
            .checked_add(body_length)
            .filter(|n| *n <= MAX_REQUEST_BYTES)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "RTSP request too large"))?;
        if input.len() < total {
            return Ok(None);
        }
        let request = parse_request(headers)?;
        input.drain(..total);
        return Ok(Some(request));
    }
}

fn read_control(stream: &mut TcpStream, input: &mut Vec<u8>) -> io::Result<Option<Request>> {
    if let Some(request) = take_request(input)? {
        return Ok(Some(request));
    }
    let mut chunk = [0_u8; 4096];
    stream.set_nonblocking(true)?;
    let result = stream.read(&mut chunk);
    stream.set_nonblocking(false)?;
    match result {
        Ok(0) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "RTSP client disconnected",
        )),
        Ok(count) => {
            input.extend_from_slice(&chunk[..count]);
            take_request(input)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn parse_request(raw: &str) -> io::Result<Request> {
    let mut lines = raw.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing RTSP method"))?;
    let uri = request_line
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing RTSP URI"))?;
    if request_line.next() != Some("RTSP/1.0") || request_line.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported RTSP version",
        ));
    }
    let mut headers = HashMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed RTSP header"))?;
        if headers
            .insert(key.trim().to_ascii_lowercase(), value.trim().to_string())
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "duplicate RTSP header",
            ));
        }
    }
    Ok(Request {
        method: method.to_ascii_uppercase(),
        uri: uri.to_string(),
        headers,
    })
}

fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.split("\r\n").find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then_some(value.trim())
    })
}

fn respond(
    stream: &mut TcpStream,
    cseq: &str,
    status: &str,
    headers: &str,
    body: &str,
) -> io::Result<()> {
    stream.write_all(
        format!(
            "RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

fn sdp(rate: Option<(u32, u32)>, codec: &CodecConfig) -> String {
    let framerate = rate
        .map(|(n, d)| format!("a=framerate:{}\r\n", n as f64 / d as f64))
        .unwrap_or_default();
    format!(
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=camrtsp\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\na=control:*\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1;profile-level-id={:02x}{:02x}{:02x};sprop-parameter-sets={},{}\r\n{framerate}a=control:trackID=0\r\n",
        codec.profile_level_id[0],
        codec.profile_level_id[1],
        codec.profile_level_id[2],
        STANDARD.encode(&codec.sps),
        STANDARD.encode(&codec.pps)
    )
}

fn negotiate_transport(
    value: &str,
    client_ip: IpAddr,
    local_ip: IpAddr,
    policy: TransportPolicy,
) -> Result<Delivery, ()> {
    for alternative in value.split(',') {
        let lower = alternative.to_ascii_lowercase();
        if lower.split(';').any(|part| part.trim() == "multicast") {
            continue;
        }
        let protocol = lower.split(';').next().unwrap_or_default().trim();
        if protocol == "rtp/avp/tcp" && policy != TransportPolicy::Udp {
            let channels = match parameter(&lower, "interleaved") {
                None => (0, 1),
                Some(value) => match parse_pair(value) {
                    Some((a, b)) if a != b && a <= 255 && b <= 255 => (a as u8, b as u8),
                    _ => continue,
                },
            };
            return Ok(Delivery::Tcp { channels });
        }
        if matches!(protocol, "rtp/avp" | "rtp/avp/udp") && policy != TransportPolicy::Tcp {
            let Some((rtp_port, rtcp_port)) = parameter(&lower, "client_port").and_then(parse_pair)
            else {
                continue;
            };
            if rtp_port == 0 || rtcp_port == 0 || rtp_port == rtcp_port {
                continue;
            }
            let (rtp, rtcp) = bind_udp_pair(local_ip).map_err(|_| ())?;
            return Ok(Delivery::Udp {
                rtp,
                rtcp,
                client_rtp: SocketAddr::new(client_ip, rtp_port),
                client_rtcp: SocketAddr::new(client_ip, rtcp_port),
            });
        }
    }
    Err(())
}

fn parameter<'a>(value: &'a str, key: &str) -> Option<&'a str> {
    value
        .split(';')
        .find_map(|part| part.trim().strip_prefix(&format!("{key}=")))
}

fn parse_pair(value: &str) -> Option<(u16, u16)> {
    let (first, second) = value.split_once('-')?;
    Some((first.parse().ok()?, second.parse().ok()?))
}

fn bind_udp_pair(local_ip: IpAddr) -> io::Result<(UdpSocket, UdpSocket)> {
    for port in (50_000..60_000).step_by(2) {
        let rtp = match UdpSocket::bind((local_ip, port)) {
            Ok(socket) => socket,
            Err(_) => continue,
        };
        if let Ok(rtcp) = UdpSocket::bind((local_ip, port + 1)) {
            return Ok((rtp, rtcp));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "no UDP RTP/RTCP ports available",
    ))
}

fn authorized(request: &Request, credentials: Option<&(String, String)>, auth: &AuthState) -> bool {
    let Some((username, password)) = credentials else {
        return true;
    };
    let Some(value) = request.header("authorization") else {
        return false;
    };
    if let Some(token) = value.strip_prefix("Basic ") {
        let Ok(decoded) = STANDARD.decode(token) else {
            return false;
        };
        return format!("{username}:{password}")
            .as_bytes()
            .ct_eq(&decoded)
            .into();
    }
    let Some(parameters) = value.strip_prefix("Digest ") else {
        return false;
    };
    if auth.issued.elapsed() > NONCE_TTL {
        return false;
    }
    let values = digest_parameters(parameters);
    if values.get("username").map(String::as_str) != Some(username.as_str())
        || values.get("realm").map(String::as_str) != Some("camrtsp")
        || values.get("nonce").map(String::as_str) != Some(auth.nonce.as_str())
        || values.get("uri").map(String::as_str) != Some(request.uri.as_str())
    {
        return false;
    }
    let algorithm = values.get("algorithm").map(String::as_str).unwrap_or("MD5");
    if !algorithm.eq_ignore_ascii_case("SHA-256") && !algorithm.eq_ignore_ascii_case("MD5") {
        return false;
    }
    let hash = |value: String| {
        if algorithm.eq_ignore_ascii_case("SHA-256") {
            format!("{:x}", Sha256::digest(value.as_bytes()))
        } else {
            format!("{:x}", md5::compute(value))
        }
    };
    let ha1 = hash(format!("{username}:camrtsp:{password}"));
    let ha2 = hash(format!("{}:{}", request.method, request.uri));
    let expected = match (values.get("qop"), values.get("nc"), values.get("cnonce")) {
        (Some(qop), Some(nc), Some(cnonce))
            if qop == "auth"
                && nc.len() == 8
                && u32::from_str_radix(nc, 16).is_ok_and(|n| n > 0)
                && !cnonce.is_empty() =>
        {
            hash(format!("{ha1}:{}:{nc}:{cnonce}:{qop}:{ha2}", auth.nonce))
        }
        (None, None, None) => hash(format!("{ha1}:{}:{ha2}", auth.nonce)),
        _ => return false,
    };
    values
        .get("response")
        .is_some_and(|response| expected.as_bytes().ct_eq(response.as_bytes()).into())
}

fn digest_parameters(value: &str) -> HashMap<String, String> {
    value
        .split(',')
        .filter_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            Some((
                key.trim().to_ascii_lowercase(),
                value.trim().trim_matches('"').to_string(),
            ))
        })
        .collect()
}

fn random_hex(bytes: usize) -> String {
    let mut data = vec![0_u8; bytes];
    rand::rng().fill(data.as_mut_slice());
    data.iter().map(|value| format!("{value:02x}")).collect()
}

fn rtcp_sender_report(ssrc: u32, rtp_timestamp: u32, packets: u32, octets: u32) -> Vec<u8> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let ntp_seconds = now.as_secs().wrapping_add(2_208_988_800) as u32;
    let ntp_fraction = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    let cname = b"camrtsp";
    let mut packet = Vec::with_capacity(44);
    packet.extend_from_slice(&[0x80, 200, 0, 6]);
    packet.extend_from_slice(&ssrc.to_be_bytes());
    packet.extend_from_slice(&ntp_seconds.to_be_bytes());
    packet.extend_from_slice(&(ntp_fraction as u32).to_be_bytes());
    packet.extend_from_slice(&rtp_timestamp.to_be_bytes());
    packet.extend_from_slice(&packets.to_be_bytes());
    packet.extend_from_slice(&octets.to_be_bytes());
    packet.extend_from_slice(&[0x81, 202, 0, 4]);
    packet.extend_from_slice(&ssrc.to_be_bytes());
    packet.extend_from_slice(&[1, cname.len() as u8]);
    packet.extend_from_slice(cname);
    packet.extend_from_slice(&[0, 0, 0]);
    packet
}

fn rtcp_bye(ssrc: u32) -> Vec<u8> {
    [vec![0x81, 203, 0, 1], ssrc.to_be_bytes().to_vec()].concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn overflow_waits_for_a_new_keyframe_and_tracks_viewers() {
        let broadcaster = Broadcaster::default();
        let subscription = broadcaster.subscribe();
        assert_eq!(broadcaster.stats().viewers, 1);
        for timestamp in 0..18 {
            broadcaster.publish(EncodedAccessUnit {
                nal_units: vec![Bytes::from_static(&[0x41])],
                pts_90khz: timestamp,
                duration_90khz: 3000,
                keyframe: timestamp == 0,
            });
        }
        assert!(subscription.recv_timeout(Duration::ZERO).is_none());
        assert!(broadcaster.take_keyframe_request());
        broadcaster.publish(EncodedAccessUnit {
            nal_units: vec![Bytes::from_static(&[0x65])],
            pts_90khz: 18,
            duration_90khz: 3000,
            keyframe: false,
        });
        assert_eq!(
            subscription.recv_timeout(Duration::ZERO).unwrap().pts_90khz,
            18
        );
        drop(subscription);
        assert_eq!(broadcaster.stats().viewers, 0);
    }

    #[test]
    fn sdp_uses_live_codec_configuration() {
        let codec = CodecConfig {
            sps: Bytes::from_static(&[0x67, 0x4d, 0, 0x1f]),
            pps: Bytes::from_static(&[0x68, 0xee]),
            profile_level_id: [0x4d, 0, 0x1f],
        };
        let value = sdp(Some((30, 1)), &codec);
        assert!(value.contains("profile-level-id=4d001f"));
        assert!(value.contains("sprop-parameter-sets=Z00AHw==,aO4="));
    }

    #[test]
    fn accepts_rfc_digest_md5_authentication() {
        let uri = "rtsp://127.0.0.1:8554/camera";
        let nonce = "nonce";
        let digest = |value: &str| format!("{:x}", md5::compute(value));
        let ha1 = digest("admin:camrtsp:secret");
        let ha2 = digest(&format!("DESCRIBE:{uri}"));
        let response = digest(&format!("{ha1}:{nonce}:00000001:client:auth:{ha2}"));
        let request = Request {
            method: "DESCRIBE".into(),
            uri: uri.into(),
            headers: HashMap::from([(
                "authorization".into(),
                format!(
                    "Digest username=\"admin\", realm=\"camrtsp\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\", algorithm=MD5, qop=auth, nc=00000001, cnonce=\"client\""
                ),
            )]),
        };
        let auth = AuthState {
            nonce: nonce.into(),
            issued: Instant::now(),
        };
        assert!(authorized(
            &request,
            Some(&("admin".into(), "secret".into())),
            &auth
        ));
    }

    #[test]
    fn tcp_session_delivers_interleaved_rtp() {
        let broadcaster = Broadcaster::default();
        let server = RtspServer::bind_with_transport(
            "127.0.0.1:0",
            "/camera",
            broadcaster.clone(),
            30,
            None,
            TransportPolicy::Tcp,
        )
        .expect("RTSP bind");
        let addr = server.local_addr().expect("local address");
        let stopping = Arc::new(AtomicBool::new(false));
        let server_stop = stopping.clone();
        let server_thread = thread::spawn(move || server.run_until(server_stop));

        broadcaster.set_codec_config(CodecConfig {
            sps: Bytes::from_static(&[0x67, 0x4d, 0, 0x1f]),
            pps: Bytes::from_static(&[0x68, 0xee]),
            profile_level_id: [0x4d, 0, 0x1f],
        });
        let publishing = Arc::new(AtomicBool::new(true));
        let publish_flag = publishing.clone();
        let publisher = broadcaster.clone();
        let publish_thread = thread::spawn(move || {
            let mut pts = 0_u32;
            while publish_flag.load(Ordering::Relaxed) {
                publisher.publish(EncodedAccessUnit {
                    nal_units: vec![Bytes::from_static(&[0x65, 1, 2, 3, 4, 5])],
                    pts_90khz: pts,
                    duration_90khz: 3000,
                    keyframe: true,
                });
                pts = pts.wrapping_add(3000);
                thread::sleep(Duration::from_millis(20));
            }
        });

        let mut stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("client connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("write timeout");

        let options = rtsp_exchange(
            &mut stream,
            "OPTIONS rtsp://127.0.0.1/camera RTSP/1.0\r\nCSeq: 1\r\n\r\n",
        );
        assert!(
            options.starts_with("RTSP/1.0 200 OK"),
            "unexpected OPTIONS: {options}"
        );

        let describe = rtsp_exchange(
            &mut stream,
            "DESCRIBE rtsp://127.0.0.1/camera RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n",
        );
        assert!(
            describe.contains("RTSP/1.0 200 OK"),
            "unexpected DESCRIBE: {describe}"
        );
        assert!(
            describe.contains("sprop-parameter-sets=Z00AHw==,aO4="),
            "missing SDP parameter sets: {describe}"
        );

        let setup = rtsp_exchange(
            &mut stream,
            "SETUP rtsp://127.0.0.1/camera/trackID=0 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
        );
        assert!(
            setup.contains("RTSP/1.0 200 OK"),
            "unexpected SETUP: {setup}"
        );
        let session = setup
            .lines()
            .find_map(|line| line.strip_prefix("Session: "))
            .expect("SETUP Session header")
            .split(';')
            .next()
            .unwrap()
            .trim()
            .to_string();

        stream
            .write_all(
                format!(
                    "PLAY rtsp://127.0.0.1/camera RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("PLAY write");

        let mut buf = Vec::new();
        let mut rtp_packets = 0_usize;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_play = false;
        while Instant::now() < deadline && rtp_packets == 0 {
            let mut chunk = [0_u8; 4096];
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => buf.extend_from_slice(&chunk[..count]),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(error) => panic!("PLAY read failed: {error}"),
            }
            if !saw_play {
                if let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&buf[..end]);
                    assert!(
                        header.contains("RTSP/1.0 200 OK"),
                        "unexpected PLAY: {header}"
                    );
                    buf.drain(..end + 4);
                    saw_play = true;
                }
                continue;
            }
            while buf.len() >= 4 && buf[0] == b'$' {
                let length = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                if buf.len() < 4 + length {
                    break;
                }
                if buf[1] == 0 {
                    rtp_packets += 1;
                }
                buf.drain(..4 + length);
            }
        }

        publishing.store(false, Ordering::Relaxed);
        stopping.store(true, Ordering::Relaxed);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        let _ = publish_thread.join();
        let _ = server_thread.join();
        assert!(saw_play, "PLAY response was not received");
        assert!(rtp_packets > 0, "expected interleaved RTP packets");
    }

    #[test]
    fn parser_handles_fragmented_binary_frames_bodies_and_pipelining() {
        let mut input = vec![b'$', 1, 0];
        assert!(take_request(&mut input).unwrap().is_none());
        input.extend_from_slice(&[8, 0x80, 201, 0, 1, 0, 0, 0, 1]);
        input.extend_from_slice(
            b"GET_PARAMETER rtsp://host/camera RTSP/1.0\r\nCSeq: 8\r\nContent-Length: 4\r\n\r\nab",
        );
        assert!(take_request(&mut input).unwrap().is_none());
        input.extend_from_slice(b"cdOPTIONS * RTSP/1.0\r\nCSeq: 9\r\n\r\n");
        assert_eq!(take_request(&mut input).unwrap().unwrap().cseq(), "8");
        assert_eq!(take_request(&mut input).unwrap().unwrap().cseq(), "9");
        assert!(input.is_empty());
        for length in ["18446744073709551615", "65536", "-1", "oops"] {
            let mut input =
                format!("GET_PARAMETER /camera RTSP/1.0\r\nContent-Length: {length}\r\n\r\n")
                    .into_bytes();
            assert!(take_request(&mut input).is_err());
        }
        let mut input = vec![b'a'; MAX_REQUEST_BYTES];
        assert!(take_request(&mut input).is_err());
        assert!(
            parse_request("OPTIONS * RTSP/1.0\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\n")
                .is_err()
        );
    }

    #[test]
    fn transport_policy_rejects_invalid_channels_ports_and_multicast() {
        let ip = IpAddr::from([127, 0, 0, 1]);
        for value in [
            "RTP/AVP/TCP;interleaved=256-257",
            "RTP/AVP/TCP;interleaved=1-1",
            "RTP/AVP;client_port=0-1",
            "RTP/AVP;multicast;client_port=6000-6001",
            "bogusRTP/AVP/TCP",
        ] {
            assert!(
                negotiate_transport(value, ip, ip, TransportPolicy::Both).is_err(),
                "{value}"
            );
        }
        assert!(negotiate_transport("RTP/AVP/TCP", ip, ip, TransportPolicy::Udp).is_err());
        assert!(
            negotiate_transport(
                "RTP/AVP;client_port=6000-6001",
                ip,
                ip,
                TransportPolicy::Tcp
            )
            .is_err()
        );
    }

    #[test]
    fn rtcp_compound_packet_lengths_cover_the_entire_report() {
        let report = rtcp_sender_report(1, 90_000, 30, 5000);
        let mut offset = 0;
        let mut types = Vec::new();
        while offset < report.len() {
            types.push(report[offset + 1]);
            offset +=
                (u16::from_be_bytes([report[offset + 2], report[offset + 3]]) as usize + 1) * 4;
            assert!(offset <= report.len());
        }
        assert_eq!(types, vec![200, 202]);
        assert_eq!(offset, report.len());
    }

    #[test]
    fn codec_changes_drop_old_frames_and_republish_parameter_sets() {
        let source = Broadcaster::default();
        let codec = CodecConfig {
            sps: Bytes::from_static(&[0x67, 0x42, 0, 0x1e]),
            pps: Bytes::from_static(&[0x68, 1]),
            profile_level_id: [0x42, 0, 0x1e],
        };
        source.set_codec_config(codec.clone());
        let sub = source.subscribe();
        let mut frame = EncodedAccessUnit {
            nal_units: vec![Bytes::from_static(&[0x65, 1])],
            pts_90khz: 0,
            duration_90khz: 3000,
            keyframe: true,
        };
        source.publish(frame.clone());
        let mut new_codec = codec;
        new_codec.pps = Bytes::from_static(&[0x68, 2]);
        source.set_codec_config(new_codec.clone());
        assert!(sub.recv_timeout(Duration::ZERO).is_none());
        frame.pts_90khz = 3000;
        source.publish(frame);
        let unit = sub.recv_timeout(Duration::ZERO).unwrap();
        assert_eq!(unit.nal_units[0], new_codec.sps);
        assert_eq!(unit.nal_units[1], new_codec.pps);
        assert_eq!(unit.pts_90khz, 3000);
        assert!(!sdp(None, &new_codec).contains("framerate"));
        assert!(sdp(Some((30_000, 1001)), &new_codec).contains("a=framerate:29.970"));
    }

    struct TestServer {
        address: SocketAddr,
        source: Broadcaster,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<io::Result<()>>>,
    }
    impl TestServer {
        fn new(policy: TransportPolicy, auth: bool) -> Self {
            let source = Broadcaster::default();
            source.set_codec_config(CodecConfig {
                sps: Bytes::from_static(&[0x67, 0x42, 0, 0x1e]),
                pps: Bytes::from_static(&[0x68, 1]),
                profile_level_id: [0x42, 0, 0x1e],
            });
            let server = RtspServer::bind_with_transport(
                "127.0.0.1:0",
                "/camera",
                source.clone(),
                30,
                auth.then(|| ("user".into(), "pass".into())),
                policy,
            )
            .unwrap();
            let address = server.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            Self {
                address,
                source,
                stop,
                worker: Some(thread::spawn(move || server.run_until(flag))),
            }
        }
        fn connect(&self) -> TcpStream {
            let stream = TcpStream::connect(self.address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
        }
        fn frame(&self, timestamp: u32, keyframe: bool) {
            self.source.publish(EncodedAccessUnit {
                nal_units: vec![Bytes::from(vec![
                    if keyframe { 0x65 } else { 0x41 },
                    1,
                    2,
                    3,
                ])],
                pts_90khz: timestamp,
                duration_90khz: 3000,
                keyframe,
            });
        }
        fn stop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap().unwrap();
            }
        }
    }
    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop();
        }
    }

    enum Message {
        Response(String),
        Media(u8, Vec<u8>),
    }
    fn message(stream: &mut TcpStream) -> io::Result<Message> {
        let mut first = [0];
        stream.read_exact(&mut first)?;
        if first[0] == b'$' {
            let mut header = [0; 3];
            stream.read_exact(&mut header)?;
            let mut packet = vec![0; u16::from_be_bytes([header[1], header[2]]) as usize];
            stream.read_exact(&mut packet)?;
            return Ok(Message::Media(header[0], packet));
        }
        let mut data = first.to_vec();
        while !data.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut first)?;
            data.extend_from_slice(&first);
        }
        let headers = String::from_utf8(data).unwrap();
        let length = header_value(&headers, "content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body)?;
        Ok(Message::Response(
            headers + &String::from_utf8(body).unwrap(),
        ))
    }
    fn response(stream: &mut TcpStream) -> String {
        loop {
            if let Message::Response(text) = message(stream).unwrap() {
                return text;
            }
        }
    }
    fn exchange(
        stream: &mut TcpStream,
        method: &str,
        session: Option<&str>,
        headers: &str,
    ) -> String {
        exchange_uri(
            stream,
            method,
            "rtsp://camera.example:8554/camera",
            session,
            headers,
        )
    }
    fn exchange_uri(
        stream: &mut TcpStream,
        method: &str,
        uri: &str,
        session: Option<&str>,
        headers: &str,
    ) -> String {
        let session = session
            .map(|id| format!("Session: {id}\r\n"))
            .unwrap_or_default();
        stream
            .write_all(
                format!("{method} {uri} RTSP/1.0\r\nCSeq: 10\r\n{session}{headers}\r\n").as_bytes(),
            )
            .unwrap();
        response(stream)
    }
    fn session_from(response: &str) -> String {
        header_value(response, "session").unwrap().to_string()
    }

    #[test]
    fn pause_resume_auth_and_binary_rtcp_work_on_the_same_connection() {
        let server = TestServer::new(TransportPolicy::Tcp, true);
        let mut client = server.connect();
        let auth = format!("Authorization: Basic {}\r\n", STANDARD.encode("user:pass"));
        assert!(exchange(&mut client, "DESCRIBE", None, "").starts_with("RTSP/1.0 401"));
        let describe = exchange(&mut client, "DESCRIBE", None, &auth);
        assert!(describe.contains("Content-Base: rtsp://camera.example:8554/camera/"));
        let setup = exchange(
            &mut client,
            "SETUP",
            None,
            &(auth.clone() + "Transport: RTP/AVP/TCP;interleaved=4-5\r\n"),
        );
        let session = session_from(&setup);
        assert!(exchange(&mut client, "PLAY", Some("wrong"), &auth).starts_with("RTSP/1.0 454"));
        assert!(
            exchange_uri(
                &mut client,
                "PLAY",
                "rtsp://camera.example:8554/camera/",
                Some(&session),
                &auth,
            )
            .starts_with("RTSP/1.0 200")
        );
        server.frame(0, true);
        loop {
            if let Message::Media(channel, packet) = message(&mut client).unwrap() {
                assert_eq!(channel, 4);
                if packet[1] & 0x80 != 0 {
                    break;
                }
            }
        }
        client
            .write_all(&[b'$', 5, 0, 8, 0x80, 201, 0, 1, 0, 0, 0, 1])
            .unwrap();
        assert!(
            exchange(
                &mut client,
                "GET_PARAMETER",
                Some(&session),
                &(auth.clone() + "Content-Length: 4\r\n\r\nPINGOPTIONS * RTSP/1.0\r\nCSeq: 11\r\n")
            )
            .starts_with("RTSP/1.0 200")
        );
        assert!(response(&mut client).contains("CSeq: 11"));
        assert!(exchange(&mut client, "PAUSE", Some(&session), "").starts_with("RTSP/1.0 401"));
        assert!(exchange(&mut client, "PAUSE", Some(&session), &auth).starts_with("RTSP/1.0 200"));
        assert_eq!(server.source.stats().viewers, 0);
        server.frame(3000, true);
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let err = message(&mut client).err().expect("PAUSE must stop media");
        assert!(matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert!(exchange(&mut client, "PLAY", Some(&session), &auth).starts_with("RTSP/1.0 200"));
        server.frame(6000, false);
        server.frame(9000, true);
        if let Message::Media(channel, packet) = message(&mut client).unwrap() {
            assert_eq!(channel, 4);
            assert_eq!(u32::from_be_bytes(packet[4..8].try_into().unwrap()), 9000);
        } else {
            panic!("expected resumed RTP");
        }
        assert!(
            exchange(&mut client, "TEARDOWN", Some(&session), &auth).starts_with("RTSP/1.0 200")
        );
    }

    #[test]
    fn udp_delivery_and_server_shutdown_close_active_and_idle_clients() {
        let mut server = TestServer::new(TransportPolicy::Udp, false);
        let mut client = server.connect();
        let mut idle = server.connect();
        let rtp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let rtcp = UdpSocket::bind("127.0.0.1:0").unwrap();
        rtp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert!(
            exchange(&mut client, "SETUP", None, "Transport: RTP/AVP/TCP\r\n")
                .starts_with("RTSP/1.0 461")
        );
        let setup = exchange(
            &mut client,
            "SETUP",
            None,
            &format!(
                "Transport: RTP/AVP;unicast;client_port={}-{}\r\n",
                rtp.local_addr().unwrap().port(),
                rtcp.local_addr().unwrap().port()
            ),
        );
        assert!(setup.contains("source=127.0.0.1"));
        let session = session_from(&setup);
        assert!(exchange(&mut client, "PLAY", Some(&session), "").starts_with("RTSP/1.0 200"));
        server.frame(90_000, true);
        let mut packet = [0; 2048];
        let count = rtp.recv(&mut packet).unwrap();
        assert!(count > 12);
        assert_eq!(packet[0], 0x80);
        assert_eq!(u32::from_be_bytes(packet[4..8].try_into().unwrap()), 90_000);
        server.stop();
        assert_eq!(server.source.stats().viewers, 0);
        assert_eq!(client.read(&mut packet).unwrap(), 0);
        assert_eq!(idle.read(&mut packet).unwrap(), 0);
        let rebound = TcpListener::bind(server.address);
        assert!(rebound.is_ok());
    }

    fn rtsp_exchange(stream: &mut TcpStream, request: &str) -> String {
        stream.write_all(request.as_bytes()).expect("RTSP write");
        let mut buf = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).expect("RTSP read");
            assert_ne!(count, 0, "RTSP client disconnected");
            buf.extend_from_slice(&chunk[..count]);
            let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&buf[..end]).expect("RTSP headers");
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= end + 4 + length {
                return String::from_utf8_lossy(&buf[..end + 4 + length]).into_owned();
            }
        }
    }
}
