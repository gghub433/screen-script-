//! Application controller: discovery, pairing, stream lifecycle. Platform neutral; the Windows capture
//! pipeline is plugged in under `cfg(windows)`.
//!
//! Two kinds of targets share one list:
//! * **Revizor receivers** (app installed on the other device): fast, adaptive, end-to-end encrypted, need pairing once;
//! * **TVs** (Google Cast / DLNA): nothing installed there, a few seconds of delay, plain HTTP limited to the TV.

use crate::pipeline::{EncoderInfo, FrameSink, PipelineCmd, RevizorSink, TvSink};
use crate::sources::Source;
use revizor_adaptive::Profile;
use revizor_cast::{CastConfig, CastEvent, CastSession, CastState, Tv};
use revizor_crypto::{FileTrustStore, Identity, TrustStore};
use revizor_proto::{AudioCodec, Capabilities, Codec, CodecCap, Platform, Preference, StreamParams, TransportKind};
use revizor_session::{pair_as_sender, MonotonicClock, PairError, SenderConfig, SenderEvent, SenderSession, SenderState, SenderStats, SourceInfo};
use revizor_transport::discovery::{broadcast_targets, scan, Found};
use revizor_transport::UdpTransport;
use serde::Serialize;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A device that did not answer one scan is still shown for this long (multicast replies get lost now and then),
/// so the list does not flicker and a selected entry does not vanish under the user's finger.
const KEEP_SEEN: Duration = Duration::from_secs(30);

struct Seen<T> {
    at: Instant,
    v: T,
}

/// Merges one scan's results into the remembered list: refreshes known entries, adds new ones, forgets stale ones.
fn merge_seen<T, K: PartialEq>(list: &mut Vec<Seen<T>>, fresh: Vec<T>, key: impl Fn(&T) -> K, now: Instant, keep: Duration) {
    for v in fresh {
        let k = key(&v);
        match list.iter_mut().find(|s| key(&s.v) == k) {
            Some(s) => {
                s.at = now;
                s.v = v;
            }
            None => list.push(Seen { at: now, v }),
        }
    }
    list.retain(|s| now.saturating_duration_since(s.at) <= keep);
}

#[derive(Serialize, Clone)]
pub struct ReceiverView {
    /// Revizor: device id. TV: `tv:<ip>`.
    pub id: String,
    /// `"revizor"` or `"tv"`.
    pub kind: &'static str,
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub pairing_open: bool,
    /// TVs need no pairing, so they are always "ready".
    pub trusted: bool,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    pub codecs: Vec<String>,
    /// TV: how we can reach it ("Google Cast", "DLNA").
    pub methods: Vec<String>,
    pub details: String,
}

#[derive(Serialize, Clone, Default)]
pub struct StreamView {
    pub state: String,
    pub error: Option<String>,
    pub receiver: String,
    pub mode: &'static str,
    pub notice: Option<String>,
    pub method: Option<String>,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_mbps: f32,
    pub codec: Option<String>,
    pub encoder: Option<EncoderInfo>,
    pub limited_by: Option<String>,
    pub stats: Option<serde_json::Value>,
}

#[derive(Default)]
struct Live {
    state: String,
    error: Option<String>,
    notice: Option<String>,
    params: Option<StreamParams>,
    limited_by: Option<String>,
}

enum Active {
    Revizor(Arc<SenderSession>),
    Tv(Arc<CastSession>),
}

struct Stream {
    active: Active,
    receiver_name: String,
    live: Arc<Mutex<Live>>,
    #[cfg(windows)]
    pipeline: Arc<Mutex<Option<crate::win::WinPipeline>>>,
}

pub struct Controller {
    pub device_name: String,
    identity: Arc<Identity>,
    trust: Arc<FileTrustStore>,
    found: Mutex<Vec<Seen<Found>>>,
    tvs: Mutex<Vec<Seen<Tv>>>,
    /// Receivers added by IP address; probed on every scan because broadcast may not reach them.
    manual: Mutex<Vec<IpAddr>>,
    scanning: AtomicBool,
    stream: Mutex<Option<Stream>>,
}

#[derive(Debug)]
pub enum ControlError {
    Msg(String),
}
impl<T: ToString> From<T> for ControlError {
    fn from(t: T) -> Self {
        ControlError::Msg(t.to_string())
    }
}
pub type Res<T> = Result<T, ControlError>;
fn err<T>(m: &str) -> Res<T> {
    Err(ControlError::Msg(m.to_string()))
}

