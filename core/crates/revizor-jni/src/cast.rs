//! JNI for casting to TVs that have nothing of Revizor installed (Google Cast / DLNA).

use crate::events::Callback;
use jni::objects::{JByteBuffer, JClass, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;
use revizor_cast::{CastConfig, CastEvent, CastSession, CastState, Method, Tv};
use std::net::IpAddr;
use std::time::Duration;

struct CastH {
    session: CastSession,
}

fn jstr(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// `cast,<port>` or `dlna,<control url>,<service type>` joined by `;`.
fn encode_methods(m: &[Method]) -> String {
    m.iter()
        .map(|m| match m {
            Method::Cast { port } => format!("cast,{port}"),
            Method::Dlna { control_url, service_type } => format!("dlna,{control_url},{service_type}"),
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn decode_methods(s: &str) -> Vec<Method> {
    s.split(';')
        .filter_map(|p| {
            let mut it = p.splitn(3, ',');
            match it.next()? {
                "cast" => Some(Method::Cast { port: it.next()?.parse().ok()? }),
                "dlna" => Some(Method::Dlna { control_url: it.next()?.to_string(), service_type: it.next()?.to_string() }),
                _ => None,
            }
        })
        .collect()
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// One TV per line, tab-separated: `name ip model manufacturer methods`.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_castScan(env: JNIEnv, _c: JClass, timeout_ms: jint) -> jstring {
    let tvs = revizor_cast::scan_tvs(Duration::from_millis(timeout_ms.clamp(500, 15_000) as u64));
    let mut out = String::new();
    for t in tvs {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            clean(&t.name),
            t.ip,
            clean(t.model.as_deref().unwrap_or("")),
            clean(t.manufacturer.as_deref().unwrap_or("")),
            encode_methods(&t.methods)
        ));
    }
    env.new_string(out).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

fn state_event(cb: &Callback, s: CastState) {
    match s {
        CastState::Preparing => cb.call(1, &[0], ""),
        CastState::Starting => cb.call(1, &[1], ""),
        CastState::Playing => cb.call(1, &[2], ""),
        CastState::Buffering => cb.call(1, &[3], ""),
        CastState::Reconnecting => cb.call(1, &[4], ""),
        CastState::Stopped => cb.call(1, &[5], ""),
        CastState::Failed(m) => cb.call(1, &[6], &m),
    }
}

/// Returns a handle (0 on failure). Events: `1` state (nums[0]: 0 preparing, 1 starting, 2 playing, 3 buffering,
/// 4 reconnecting, 5 stopped, 6 failed + text), `2` trying next method (text), `3` keyframe needed now.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_castStart(
    mut env: JNIEnv,
    _c: JClass,
    cb: JObject,
    ip: JString,
    name: JString,
    methods: JString,
    has_audio: jboolean,
) -> jlong {
    let Some(callback) = Callback::new(&mut env, &cb) else { return 0 };
    let Ok(ip) = jstr(&mut env, &ip).parse::<IpAddr>() else { return 0 };
    let tv = Tv { name: jstr(&mut env, &name), ip, model: None, manufacturer: None, methods: decode_methods(&jstr(&mut env, &methods)) };
    let cfg = CastConfig { has_audio: has_audio != 0, ..Default::default() };
    match CastSession::start(&tv, cfg, move |e| match e {
        CastEvent::State(s) => state_event(&callback, s),
        CastEvent::TryingNext { method } => callback.call(2, &[], method),
        CastEvent::NeedKeyframe => callback.call(3, &[], ""),
    }) {
        Ok(session) => Box::into_raw(Box::new(CastH { session })) as jlong,
        Err(e) => {
            log::warn!("cast start failed: {e}");
            0
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_castStop(_e: JNIEnv, _c: JClass, h: jlong) {
    if h != 0 {
        unsafe { Box::from_raw(h as *mut CastH) }.session.stop();
    }
}

/// Zero-copy: the encoder's direct buffer is read in place while the muxer builds the TS packets.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_castSubmitVideo(
    env: JNIEnv,
    _c: JClass,
    h: jlong,
    pts_us: jlong,
    keyframe: jboolean,
    buf: JByteBuffer,
    offset: jint,
    len: jint,
) -> jboolean {
    if h == 0 {
        return JNI_FALSE;
    }
    let s = unsafe { &*(h as *const CastH) };
    let (Ok(base), Ok(cap)) = (env.get_direct_buffer_address(&buf), env.get_direct_buffer_capacity(&buf)) else { return JNI_FALSE };
    if offset < 0 || len < 0 || offset as usize + len as usize > cap {
        return JNI_FALSE;
    }
    // SAFETY: bounds checked against the buffer capacity; MediaCodec keeps the buffer valid until released.
    let data = unsafe { std::slice::from_raw_parts(base.add(offset as usize), len as usize) };
    s.session.submit_video(pts_us.max(0) as u64, keyframe != 0, data);
    JNI_TRUE
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_castSubmitAudio(env: JNIEnv, _c: JClass, h: jlong, pts_us: jlong, buf: JByteBuffer, offset: jint, len: jint) -> jboolean {
    if h == 0 {
        return JNI_FALSE;
    }
    let s = unsafe { &*(h as *const CastH) };
    let (Ok(base), Ok(cap)) = (env.get_direct_buffer_address(&buf), env.get_direct_buffer_capacity(&buf)) else { return JNI_FALSE };
    if offset < 0 || len < 0 || offset as usize + len as usize > cap {
        return JNI_FALSE;
    }
    let data = unsafe { std::slice::from_raw_parts(base.add(offset as usize), len as usize) };
    s.session.submit_audio(pts_us.max(0) as u64, data);
    JNI_TRUE
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_castStats(env: JNIEnv, _c: JClass, h: jlong) -> jstring {
    if h == 0 {
        return env.new_string("{}").map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut());
    }
    let s = unsafe { &*(h as *const CastH) }.session.stats();
    let json = format!(
        "{{\"method\":\"{}\",\"framesIn\":{},\"bytesIn\":{},\"inBps\":{},\"bytesServed\":{},\"viewers\":{},\"segments\":{},\"playlistRequests\":{},\"segmentRequests\":{},\"rejected\":{},\"slowDropped\":{},\"bufferedBytes\":{},\"uptimeS\":{}}}",
        s.method, s.frames_in, s.bytes_in, s.in_bps, s.bytes_served, s.viewers, s.segments, s.playlist_requests, s.segment_requests, s.rejected_requests, s.slow_viewers_dropped, s.buffered_bytes, s.uptime_s
    );
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_roundtrip() {
        let m = vec![Method::Cast { port: 8009 }, Method::Dlna { control_url: "http://10.0.0.2:9197/upnp/control/AVTransport1".into(), service_type: "urn:schemas-upnp-org:service:AVTransport:1".into() }];
        assert_eq!(decode_methods(&encode_methods(&m)), m);
        assert!(decode_methods("garbage;cast,x;dlna").is_empty());
    }
}
