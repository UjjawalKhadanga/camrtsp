use bytes::Bytes;
use camrtsp_core::{CodecConfig, EncodedAccessUnit};
use camrtsp_server::{Broadcaster, RtspServer, TransportPolicy};
use jni::{
    JNIEnv,
    objects::{JByteArray, JByteBuffer, JClass, JString},
    sys::{jint, jlong, jstring},
};
use once_cell::sync::Lazy;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread,
};

struct ServerHandle {
    broadcaster: Broadcaster,
    stopping: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);
static SERVERS: Lazy<Mutex<HashMap<i64, ServerHandle>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn throw(env: &mut JNIEnv<'_>, message: impl AsRef<str>) {
    let _ = env.throw_new("java/lang/IllegalStateException", message.as_ref());
}

fn string(env: &mut JNIEnv<'_>, value: JString<'_>) -> Result<String, String> {
    env.get_string(&value)
        .map(|text| text.into())
        .map_err(|error| error.to_string())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_camrtsp_NativeBridge_nativeCreateServer(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    port: jint,
    path: JString<'_>,
    username: JString<'_>,
    password: JString<'_>,
    transport: jint,
) -> jlong {
    let result = (|| {
        if !(1..=65535).contains(&port) {
            return Err("RTSP port must be in 1..65535".to_string());
        }
        let path = string(&mut env, path)?;
        if !camrtsp_server::valid_path(&path) {
            return Err("RTSP path must start with '/'".to_string());
        }
        let username = string(&mut env, username)?;
        let password = string(&mut env, password)?;
        if username.is_empty() != password.is_empty() {
            return Err("supply both username and password or neither".into());
        }
        let credentials = (!username.is_empty()).then_some((username, password));
        let transport = match transport {
            0 => TransportPolicy::Both,
            1 => TransportPolicy::Tcp,
            2 => TransportPolicy::Udp,
            _ => return Err("invalid transport policy".into()),
        };
        let broadcaster = Broadcaster::default();
        let server = RtspServer::bind_with_transport(
            &format!("0.0.0.0:{port}"),
            path,
            broadcaster.clone(),
            0,
            credentials,
            transport,
        )
        .map_err(|error| error.to_string())?;
        let stopping = Arc::new(AtomicBool::new(false));
        let server_stopping = stopping.clone();
        let worker = thread::Builder::new()
            .name("camrtsp-rtsp".into())
            .spawn(move || {
                if let Err(error) = server.run_until(server_stopping.clone()) {
                    eprintln!("camrtsp server stopped: {error}");
                }
                server_stopping.store(true, Ordering::Relaxed);
            })
            .map_err(|error| error.to_string())?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        SERVERS.lock().expect("server map poisoned").insert(
            handle,
            ServerHandle {
                broadcaster,
                stopping,
                worker,
            },
        );
        Ok(handle as jlong)
    })();
    match result {
        Ok(handle) => handle,
        Err(error) => {
            throw(&mut env, error);
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_camrtsp_NativeBridge_nativeSetCodecConfig(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    sps: JByteArray<'_>,
    pps: JByteArray<'_>,
) {
    let result = (|| {
        let sps = env
            .convert_byte_array(&sps)
            .map_err(|error| error.to_string())?;
        let pps = env
            .convert_byte_array(&pps)
            .map_err(|error| error.to_string())?;
        if sps.len() < 4 || sps[0] & 0x1f != 7 || pps.first().is_none_or(|b| b & 0x1f != 8) {
            return Err("SPS and PPS are required".to_string());
        }
        let profile_level_id = sps
            .get(1..4)
            .ok_or("invalid SPS")?
            .try_into()
            .map_err(|_| "invalid SPS")?;
        let servers = SERVERS.lock().expect("server map poisoned");
        let server = servers.get(&handle).ok_or("unknown native server handle")?;
        server.broadcaster.set_codec_config(CodecConfig {
            sps: Bytes::from(sps),
            pps: Bytes::from(pps),
            profile_level_id,
        });
        Ok(())
    })();
    if let Err(error) = result {
        throw(&mut env, error);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_camrtsp_NativeBridge_nativePushAccessUnit(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    buffer: JByteBuffer<'_>,
    offset: jint,
    size: jint,
    pts_us: jlong,
    flags: jint,
) {
    let result = (|| {
        if offset < 0 || size < 0 || pts_us < 0 {
            return Err("encoded access-unit range is invalid".to_string());
        }
        let pointer = env
            .get_direct_buffer_address(&buffer)
            .map_err(|error| error.to_string())?;
        let capacity = env
            .get_direct_buffer_capacity(&buffer)
            .map_err(|error| error.to_string())?;
        let start = offset as usize;
        let end = start
            .checked_add(size as usize)
            .ok_or("encoded access-unit range overflow")?;
        let bytes = unsafe { std::slice::from_raw_parts(pointer, capacity) };
        let payload = bytes
            .get(start..end)
            .ok_or("encoded access-unit range is outside the buffer")?;
        let nal_units = split_h264(payload);
        if nal_units.is_empty() {
            return Err("encoded access unit contains no H.264 NAL units".to_string());
        }
        let servers = SERVERS.lock().expect("server map poisoned");
        let server = servers.get(&handle).ok_or("unknown native server handle")?;
        server.broadcaster.publish(EncodedAccessUnit {
            nal_units: nal_units.into_iter().map(Bytes::from).collect(),
            pts_90khz: ((pts_us as u128 * 90) / 1000) as u32,
            duration_90khz: 0,
            keyframe: flags & 1 != 0,
        });
        Ok(())
    })();
    if let Err(error) = result {
        throw(&mut env, error);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_camrtsp_NativeBridge_nativeGetStats(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    let servers = SERVERS.lock().expect("server map poisoned");
    let text = match servers.get(&handle) {
        Some(server) => {
            let mut value =
                serde_json::to_value(server.broadcaster.stats()).expect("serializable stats");
            value["active"] = serde_json::json!(!server.stopping.load(Ordering::Relaxed));
            value.to_string()
        }
        None => "{\"active\":false,\"ready\":false,\"viewers\":0,\"frames\":0}".into(),
    };
    match env.new_string(text) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_camrtsp_NativeBridge_nativeStopServer(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let server = SERVERS.lock().expect("server map poisoned").remove(&handle);
    if let Some(server) = server {
        server.stopping.store(true, Ordering::Relaxed);
        if server.worker.join().is_err() {
            throw(&mut env, "RTSP server thread panicked");
        }
    } else {
        throw(&mut env, "unknown native server handle");
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_camrtsp_NativeBridge_nativeSetFrameRate(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    numerator: jint,
    denominator: jint,
) {
    if numerator < 0 || denominator <= 0 {
        throw(&mut env, "invalid frame rate");
        return;
    }
    if let Some(server) = SERVERS.lock().expect("server map poisoned").get(&handle) {
        server
            .broadcaster
            .set_frame_rate(numerator as u32, denominator as u32);
    } else {
        throw(&mut env, "unknown native server handle");
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_camrtsp_NativeBridge_nativeTakeKeyframeRequest(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    SERVERS
        .lock()
        .expect("server map poisoned")
        .get(&handle)
        .is_some_and(|server| server.broadcaster.take_keyframe_request()) as jint
}

fn split_h264(payload: &[u8]) -> Vec<Vec<u8>> {
    if payload.starts_with(&[0, 0, 0, 1]) || payload.starts_with(&[0, 0, 1]) {
        let mut units = Vec::new();
        let mut start = None;
        let mut index = 0;
        while index < payload.len() {
            let prefix = if payload[index..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if payload[index..].starts_with(&[0, 0, 1]) {
                3
            } else {
                0
            };
            if prefix > 0 {
                if let Some(start) = start {
                    let mut end = index;
                    while end > start && payload[end - 1] == 0 {
                        end -= 1;
                    }
                    if end > start {
                        units.push(payload[start..end].to_vec());
                    }
                }
                index += prefix;
                start = Some(index);
            } else {
                index += 1;
            }
        }
        if let Some(start) = start {
            let mut end = payload.len();
            while end > start && payload[end - 1] == 0 {
                end -= 1;
            }
            if end > start {
                units.push(payload[start..end].to_vec());
            }
        }
        return units;
    }
    // A raw NAL header is nonzero; a four-byte AVCC length for our bounded buffers starts with zero.
    if payload
        .first()
        .is_some_and(|b| b & 0x80 == 0 && (1..=23).contains(&(b & 0x1f)))
    {
        return vec![payload.to_vec()];
    }
    let mut units = Vec::new();
    let mut offset = 0;
    while offset < payload.len() {
        let Some(length) = payload.get(offset..offset + 4) else {
            return Vec::new();
        };
        let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
        offset += 4;
        let Some(end) = offset.checked_add(length) else {
            return Vec::new();
        };
        let Some(nal) = payload.get(offset..end).filter(|nal| !nal.is_empty()) else {
            return Vec::new();
        };
        units.push(nal.to_vec());
        offset = end;
    }
    units
}

#[cfg(test)]
mod tests {
    use super::split_h264;

    #[test]
    fn handles_four_byte_start_codes_and_rejects_truncated_avcc() {
        assert_eq!(
            split_h264(&[0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x41, 2]),
            vec![vec![0x65, 1], vec![0x41, 2]]
        );
        assert_eq!(
            split_h264(&[0x65, 1, 2, 3, 4]),
            vec![vec![0x65, 1, 2, 3, 4]]
        );
        assert!(split_h264(&[0, 0, 0, 2, 0x65]).is_empty());
        assert!(split_h264(&[0, 0, 0, 1, 0x65, 0, 0, 0, 0]).len() == 1);
        assert!(split_h264(&[0, 0, 0, 2, 0x65, 1, 0]).is_empty());
    }

    #[test]
    fn splits_annex_b_and_avcc() {
        assert_eq!(
            split_h264(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2]),
            vec![vec![0x67, 1], vec![0x68, 2]]
        );
        assert_eq!(split_h264(&[0, 0, 0, 2, 0x67, 1]), vec![vec![0x67, 1]]);
    }
}