pub fn profile_from_str(s: &str) -> Profile {
    match s {
        "quality" => Profile::Quality,
        "low-latency" => Profile::LowLatency,
        "battery" => Profile::BatterySaver,
        _ => Profile::Balanced,
    }
}

impl Controller {
    pub fn open(dir: PathBuf, device_name: String) -> Res<Self> {
        std::fs::create_dir_all(&dir)?;
        let key = dir.join("identity.key");
        let identity = match std::fs::read_to_string(&key)
            .ok()
            .and_then(|s| revizor_crypto::identity::unhex(s.trim()))
            .and_then(|v| <[u8; 32]>::try_from(v).ok())
        {
            Some(b) => Identity::from_secret(b),
            None => {
                let id = Identity::generate();
                std::fs::write(&key, revizor_crypto::identity::hex(&id.secret_bytes()))?;
                id
            }
        };
        let trust = FileTrustStore::open(dir.join("trusted.tsv"))?;
        Ok(Self {
            device_name,
            identity: Arc::new(identity),
            trust: Arc::new(trust),
            found: Mutex::new(vec![]),
            tvs: Mutex::new(vec![]),
            manual: Mutex::new(vec![]),
            scanning: AtomicBool::new(false),
            stream: Mutex::new(None),
        })
    }

    pub fn device_id(&self) -> String {
        self.identity.device_id()
    }

    pub fn trusted(&self) -> Vec<(String, String)> {
        self.trust.list().into_iter().map(|d| (d.id(), d.name)).collect()
    }

    pub fn forget(&self, id: &str) -> bool {
        self.trust.remove(id)
    }

    pub fn scanning(&self) -> bool {
        self.scanning.load(Ordering::SeqCst)
    }

