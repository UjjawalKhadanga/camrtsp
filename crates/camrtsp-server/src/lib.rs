use base64::{Engine, engine::general_purpose::STANDARD};
use camrtsp_core::{CodecConfig, EncodedAccessUnit};
use camrtsp_rtp::{packetize_h264, rtp_packet};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

const MAX_QUEUED_ACCESS_UNITS: usize = 16;
const RTP_MTU: usize = 1_400;
const NONCE_TTL: Duration = Duration::from_secs(300);

struct Subscriber {
    queue: Arc<Mutex<VecDeque<EncodedAccessUnit>>>,
    alive: Arc<AtomicBool>,
}

#[derive(Default)]
struct BroadcastState {
    clients: Vec<Subscriber>,
    codec: Option<CodecConfig>,
}

#[derive(Clone, Default)]
pub struct Broadcaster(Arc<Mutex<BroadcastState>>);

pub struct Subscription {
    queue: Arc<Mutex<VecDeque<EncodedAccessUnit>>>,
    alive: Arc<AtomicBool>,
}

impl Subscription {
    fn recv_timeout(&self, timeout: Duration) -> Option<EncodedAccessUnit> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(access_unit) = self
                .queue
                .lock()
                .expect("subscriber lock poisoned")
                .pop_front()
            {
                return Some(access_unit);
            }
            if Instant::now() >= deadline || !self.alive.load(Ordering::Relaxed) {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

impl Broadcaster {
    pub fn subscribe(&self) -> Subscription {
        let queue = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_QUEUED_ACCESS_UNITS)));
        let alive = Arc::new(AtomicBool::new(true));
        let mut state = self.0.lock().expect("broadcaster lock poisoned");
        state
            .clients
            .retain(|client| client.alive.load(Ordering::Relaxed));
        state.clients.push(Subscriber {
            queue: queue.clone(),
            alive: alive.clone(),
        });
        Subscription { queue, alive }
    }

    pub fn set_codec_config(&self, codec: CodecConfig) {
        self.0.lock().expect("broadcaster lock poisoned").codec = Some(codec);
    }

    pub fn codec_config(&self) -> Option<CodecConfig> {
        self.0
            .lock()
            .expect("broadcaster lock poisoned")
            .codec
            .clone()
    }

    pub fn publish(&self, access_unit: EncodedAccessUnit) {
        let mut state = self.0.lock().expect("broadcaster lock poisoned");
        state.clients.retain(|client| {
            if !client.alive.load(Ordering::Relaxed) {
                return false;
            }
            let mut queue = client.queue.lock().expect("subscriber lock poisoned");
            if queue.len() == MAX_QUEUED_ACCESS_UNITS {
                if let Some(index) = queue.iter().position(|item| !item.keyframe) {
                    queue.remove(index);
                } else {
                    queue.pop_front();
                }
            }
            queue.push_back(access_unit.clone());
            true
        });
    }
}

