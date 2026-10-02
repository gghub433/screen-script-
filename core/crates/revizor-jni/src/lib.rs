//! JNI bridge used by the Android app (`app.revizor.core.Native`).
//!
//! Design rules:
//! * Opaque `long` handles own boxed Rust objects; Kotlin must call the matching
//!   `…Stop`/`…Free` exactly once.
//! * Encoded video/audio cross the boundary as *direct* `ByteBuffer`s straight
//!   from/to `MediaCodec`, so the only copy is the one into the send queue.
//! * Events are delivered to a Kotlin callback as `(kind: Int, nums: LongArray, text: String)`.
//!   Kind numbers are documented in `app/revizor/core/Events.kt`.
//! * Nothing here invents numbers: every statistic comes from the session objects.

mod cast;
mod caps;
mod events;
mod stats;

use caps::{build_caps, build_pref, profile_from};
use events::{receiver_event, sender_event, Callback};
use jni::objects::{JByteBuffer, JClass, JIntArray, JLongArray, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;
use revizor_adaptive::{Battery, ThermalLevel, Tier};
use revizor_crypto::{FileTrustStore, Identity, TrustStore};
use revizor_session::{
    pair_as_sender, MonotonicClock, PairError, ReceiverConfig, ReceiverSession, SenderConfig, SenderSession, SourceInfo,
};
use revizor_transport::discovery::{broadcast_targets, scan, DiscoveryResponder};
use revizor_transport::{TcpTransport, Transport, UdpTransport};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

struct Core {
    identity: Arc<Identity>,
    trust: Arc<FileTrustStore>,
}

struct SenderH {
    session: SenderSession,
}

struct ReceiverH {
    session: Arc<ReceiverSession>,
    _discovery: Option<DiscoveryResponder>,
}

fn jstr(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

pub(crate) fn int_vec(env: &mut JNIEnv, a: &JIntArray) -> Vec<i32> {
    let n = env.get_array_length(a).unwrap_or(0) as usize;
    let mut v = vec![0i32; n];
    let _ = env.get_int_array_region(a, 0, &mut v);
    v
}

fn make_transport(tcp: bool, bind: &str) -> Result<Arc<dyn Transport>, String> {
    let addr: SocketAddr = bind.parse().map_err(|e| format!("{e}"))?;
    Ok(if tcp { Arc::new(TcpTransport::listen(addr).map_err(|e| e.to_string())?) } else { Arc::new(UdpTransport::bind(addr).map_err(|e| e.to_string())?) })
}

// ───────────────────────────── core / identity ─────────────────────────────

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_coreOpen(mut env: JNIEnv, _c: JClass, dir: JString) -> jlong {
    let dir = std::path::PathBuf::from(jstr(&mut env, &dir));
    let _ = std::fs::create_dir_all(&dir);
    let key = dir.join("identity.key");
    let identity = match std::fs::read_to_string(&key)
        .ok()
        .and_then(|s| revizor_crypto::identity::unhex(s.trim()))
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
    {
        Some(b) => Identity::from_secret(b),
        None => {
            let id = Identity::generate();
            if std::fs::write(&key, revizor_crypto::identity::hex(&id.secret_bytes())).is_err() {
                return 0;
            }
            id
        }
    };
    let Ok(trust) = FileTrustStore::open(dir.join("trusted.tsv")) else { return 0 };
    Box::into_raw(Box::new(Core { identity: Arc::new(identity), trust: Arc::new(trust) })) as jlong
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_coreFree(_e: JNIEnv, _c: JClass, h: jlong) {
    if h != 0 {
        drop(unsafe { Box::from_raw(h as *mut Core) });
    }
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_coreDeviceId(env: JNIEnv, _c: JClass, h: jlong) -> jstring {
    let core = unsafe { &*(h as *const Core) };
    env.new_string(core.identity.device_id()).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// `id\tname` per line.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_coreTrusted(env: JNIEnv, _c: JClass, h: jlong) -> jstring {
    let core = unsafe { &*(h as *const Core) };
    let s: String = core.trust.list().iter().map(|d| format!("{}\t{}\n", d.id(), d.name)).collect();
    env.new_string(s).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_coreForget(mut env: JNIEnv, _c: JClass, h: jlong, id: JString) -> jboolean {
    let core = unsafe { &*(h as *const Core) };
    let id = jstr(&mut env, &id);
    if core.trust.remove(&id) { JNI_TRUE } else { JNI_FALSE }
}

// ───────────────────────────── discovery & pairing ─────────────────────────────

/// Probes the LAN for `timeoutMs`. One line per device:
/// `deviceId|name|ip|port|pairingOpen|trusted|maxW|maxH|maxFps|codecs`.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_scan(env: JNIEnv, _c: JClass, h: jlong, timeout_ms: jint) -> jstring {
    let core = unsafe { &*(h as *const Core) };
    let found = scan(&broadcast_targets(revizor_proto::discovery::DISCOVERY_PORT), Duration::from_millis(timeout_ms.max(200) as u64)).unwrap_or_default();
    let mut out = String::new();
    for f in found {
        let a = &f.announcement;
        let trusted = core.trust.list().iter().any(|d| d.id() == a.device_id);
        let codecs: Vec<String> = a.codecs.iter().map(|c| (*c as u8).to_string()).collect();
        out.push_str(&format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}\n",
            a.device_id,
            a.name.replace('|', " "),
            f.addr,
            a.media_port,
            a.pairing_open as u8,
            trusted as u8,
            a.max_width,
            a.max_height,
            a.max_fps,
            codecs.join(",")
        ));
    }
    env.new_string(out).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// 0 = ok, 1 = wrong PIN, 2 = no answer, 3 = I/O error.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_pairAsSender(
    mut env: JNIEnv,
    _c: JClass,
    h: jlong,
    ip: JString,
    port: jint,
    pin: JString,
    name: JString,
) -> jint {
    let core = unsafe { &*(h as *const Core) };
    let (ip, pin, name) = (jstr(&mut env, &ip), jstr(&mut env, &pin), jstr(&mut env, &name));
    let Ok(ip) = ip.parse::<IpAddr>() else { return 3 };
    let Ok(t) = UdpTransport::bind("0.0.0.0:0".parse().unwrap()) else { return 3 };
    match pair_as_sender(&t, SocketAddr::new(ip, port as u16), core.identity.clone(), &name, &pin, core.trust.clone()) {
        Ok(()) => 0,
        Err(PairError::WrongPin) => 1,
        Err(PairError::NoAnswer) => 2,
        Err(PairError::Io(_)) => 3,
    }
}

// ───────────────────────────── sender ─────────────────────────────

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_app_revizor_core_Native_senderStart(
    mut env: JNIEnv,
    _c: JClass,
    core: jlong,
    cb: JObject,
    ip: JString,
    port: jint,
    name: JString,
    codec_caps: JIntArray,
    audio_caps: JIntArray,
    max_bitrate: jint,
    codec_order: JIntArray,
    want_audio: jboolean,
    profile: jint,
    custom: JIntArray,
    src_w: jint,
    src_h: jint,
    src_hz: jint,
    size_align: jint,
    tcp: jboolean,
) -> jlong {
    let core = unsafe { &*(core as *const Core) };
    let Some(callback) = Callback::new(&mut env, &cb) else { return 0 };
    let ip = jstr(&mut env, &ip);
    let Ok(ip) = ip.parse::<IpAddr>() else { return 0 };
    let name = jstr(&mut env, &name);
    let codec_caps = int_vec(&mut env, &codec_caps);
    let audio_caps = int_vec(&mut env, &audio_caps);
    let order = int_vec(&mut env, &codec_order);
    let custom = int_vec(&mut env, &custom);
    let tcp = tcp != 0;

    let caps = build_caps(&name, &codec_caps, &audio_caps, max_bitrate.max(1_000_000) as u32, tcp);
    let (pref, ceiling_short) = build_pref(&order, want_audio != 0, &custom);
    let transport: Arc<dyn Transport> = if tcp {
        match TcpTransport::connect(SocketAddr::new(ip, port as u16)) {
            Ok(t) => Arc::new(t),
            Err(_) => return 0,
        }
    } else {
        match UdpTransport::bind("0.0.0.0:0".parse().unwrap()) {
            Ok(t) => Arc::new(t),
            Err(_) => return 0,
        }
    };
    let cfg = SenderConfig {
        identity: core.identity.clone(),
        trust: core.trust.clone(),
        caps,
        pref: pref.clone(),
        profile: profile_from(profile),
        custom_tier: ceiling_short.map(|(s, f)| Tier::new(s, f)),
        custom_bitrate: custom.get(2).copied().filter(|b| *b > 0).map(|b| b as u32),
        source: SourceInfo { width: src_w as u16, height: src_h as u16, refresh_hz: src_hz.clamp(1, 1000) as u16 },
        peer: SocketAddr::new(ip, port as u16),
        size_align: size_align.clamp(2, 64) as u16,
        give_up_after: Duration::from_secs(45),
    };
    let session = SenderSession::start(cfg, transport, Arc::new(MonotonicClock), move |e| sender_event(&callback, e));
    Box::into_raw(Box::new(SenderH { session })) as jlong
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_senderStop(_e: JNIEnv, _c: JClass, h: jlong) {
    if h != 0 {
        let s = unsafe { Box::from_raw(h as *mut SenderH) };
        s.session.stop();
    }
}

/// Copies `len` bytes at `offset` out of a direct buffer (MediaCodec output).
fn take_direct(env: &mut JNIEnv, buf: &JByteBuffer, offset: jint, len: jint) -> Option<Vec<u8>> {
    let base = env.get_direct_buffer_address(buf).ok()?;
    let cap = env.get_direct_buffer_capacity(buf).ok()?;
    if offset < 0 || len < 0 || (offset as usize + len as usize) > cap {
        return None;
    }
    // SAFETY: bounds checked against the buffer capacity; MediaCodec keeps the buffer valid until released.
    Some(unsafe { std::slice::from_raw_parts(base.add(offset as usize), len as usize) }.to_vec())
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_senderSubmitVideo(
    mut env: JNIEnv,
    _c: JClass,
    h: jlong,
    epoch: jint,
    pts_us: jlong,
    keyframe: jboolean,
    buf: JByteBuffer,
    offset: jint,
    len: jint,
) -> jboolean {
    let s = unsafe { &*(h as *const SenderH) };
    match take_direct(&mut env, &buf, offset, len) {
        Some(d) => s.session.submit_video(epoch as u16, pts_us as u64, keyframe != 0, d) as jboolean,
        None => JNI_FALSE,
    }
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_senderSubmitAudio(
    mut env: JNIEnv,
    _c: JClass,
    h: jlong,
    pts_us: jlong,
    buf: JByteBuffer,
    offset: jint,
    len: jint,
) -> jboolean {
    let s = unsafe { &*(h as *const SenderH) };
    match take_direct(&mut env, &buf, offset, len) {
        Some(d) => s.session.submit_audio(pts_us as u64, d) as jboolean,
        None => JNI_FALSE,
    }
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_senderEncodeTime(_e: JNIEnv, _c: JClass, h: jlong, us: jint) {
    unsafe { &*(h as *const SenderH) }.session.report_encode_time_us(us.max(0) as u32);
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_senderCaptureDrop(_e: JNIEnv, _c: JClass, h: jlong) {
    unsafe { &*(h as *const SenderH) }.session.report_capture_drop();
}

/// `status` = Android `PowerManager.getCurrentThermalStatus()`.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_senderThermal(_e: JNIEnv, _c: JClass, h: jlong, status: jint) {
    let lvl = ThermalLevel::from_android(status);
    unsafe { &*(h as *const SenderH) }.session.update_device_signals(|d| d.thermal = Some(lvl));
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_senderBattery(_e: JNIEnv, _c: JClass, h: jlong, pct: jint, charging: jboolean, power_save: jboolean) {
    let b = Battery { percent: pct.clamp(0, 100) as u8, charging: charging != 0, power_save: power_save != 0 };
    unsafe { &*(h as *const SenderH) }.session.update_device_signals(|d| d.battery = Some(b));
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_senderSetSource(_e: JNIEnv, _c: JClass, h: jlong, w: jint, hh: jint, hz: jint) {
    unsafe { &*(h as *const SenderH) }
        .session
        .set_source(SourceInfo { width: w as u16, height: hh as u16, refresh_hz: hz.clamp(1, 1000) as u16 });
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_senderStats(env: JNIEnv, _c: JClass, h: jlong) -> jstring {
    let s = unsafe { &*(h as *const SenderH) };
    env.new_string(stats::sender_json(&s.session.stats())).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Session clock in microseconds (CLOCK_MONOTONIC) — same domain as MediaCodec surface timestamps.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_nowUs(_e: JNIEnv, _c: JClass) -> jlong {
    use revizor_session::Clock;
    MonotonicClock.now_us() as jlong
}

// ───────────────────────────── receiver ─────────────────────────────

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_app_revizor_core_Native_receiverStart(
    mut env: JNIEnv,
    _c: JClass,
    core: jlong,
    cb: JObject,
    port: jint,
    name: JString,
    codec_caps: JIntArray,
    audio_caps: JIntArray,
    tcp: jboolean,
) -> jlong {
    let core = unsafe { &*(core as *const Core) };
    let Some(callback) = Callback::new(&mut env, &cb) else { return 0 };
    let name = jstr(&mut env, &name);
    let codec_caps = int_vec(&mut env, &codec_caps);
    let audio_caps = int_vec(&mut env, &audio_caps);
    let tcp = tcp != 0;
    let caps = build_caps(&name, &codec_caps, &audio_caps, 200_000_000, true);
    let Ok(transport) = make_transport(tcp, &format!("0.0.0.0:{port}")) else { return 0 };
    let cfg = ReceiverConfig::new(core.identity.clone(), core.trust.clone(), caps);
    let session = Arc::new(ReceiverSession::start(cfg, transport, Arc::new(MonotonicClock), move |e| receiver_event(&callback, e)));
    let s2 = session.clone();
    let n2 = name.clone();
    let disc = DiscoveryResponder::start(
        DiscoveryResponder::default_bind(),
        broadcast_targets(revizor_proto::discovery::DISCOVERY_PORT),
        move || s2.announcement(&n2, port as u16),
    )
    .ok();
    Box::into_raw(Box::new(ReceiverH { session, _discovery: disc })) as jlong
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverStop(_e: JNIEnv, _c: JClass, h: jlong) {
    if h != 0 {
        drop(unsafe { Box::from_raw(h as *mut ReceiverH) });
    }
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverOpenPairing(env: JNIEnv, _c: JClass, h: jlong) -> jstring {
    let r = unsafe { &*(h as *const ReceiverH) };
    env.new_string(r.session.open_pairing()).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverClosePairing(_e: JNIEnv, _c: JClass, h: jlong) {
    unsafe { &*(h as *const ReceiverH) }.session.close_pairing();
}

/// Waits up to `timeoutMs` for a frame and copies it into `dst` (direct buffer).
/// `meta` receives `[ptsUs, keyframe, length, epoch, frameId]`. Returns the length, 0 on timeout, -1 if `dst` is too small.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverNextFrame(
    env: JNIEnv,
    _c: JClass,
    h: jlong,
    timeout_ms: jint,
    dst: JByteBuffer,
    meta: JLongArray,
) -> jint {
    let r = unsafe { &*(h as *const ReceiverH) };
    let Some(f) = r.session.next_frame(Duration::from_millis(timeout_ms.max(0) as u64)) else { return 0 };
    let (Ok(base), Ok(cap)) = (env.get_direct_buffer_address(&dst), env.get_direct_buffer_capacity(&dst)) else { return -1 };
    if f.data.len() > cap {
        return -1;
    }
    // SAFETY: length checked against capacity; buffer owned by the Kotlin caller for the duration of the call.
    unsafe { std::ptr::copy_nonoverlapping(f.data.as_ptr(), base, f.data.len()) };
    let m = [f.pts_us as i64, f.keyframe as i64, f.data.len() as i64, f.epoch as i64, f.frame_id as i64];
    let _ = env.set_long_array_region(&meta, 0, &m);
    f.data.len() as jint
}

/// Same contract as `receiverNextFrame` for audio; `meta` = `[ptsUs, 0, length, epoch, 0]`.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverNextAudio(env: JNIEnv, _c: JClass, h: jlong, dst: JByteBuffer, meta: JLongArray) -> jint {
    let r = unsafe { &*(h as *const ReceiverH) };
    let Some(f) = r.session.next_audio() else { return 0 };
    let (Ok(base), Ok(cap)) = (env.get_direct_buffer_address(&dst), env.get_direct_buffer_capacity(&dst)) else { return -1 };
    if f.data.len() > cap {
        return -1;
    }
    unsafe { std::ptr::copy_nonoverlapping(f.data.as_ptr(), base, f.data.len()) };
    let m = [f.pts_us as i64, 0, f.data.len() as i64, f.epoch as i64, 0];
    let _ = env.set_long_array_region(&meta, 0, &m);
    f.data.len() as jint
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverPresented(_e: JNIEnv, _c: JClass, h: jlong, pts_us: jlong, decode_us: jint) {
    unsafe { &*(h as *const ReceiverH) }.session.on_frame_presented(pts_us as u64, decode_us.max(0) as u32);
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverDecodeError(_e: JNIEnv, _c: JClass, h: jlong) {
    unsafe { &*(h as *const ReceiverH) }.session.report_decode_error();
}

/// `cpuPct` < 0 and `thermalStatus` < 0 mean "unknown".
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverSignals(_e: JNIEnv, _c: JClass, h: jlong, cpu_pct: jint, thermal_status: jint) {
    let cpu = (cpu_pct >= 0).then(|| cpu_pct.min(100) as u8);
    let th = (thermal_status >= 0).then(|| ThermalLevel::from_android(thermal_status));
    unsafe { &*(h as *const ReceiverH) }.session.update_device_signals(cpu, th);
}

#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverStats(env: JNIEnv, _c: JClass, h: jlong) -> jstring {
    let r = unsafe { &*(h as *const ReceiverH) };
    env.new_string(stats::receiver_json(&r.session.stats())).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// `sender_clock − local_clock` in µs, or `Long.MIN_VALUE` if the clocks are not synchronised yet.
#[no_mangle]
pub extern "system" fn Java_app_revizor_core_Native_receiverClockOffset(_e: JNIEnv, _c: JClass, h: jlong) -> jlong {
    unsafe { &*(h as *const ReceiverH) }.session.stats().clock_offset_us.unwrap_or(i64::MIN)
}