    /// Starts a scan in the background and returns at once, so the UI keeps updating (a TV scan takes seconds).
    /// Returns false if one is already running.
    pub fn scan_async(self: &Arc<Self>, ms: u64) -> bool {
        if self.scanning.swap(true, Ordering::SeqCst) {
            return false;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let _ = me.scan(ms);
            me.scanning.store(false, Ordering::SeqCst);
        });
        true
    }

    /// Looks for Revizor receivers and standard TVs at the same time.
    pub fn scan(&self, ms: u64) -> Res<()> {
        let mut targets = broadcast_targets(revizor_proto::discovery::DISCOVERY_PORT);
        targets.extend(self.manual.lock().unwrap().iter().map(|ip| SocketAddr::new(*ip, revizor_proto::discovery::DISCOVERY_PORT)));
        let (revizor, tvs) = std::thread::scope(|s| {
            let a = s.spawn(|| scan(&targets, Duration::from_millis(ms)));
            // mDNS / SSDP replies trickle in over a second or two.
            let b = s.spawn(|| revizor_cast::scan_tvs(Duration::from_millis(ms.max(2500))));
            (a.join(), b.join())
        });
        let revizor = revizor.map_err(|_| ControlError::Msg("discovery crashed".into()))??;
        let now = Instant::now();
        merge_seen(&mut self.found.lock().unwrap(), revizor, |f| f.announcement.device_id.clone(), now, KEEP_SEEN);
        merge_seen(&mut self.tvs.lock().unwrap(), tvs.unwrap_or_default(), |t| t.ip, now, KEEP_SEEN);
        Ok(())
    }

    pub fn receivers(&self) -> Vec<ReceiverView> {
        let trusted: Vec<String> = self.trust.list().iter().map(|d| d.id()).collect();
        let found = self.found.lock().unwrap();
        let mut out: Vec<ReceiverView> = found
            .iter()
            .map(|s| {
                let f = &s.v;
                let a = &f.announcement;
                ReceiverView {
                    id: a.device_id.clone(),
                    kind: "revizor",
                    name: a.name.clone(),
                    ip: f.addr.to_string(),
                    port: a.media_port,
                    pairing_open: a.pairing_open,
                    trusted: trusted.contains(&a.device_id),
                    max_width: a.max_width,
                    max_height: a.max_height,
                    max_fps: a.max_fps,
                    codecs: a.codecs.iter().map(|c| format!("{c:?}")).collect(),
                    methods: vec![],
                    details: String::new(),
                }
            })
            .collect();
        // A TV that runs a paired Revizor receiver is listed once, as the better (encrypted, low-delay) entry.
        let trusted_ips: Vec<IpAddr> = found.iter().filter(|s| trusted.contains(&s.v.announcement.device_id)).map(|s| s.v.addr).collect();
        for t in self.tvs.lock().unwrap().iter().map(|s| &s.v).filter(|t| !trusted_ips.contains(&t.ip)) {
            out.push(ReceiverView {
                id: format!("tv:{}", t.ip),
                kind: "tv",
                name: t.name.clone(),
                ip: t.ip.to_string(),
                port: 0,
                pairing_open: false,
                trusted: true,
                max_width: 0,
                max_height: 0,
                max_fps: 0,
                codecs: vec![],
                methods: t.methods.iter().map(|m| m.label().to_string()).collect(),
                details: [t.manufacturer.clone(), t.model.clone()].into_iter().flatten().collect::<Vec<_>>().join(" "),
            });
        }
        out
    }

    fn find(&self, id: &str) -> Res<Found> {
        self.found.lock().unwrap().iter().find(|s| s.v.announcement.device_id == id).map(|s| s.v.clone()).ok_or_else(|| ControlError::Msg("Receiver not found. Scan again.".into()))
    }

    /// Manual connection for advanced users (no discovery).
    pub fn add_manual(&self, ip: &str) -> Res<()> {
        let ip: IpAddr = ip.trim().parse().map_err(|_| ControlError::Msg("Not a valid IP address".into()))?;
        let found = scan(&[SocketAddr::new(ip, revizor_proto::discovery::DISCOVERY_PORT)], Duration::from_millis(1500))?;
        if found.is_empty() {
            return err("No Revizor receiver answered at that address");
        }
        merge_seen(&mut self.found.lock().unwrap(), found, |f| f.announcement.device_id.clone(), Instant::now(), KEEP_SEEN);
        let mut m = self.manual.lock().unwrap();
        if !m.contains(&ip) {
            m.push(ip);
        }
        Ok(())
    }

    pub fn pair(&self, receiver_id: &str, pin: &str) -> Res<()> {
        let f = self.find(receiver_id)?;
        let t = UdpTransport::bind("0.0.0.0:0".parse().unwrap())?;
        pair_as_sender(&t, SocketAddr::new(f.addr, f.announcement.media_port), self.identity.clone(), &self.device_name, pin, self.trust.clone())
            .map_err(|e| match e {
                PairError::WrongPin => ControlError::Msg("Wrong PIN. Check the code on the receiver.".into()),
                PairError::NoAnswer => ControlError::Msg("The receiver did not answer. Is pairing open on it?".into()),
                PairError::Io(m) => ControlError::Msg(m),
            })
    }

    fn caps(&self) -> Capabilities {
        #[cfg(windows)]
        let hw = crate::win::probe_hardware_h264().is_some();
        #[cfg(not(windows))]
        let hw = false;
        Capabilities {
            device_name: self.device_name.clone(),
            platform: Platform::Windows,
            codecs: vec![CodecCap { codec: Codec::H264, max_width: 4096, max_height: 2304, max_fps: 120, hardware: hw }],
            // Audio capture on Windows is not implemented yet; do not advertise it.
            audio: Vec::<AudioCodec>::new(),
            transports: TransportKind::Udp.bit(),
            hdr: false,
            max_bitrate_bps: 100_000_000,
        }
    }

    pub fn start(&self, receiver_id: &str, source: &Source, profile: Profile) -> Res<()> {
        if self.stream.lock().unwrap().is_some() {
            return err("A stream is already running");
        }
        if let Some(ip) = receiver_id.strip_prefix("tv:") {
            return self.start_tv(ip, source);
        }
        self.start_revizor(receiver_id, source, profile)
    }

    fn start_revizor(&self, receiver_id: &str, source: &Source, profile: Profile) -> Res<()> {
        let mut slot = self.stream.lock().unwrap();
        let f = self.find(receiver_id)?;
        if !self.trusted().iter().any(|(id, _)| id == receiver_id) {
            return err("Pair with this receiver first");
        }
        let peer = SocketAddr::new(f.addr, f.announcement.media_port);
        let transport = Arc::new(UdpTransport::bind("0.0.0.0:0".parse().unwrap())?);
        let live = Arc::new(Mutex::new(Live { state: "connecting".into(), ..Default::default() }));
        let (cmd_tx, cmd_rx) = channel::<PipelineCmd>();
        let session_cell: Arc<OnceLock<Arc<SenderSession>>> = Arc::new(OnceLock::new());

        #[cfg(windows)]
        let pipeline: Arc<Mutex<Option<crate::win::WinPipeline>>> = Arc::new(Mutex::new(None));
        #[cfg(windows)]
        let pipe_for_cb = pipeline.clone();
        let cmd_rx_cell = Arc::new(Mutex::new(Some(cmd_rx)));
        let cmd_tx_cb: Sender<PipelineCmd> = cmd_tx.clone();
        let live_cb = live.clone();
        let source_id = source.id.clone();
        let refresh = source.refresh_hz.max(30) as u16;
        let cell_cb = session_cell.clone();

        let cfg = SenderConfig {
            identity: self.identity.clone(),
            trust: self.trust.clone(),
            caps: self.caps(),
            pref: Preference { codec_order: vec![Codec::H264], want_audio: false, max_short_side: None, target_fps: 60, target_bitrate_bps: None },
            profile,
            custom_tier: None,
            custom_bitrate: None,
            source: SourceInfo { width: source.width as u16, height: source.height as u16, refresh_hz: refresh },
            peer,
            size_align: 2,
            give_up_after: Duration::from_secs(45),
        };
        let clock: Arc<dyn revizor_session::Clock> = Arc::new(MonotonicClock);
        let clock_cb = clock.clone();
        let session = Arc::new(SenderSession::start(cfg, transport, clock, move |e| {
            #[cfg(not(windows))]
            let _ = (&clock_cb, &source_id, &cmd_rx_cell, &cell_cb, refresh);
            match e {
                SenderEvent::State(s) => {
                    let mut l = live_cb.lock().unwrap();
                    match s {
                        SenderState::Connecting => l.state = "connecting".into(),
                        SenderState::Streaming => l.state = "streaming".into(),
                        SenderState::Reconnecting { .. } => l.state = "reconnecting".into(),
                        SenderState::Stopped => l.state = "stopped".into(),
                        SenderState::Failed(m) => {
                            l.state = "failed".into();
                            l.error = Some(m);
                            // Nobody is listening any more: release the screen capture and the encoder.
                            let _ = cmd_tx_cb.send(PipelineCmd::Stop);
                        }
                    }
                }
                SenderEvent::Reconfigure { params, limited_by, .. } => {
                    {
                        let mut l = live_cb.lock().unwrap();
                        l.params = Some(params);
                        l.limited_by = limited_by.map(|r| format!("{r:?}"));
                    }
                    #[cfg(windows)]
                    {
                        let mut g = pipe_for_cb.lock().unwrap();
                        if let Some(p) = g.as_ref() {
                            p.send(PipelineCmd::Reconfigure(params));
                        } else if let Some(rx) = cmd_rx_cell.lock().unwrap().take() {
                            // first configuration: start capture + encoder
                            let mut waited = 0;
                            while cell_cb.get().is_none() && waited < 200 {
                                std::thread::sleep(Duration::from_millis(5));
                                waited += 1;
                            }
                            if let Some(s) = cell_cb.get() {
                                let live_err = live_cb.clone();
                                let sink: Arc<dyn FrameSink> = Arc::new(RevizorSink(s.clone()));
                                *g = Some(crate::win::WinPipeline::start(
                                    source_id.clone(),
                                    refresh,
                                    sink,
                                    clock_cb.clone(),
                                    params,
                                    crate::win::Mode::Revizor,
                                    (cmd_tx_cb.clone(), rx),
                                    Arc::new(move |m| {
                                        let mut l = live_err.lock().unwrap();
                                        l.state = "failed".into();
                                        l.error = Some(m);
                                    }),
                                ));
                            }
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        let mut l = live_cb.lock().unwrap();
                        l.state = "failed".into();
                        l.error = Some("Screen capture is only available on Windows.".into());
                    }
                }
                SenderEvent::SetBitrate { video_bps } => {
                    let _ = cmd_tx_cb.send(PipelineCmd::SetBitrate(video_bps));
                }
                SenderEvent::RequestKeyframe(_) => {
                    let _ = cmd_tx_cb.send(PipelineCmd::ForceKeyframe);
                }
                SenderEvent::PeerInfo { .. } => {}
            }
        }));
        let _ = session_cell.set(session.clone());
        *slot = Some(Stream {
            active: Active::Revizor(session),
            receiver_name: f.announcement.name.clone(),
            live,
            #[cfg(windows)]
            pipeline,
        });
        Ok(())
    }

    /// Cast to a TV that has nothing of Revizor installed.
    fn start_tv(&self, ip: &str, source: &Source) -> Res<()> {
        let tv = self.tvs.lock().unwrap().iter().map(|s| &s.v).find(|t| t.ip.to_string() == ip).cloned().ok_or_else(|| ControlError::Msg("TV not found. Search again.".into()))?;
        let live = Arc::new(Mutex::new(Live { state: "connecting".into(), ..Default::default() }));
        let (cmd_tx, cmd_rx) = channel::<PipelineCmd>();
        let (live_cb, tx_cb) = (live.clone(), cmd_tx.clone());
        let session = Arc::new(CastSession::start(&tv, CastConfig::default(), move |e| match e {
            CastEvent::State(s) => {
                let mut l = live_cb.lock().unwrap();
                match s {
                    CastState::Preparing | CastState::Starting => l.state = "connecting".into(),
                    CastState::Playing => {
                        l.state = "streaming".into();
                        l.notice = None;
                    }
                    CastState::Buffering => {
                        l.state = "streaming".into();
                        l.notice = Some("The TV is buffering…".into());
                    }
                    CastState::Reconnecting => l.state = "reconnecting".into(),
                    CastState::Stopped => l.state = "stopped".into(),
                    CastState::Failed(m) => {
                        l.state = "failed".into();
                        l.error = Some(m);
                        // The TV is gone or refused: release the screen capture and the encoder.
                        let _ = tx_cb.send(PipelineCmd::Stop);
                    }
                }
            }
            CastEvent::TryingNext { method } => live_cb.lock().unwrap().notice = Some(format!("That did not work, trying {method}…")),
            CastEvent::NeedKeyframe => {
                let _ = tx_cb.send(PipelineCmd::ForceKeyframe);
            }
        })?);

        // Fixed 30 fps stream, at most 1080p, aspect ratio kept; the TV decides how much it buffers.
        let (w, h) = revizor_proto::geometry::fit_short_side(source.width as u16, source.height as u16, 1080, 2);
        let bitrate = ((w as u64 * h as u64 * 30) / 10).clamp(3_000_000, 8_000_000) as u32;
        let params = StreamParams {
            epoch: 1,
            video: revizor_proto::VideoParams { codec: Codec::H264, width: w, height: h, fps: 30, bitrate_bps: bitrate, keyframe_interval_ms: 1000 },
            audio: revizor_proto::AudioParams::NONE,
        };
        live.lock().unwrap().params = Some(params);

        #[cfg(windows)]
        let pipeline = {
            let live_err = live.clone();
            let sink: Arc<dyn FrameSink> = Arc::new(TvSink(session.clone()));
            let clock: Arc<dyn revizor_session::Clock> = Arc::new(MonotonicClock);
            Arc::new(Mutex::new(Some(crate::win::WinPipeline::start(
                source.id.clone(),
                source.refresh_hz.max(30) as u16,
                sink,
                clock,
                params,
                crate::win::Mode::Tv,
                (cmd_tx, cmd_rx),
                Arc::new(move |m| {
                    let mut l = live_err.lock().unwrap();
                    l.state = "failed".into();
                    l.error = Some(m);
                }),
            ))))
        };
        #[cfg(not(windows))]
        {
            let _ = (&cmd_tx, &cmd_rx, TvSink(session.clone()));
            let mut l = live.lock().unwrap();
            l.state = "failed".into();
            l.error = Some("Screen capture is only available on Windows.".into());
        }
        *self.stream.lock().unwrap() = Some(Stream {
            active: Active::Tv(session),
            receiver_name: tv.name.clone(),
            live,
            #[cfg(windows)]
            pipeline,
        });
        Ok(())
    }

    pub fn stop(&self) {
        let s = self.stream.lock().unwrap().take();
        if let Some(s) = s {
            #[cfg(windows)]
            if let Some(p) = s.pipeline.lock().unwrap().take() {
                p.stop();
            }
            match s.active {
                Active::Revizor(sess) => {
                    if let Ok(sess) = Arc::try_unwrap(sess) {
                        sess.stop();
                    }
                }
                Active::Tv(sess) => {
                    if let Ok(sess) = Arc::try_unwrap(sess) {
                        sess.stop();
                    }
                }
            }
        }
    }

    pub fn stream_view(&self) -> Option<StreamView> {
        let g = self.stream.lock().unwrap();
        let s = g.as_ref()?;
        let l = s.live.lock().unwrap();
        let mut v = StreamView { state: l.state.clone(), error: l.error.clone(), receiver: s.receiver_name.clone(), notice: l.notice.clone(), ..Default::default() };
        if let Some(p) = l.params {
            v.width = p.video.width;
            v.height = p.video.height;
            v.fps = p.video.fps;
            v.bitrate_mbps = p.video.bitrate_bps as f32 / 1e6;
            v.codec = Some(format!("{:?}", p.video.codec));
        }
        v.limited_by = l.limited_by.clone();
        #[cfg(windows)]
        {
            v.encoder = s.pipeline.lock().unwrap().as_ref().map(|p| p.info.lock().unwrap().clone());
        }
        match &s.active {
            Active::Revizor(sess) => {
                v.mode = "revizor";
                v.stats = Some(stats_json(&sess.stats()));
            }
            Active::Tv(sess) => {
                v.mode = "tv";
                let st = sess.stats();
                v.method = Some(st.method.to_string());
                v.stats = Some(serde_json::json!({
                    "stream_mbps": st.in_bps as f32 / 1e6,
                    "sent_mb": st.bytes_served as f32 / 1e6,
                    "viewers": st.viewers,
                    "segments": st.segments,
                    "frames": st.frames_in,
                    "uptime_s": st.uptime_s,
                    "playlist_requests": st.playlist_requests,
                    "segment_requests": st.segment_requests,
                    "blocked": st.rejected_requests,
                }));
            }
        }
        Some(v)
    }
}

