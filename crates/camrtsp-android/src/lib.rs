use bytes::Bytes;
use camrtsp_core::{CodecConfig, EncodedAccessUnit};
use camrtsp_server::{Broadcaster, RtspServer};
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
    _transport: jint,
) -> jlong {
    let result = (|| {
        if !(1..=65535).contains(&port) {
            return Err("RTSP port must be in 1..65535".to_string());
        }
        let path = string(&mut env, path)?;
        if !path.starts_with('/') {
            return Err("RTSP path must start with '/'".to_string());
        }
        let username = string(&mut env, username)?;
        let password = string(&mut env, password)?;
        let credentials =
            (!username.is_empty() || !password.is_empty()).then_some((username, password));
        let broadcaster = Broadcaster::default();
        let server = RtspServer::bind(
            &format!("0.0.0.0:{port}"),
            path,
            broadcaster.clone(),
            30,
            credentials,
        )
        .map_err(|error| error.to_string())?;
        let stopping = Arc::new(AtomicBool::new(false));
        let server_stopping = stopping.clone();
        thread::Builder::new()
            .name("camrtsp-rtsp".into())
            .spawn(move || {
                if let Err(error) = server.run_until(server_stopping) {
                    eprintln!("camrtsp server stopped: {error}");
                }
            })
            .map_err(|error| error.to_string())?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        SERVERS.lock().expect("server map poisoned").insert(
            handle,
            ServerHandle {
                broadcaster,
                stopping,
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
        if sps.is_empty() || pps.is_empty() {
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
            pts_90khz: ((pts_us as u64 * 90) / 1000) as u32,
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
    let active = SERVERS
        .lock()
        .expect("server map poisoned")
        .contains_key(&handle);
    let text = format!("{{\"active\":{active},\"viewers\":0}}");
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
    if let Some(server) = SERVERS.lock().expect("server map poisoned").remove(&handle) {
        server.stopping.store(true, Ordering::Relaxed);
    } else {
        throw(&mut env, "unknown native server handle");
    }
}

fn split_h264(payload: &[u8]) -> Vec<Vec<u8>> {
    if payload.starts_with(&[0, 0, 0, 1]) || payload.starts_with(&[0, 0, 1]) {
        let starts = (0..payload.len())
            .filter(|&index| {
                payload[index..].starts_with(&[0, 0, 1])
                    || payload[index..].starts_with(&[0, 0, 0, 1])
            })
            .collect::<Vec<_>>();
        return starts
            .iter()
            .enumerate()
            .filter_map(|(index, start)| {
                let prefix = if payload[*start..].starts_with(&[0, 0, 0, 1]) {
                    4
                } else {
                    3
                };
                let end = starts.get(index + 1).copied().unwrap_or(payload.len());
                (start + prefix < end).then(|| payload[start + prefix..end].to_vec())
            })
            .collect();
    }
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 4 <= payload.len() {
        let length = u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let Some(end) = offset.checked_add(length) else {
            return Vec::new();
        };
        let Some(nal) = payload.get(offset..end) else {
            return Vec::new();
        };
        result.push(nal.to_vec());
        offset = end;
    }
    if result.is_empty() && !payload.is_empty() {
        vec![payload.to_vec()]
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::split_h264;

    #[test]
    fn splits_annex_b_and_avcc() {
        assert_eq!(
            split_h264(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2]),
            vec![vec![0x67, 1], vec![0x68, 2]]
        );
        assert_eq!(split_h264(&[0, 0, 0, 2, 0x67, 1]), vec![vec![0x67, 1]]);
    }
}