pub struct RtspServer {
    listener: TcpListener,
    path: String,
    source: Broadcaster,
    fps: u32,
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
        Ok(Self {
            listener: TcpListener::bind(address)?,
            path: path.into(),
            source,
            fps,
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
        while !stopping.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let path = self.path.clone();
                    let source = self.source.clone();
                    let credentials = self.credentials.clone();
                    let fps = self.fps;
                    let transport = self.transport;
                    thread::spawn(move || {
                        if let Err(error) =
                            handle(stream, &path, source, fps, credentials.as_ref(), transport)
                        {
                            eprintln!("RTSP session error: {error}");
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(error) => eprintln!("RTSP connection error: {error}"),
            }
        }
        Ok(())
    }

    pub fn run_blocking(self) -> io::Result<()> {
        for connection in self.listener.incoming() {
            match connection {
                Ok(stream) => {
                    let path = self.path.clone();
                    let source = self.source.clone();
                    let credentials = self.credentials.clone();
                    let fps = self.fps;
                    let transport = self.transport;
                    thread::spawn(move || {
                        if let Err(error) =
                            handle(stream, &path, source, fps, credentials.as_ref(), transport)
                        {
                            eprintln!("RTSP session error: {error}");
                        }
                    });
                }
                Err(error) => eprintln!("RTSP connection error: {error}"),
            }
        }
        Ok(())
    }
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
    fps: u32,
    credentials: Option<&(String, String)>,
    transport_policy: TransportPolicy,
) -> io::Result<()> {
    // A stream accepted from the cancellable listener inherits non-blocking mode.
    // Control-plane negotiation is deliberately blocking until PLAY succeeds.
    stream.set_nonblocking(false)?;
    let peer = stream.peer_addr()?;
    let mut input = Vec::new();
    let mut delivery = None;
    let mut auth = AuthState {
        nonce: random_hex(24),
        issued: Instant::now(),
    };
    let session_id = random_hex(12);
    loop {
        let request = read_request(&mut stream, &mut input)?;
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
        match request.method.as_str() {
            "OPTIONS" => respond(
                &mut stream,
                request.cseq(),
                "200 OK",
                "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, GET_PARAMETER, TEARDOWN\r\n",
                "",
            )?,
            "DESCRIBE" => match wait_for_codec_config(&source, Duration::from_secs(5)) {
                Some(codec) => {
                    let body = sdp(path, fps, &codec);
                    respond(
                        &mut stream,
                        request.cseq(),
                        "200 OK",
                        &format!(
                            "Content-Type: application/sdp\r\nContent-Base: rtsp://localhost{path}/\r\n"
                        ),
                        &body,
                    )?;
                }
                None => respond(
                    &mut stream,
                    request.cseq(),
                    "503 Service Unavailable",
                    "Retry-After: 1\r\n",
                    "",
                )?,
            },
            "SETUP" => {
                let result = request
                    .header("transport")
                    .and_then(|value| negotiate_transport(value, peer.ip(), transport_policy).ok());
                match result {
                    Some(selected) => {
                        let header = format!(
                            "Transport: {}\r\nSession: {session_id}\r\n",
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
            "PLAY" => match delivery.take() {
                Some(selected) => {
                    respond(
                        &mut stream,
                        request.cseq(),
                        "200 OK",
                        &format!("Session: {session_id}\r\nRange: npt=0.000-\r\n"),
                        "",
                    )?;
                    return stream_video(stream, source.subscribe(), selected);
                }
                None => respond(
                    &mut stream,
                    request.cseq(),
                    "455 Method Not Valid in This State",
                    "",
                    "",
                )?,
            },
            "PAUSE" | "GET_PARAMETER" => respond(
                &mut stream,
                request.cseq(),
                "200 OK",
                &format!("Session: {session_id}\r\n"),
                "",
            )?,
            "TEARDOWN" => {
                respond(
                    &mut stream,
                    request.cseq(),
                    "200 OK",
                    &format!("Session: {session_id}\r\n"),
                    "",
                )?;
                return Ok(());
            }
            _ => respond(
                &mut stream,
                request.cseq(),
                "405 Method Not Allowed",
                "",
                "",
            )?,
        }
    }
}

fn read_request(stream: &mut TcpStream, input: &mut Vec<u8>) -> io::Result<Request> {
    loop {
        if let Some(end) = input.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = end + 4;
            let header_text = std::str::from_utf8(&input[..header_end]).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "RTSP headers were not UTF-8")
            })?;
            let body_length = header_value(header_text, "content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            if input.len() >= header_end + body_length {
                let raw = String::from_utf8_lossy(&input[..header_end]).to_string();
                input.drain(..header_end + body_length);
                return parse_request(&raw);
            }
        }
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "RTSP client disconnected",
            ));
        }
        if input.len() + count > 65_536 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RTSP request too large",
            ));
        }
        input.extend_from_slice(&chunk[..count]);
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
    if request_line.next() != Some("RTSP/1.0") {
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
        headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
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

fn wait_for_codec_config(source: &Broadcaster, timeout: Duration) -> Option<CodecConfig> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(config) = source.codec_config() {
            return Some(config);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn sdp(_path: &str, fps: u32, codec: &CodecConfig) -> String {
    format!(
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=camrtsp\r\nt=0 0\r\na=control:*\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1;profile-level-id={:02x}{:02x}{:02x};sprop-parameter-sets={},{}\r\na=framerate:{fps}\r\na=control:trackID=0\r\n",
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
    policy: TransportPolicy,
) -> Result<Delivery, ()> {
    for alternative in value.split(',') {
        let lower = alternative.to_ascii_lowercase();
        if lower.contains("rtp/avp/tcp") && policy != TransportPolicy::Udp {
            return Ok(Delivery::Tcp {
                channels: parameter(&lower, "interleaved")
                    .and_then(parse_pair)
                    .and_then(|(rtp, rtcp)| {
                        Some((u8::try_from(rtp).ok()?, u8::try_from(rtcp).ok()?))
                    })
                    .unwrap_or((0, 1)),
            });
        }
        if lower.contains("rtp/avp") && !lower.contains("tcp") && policy != TransportPolicy::Tcp {
            let Some((rtp_port, rtcp_port)) = parameter(&lower, "client_port").and_then(parse_pair)
            else {
                continue;
            };
            let (rtp, rtcp) = bind_udp_pair().map_err(|_| ())?;
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

fn bind_udp_pair() -> io::Result<(UdpSocket, UdpSocket)> {
    for port in (50_000..60_000).step_by(2) {
        let rtp = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(socket) => socket,
            Err(_) => continue,
        };
        if let Ok(rtcp) = UdpSocket::bind(("0.0.0.0", port + 1)) {
            return Ok((rtp, rtcp));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "no UDP RTP/RTCP ports available",
    ))
}

fn stream_video(
    mut stream: TcpStream,
    subscription: Subscription,
    mut delivery: Delivery,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let mut sequence = rand::rng().random::<u16>();
    let ssrc = rand::rng().random::<u32>();
    let mut ready = false;
    let mut packets = 0_u32;
    let mut octets = 0_u32;
    let mut last_timestamp = 0_u32;
    let mut last_report = Instant::now();
    let mut control = Vec::new();
    loop {
        if let Some(access_unit) = subscription.recv_timeout(Duration::from_millis(50)) {
            ready |= access_unit.keyframe
                || access_unit
                    .nal_units
                    .iter()
                    .any(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 5));
            if ready {
                for (nal_index, nal) in access_unit.nal_units.iter().enumerate() {
                    let payloads = packetize_h264(nal, RTP_MTU);
                    let last_payload = payloads.len().saturating_sub(1);
                    for (payload_index, payload) in payloads.iter().enumerate() {
                        let marker = nal_index + 1 == access_unit.nal_units.len()
                            && payload_index == last_payload;
                        let packet =
                            rtp_packet(payload, sequence, access_unit.pts_90khz, ssrc, marker);
                        delivery.send_rtp(&mut stream, &packet)?;
                        sequence = sequence.wrapping_add(1);
                        packets = packets.wrapping_add(1);
                        octets = octets.wrapping_add(payload.len() as u32);
                    }
                }
                last_timestamp = access_unit.pts_90khz;
            }
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            delivery.send_rtcp(
                &mut stream,
                &rtcp_sender_report(ssrc, last_timestamp, packets, octets),
            )?;
            last_report = Instant::now();
        }
        if let Some(request) = read_control(&mut stream, &mut control)? {
            match request.method.as_str() {
                "TEARDOWN" => {
                    respond(&mut stream, request.cseq(), "200 OK", "", "")?;
                    let _ = delivery.send_rtcp(&mut stream, &rtcp_bye(ssrc));
                    return Ok(());
                }
                "GET_PARAMETER" | "OPTIONS" => {
                    respond(&mut stream, request.cseq(), "200 OK", "", "")?;
                }
                "PAUSE" => {
                    respond(&mut stream, request.cseq(), "200 OK", "", "")?;
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
    }
}

fn read_control(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> io::Result<Option<Request>> {
    let mut chunk = [0_u8; 4096];
    match stream.read(&mut chunk) {
        Ok(0) => {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "RTSP client disconnected",
            ));
        }
        Ok(count) => buffer.extend_from_slice(&chunk[..count]),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(error) => return Err(error),
    }
    if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
        let raw = String::from_utf8_lossy(&buffer[..end + 4]).to_string();
        buffer.drain(..end + 4);
        return parse_request(&raw).map(Some);
    }
    Ok(None)
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
        (Some(qop), Some(nc), Some(cnonce)) => {
            hash(format!("{ha1}:{}:{nc}:{cnonce}:{qop}:{ha2}", auth.nonce))
        }
        _ => hash(format!("{ha1}:{}:{ha2}", auth.nonce)),
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
    packet.extend_from_slice(&[0x81, 202, 0, 3]);
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
    fn access_unit_queue_discards_an_old_delta_frame() {
        let broadcaster = Broadcaster::default();
        let subscription = broadcaster.subscribe();
        for timestamp in 0..17 {
            broadcaster.publish(EncodedAccessUnit {
                nal_units: vec![Bytes::from_static(&[0x41])],
                pts_90khz: timestamp,
                duration_90khz: 3000,
                keyframe: false,
            });
        }
        assert_eq!(
            subscription.recv_timeout(Duration::ZERO).unwrap().pts_90khz,
            1
        );
    }

    #[test]
    fn sdp_uses_live_codec_configuration() {
        let codec = CodecConfig {
            sps: Bytes::from_static(&[0x67, 0x4d, 0, 0x1f]),
            pps: Bytes::from_static(&[0x68, 0xee]),
            profile_level_id: [0x4d, 0, 0x1f],
        };
        let value = sdp("/camera", 30, &codec);
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