/// Only measured values; unknown ones are `null`.
pub fn stats_json(s: &SenderStats) -> serde_json::Value {
    let r = s.last_report;
    serde_json::json!({
        "sent_mbps": s.sent_bps as f32 / 1e6,
        "rtt_ms": s.rtt_us.map(|v| v as f32 / 1000.0),
        "encode_ms": s.encode_us_avg.map(|v| v as f32 / 1000.0),
        "loss_pct": r.map(|r| if r.packets_expected > 0 { 100.0 * r.packets_lost as f32 / r.packets_expected as f32 } else { 0.0 }),
        "jitter_ms": r.map(|r| r.jitter_us as f32 / 1000.0),
        "latency_ms": r.and_then(|r| (r.e2e_latency_us > 0).then_some(r.e2e_latency_us as f32 / 1000.0)),
        "decode_ms": r.and_then(|r| (r.decode_us > 0).then_some(r.decode_us as f32 / 1000.0)),
        "frames_sent": s.frames_sent,
        "frames_dropped": s.frames_dropped_sender,
        "retransmitted": s.retransmitted_packets,
        "keyframes": s.keyframes_sent,
        "fec_k": s.fec_k,
        "hardware_codecs": s.hardware_codecs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use revizor_cast::Method;
    use revizor_crypto::identity::{device_id, TrustedDevice};
    use revizor_proto::discovery::Announcement;

    fn controller(tag: &str) -> (Arc<Controller>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rvz-ctl-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Arc::new(Controller::open(dir.clone(), "Test PC".into()).unwrap()), dir)
    }

    fn found(id: &str, name: &str, ip: &str, pairing_open: bool) -> Found {
        Found {
            announcement: Announcement {
                proto_version: 1,
                device_id: id.into(),
                name: name.into(),
                media_port: 5555,
                transports: 1,
                codecs: vec![Codec::H264],
                max_width: 1920,
                max_height: 1080,
                max_fps: 60,
                pairing_open,
            },
            addr: ip.parse().unwrap(),
        }
    }

    fn tv(name: &str, ip: &str, methods: Vec<Method>) -> Tv {
        Tv { name: name.into(), ip: ip.parse().unwrap(), model: Some("X1".into()), manufacturer: Some("Acme".into()), methods }
    }

    fn add_tvs(ctl: &Controller, tvs: Vec<Tv>) {
        merge_seen(&mut ctl.tvs.lock().unwrap(), tvs, |t| t.ip, Instant::now(), KEEP_SEEN);
    }

    fn add_found(ctl: &Controller, f: Vec<Found>) {
        merge_seen(&mut ctl.found.lock().unwrap(), f, |f| f.announcement.device_id.clone(), Instant::now(), KEEP_SEEN);
    }

    #[test]
    fn merge_seen_refreshes_adds_and_forgets_old_entries() {
        let t0 = Instant::now();
        let mut list: Vec<Seen<(u32, &str)>> = vec![];
        merge_seen(&mut list, vec![(1, "a"), (2, "b")], |v| v.0, t0, KEEP_SEEN);
        assert_eq!(list.len(), 2);

        // 20 s later only #2 answers: #1 missed a scan but is still listed; #2 is refreshed with its new data.
        merge_seen(&mut list, vec![(2, "b2")], |v| v.0, t0 + Duration::from_secs(20), KEEP_SEEN);
        assert_eq!(list.len(), 2);
        assert_eq!(list.iter().find(|s| s.v.0 == 2).unwrap().v.1, "b2");

        // 35 s after the start #1 has been silent for longer than KEEP_SEEN and goes away; #2 (seen 15 s ago) stays.
        merge_seen(&mut list, vec![], |v| v.0, t0 + Duration::from_secs(35), KEEP_SEEN);
        assert_eq!(list.iter().map(|s| s.v.0).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn a_tv_needs_no_pairing_and_shows_how_it_will_be_reached() {
        let (ctl, dir) = controller("tv");
        add_tvs(
            &ctl,
            vec![tv("Living room", "192.168.1.20", vec![Method::Cast { port: 8009 }, Method::Dlna { control_url: "http://192.168.1.20:80/ctl".into(), service_type: "urn:schemas-upnp-org:service:AVTransport:1".into() }])],
        );
        let r = ctl.receivers();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].kind, "tv");
        assert_eq!(r[0].id, "tv:192.168.1.20");
        assert!(r[0].trusted, "a TV is never gated behind pairing");
        assert!(!r[0].pairing_open);
        assert_eq!(r[0].methods, vec!["Google Cast", "DLNA"]);
        assert_eq!(r[0].details, "Acme X1");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tv_that_runs_a_paired_revizor_receiver_is_listed_once() {
        let (ctl, dir) = controller("dup");
        let public = [7u8; 32];
        ctl.trust.add(TrustedDevice { public, name: "Living room".into() });
        let id = device_id(&public);
        add_found(&ctl, vec![found(&id, "Living room", "192.168.1.20", false)]);
        add_tvs(&ctl, vec![tv("Living room", "192.168.1.20", vec![Method::Cast { port: 8009 }])]);
        let r = ctl.receivers();
        assert_eq!(r.len(), 1, "{:?}", r.iter().map(|x| &x.id).collect::<Vec<_>>());
        assert_eq!(r[0].kind, "revizor");
        assert!(r[0].trusted);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unpaired_revizor_receiver_and_its_tv_entry_are_both_offered() {
        let (ctl, dir) = controller("both");
        add_found(&ctl, vec![found("aabbccdd", "Bedroom TV", "192.168.1.30", true)]);
        add_tvs(&ctl, vec![tv("Bedroom TV", "192.168.1.30", vec![Method::Cast { port: 8009 }])]);
        let mut kinds: Vec<(&str, bool)> = ctl.receivers().iter().map(|r| (r.kind, r.trusted)).collect();
        kinds.sort();
        assert_eq!(kinds, vec![("revizor", false), ("tv", true)]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn starting_on_a_tv_that_is_gone_fails_with_a_clear_message() {
        let (ctl, dir) = controller("gone");
        let src = Source { id: "m:1".into(), name: "Display".into(), kind: crate::sources::SourceKind::Monitor { index: 0 }, width: 1920, height: 1080, refresh_hz: 60 };
        let ControlError::Msg(m) = ctl.start("tv:10.9.9.9", &src, Profile::Balanced).unwrap_err();
        assert!(m.contains("TV not found"), "{m}");
        assert!(ctl.stream_view().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_one_background_scan_runs_at_a_time() {
        let (ctl, dir) = controller("scan");
        assert!(ctl.scan_async(100));
        assert!(ctl.scanning());
        assert!(!ctl.scan_async(100), "a second scan must not start while one is running");
        let t0 = Instant::now();
        while ctl.scanning() && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!ctl.scanning(), "scan did not finish");
        assert!(ctl.scan_async(100), "a new scan can start after the previous one finished");
        let t0 = Instant::now();
        while ctl.scanning() && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
