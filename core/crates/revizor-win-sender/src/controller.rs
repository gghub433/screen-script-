//! Application controller: discovery, pairing, stream lifecycle. Platform
//! neutral; the Windows capture pipeline is plugged in under `cfg(windows)`.

use crate::pipeline::{EncoderInfo, PipelineCmd};
use crate::sources::Source;
use revizor_adaptive::Profile;
use revizor_crypto::{FileTrustStore, Identity, TrustStore};
use revizor_proto::{AudioCodec, Capabilities, Codec, CodecCap, Platform, Preference, StreamParams, TransportKind};
use revizor_session::{
    pair_as_sender, MonotonicClock, PairError, SenderConfig, SenderEvent, SenderSession, SenderState, SenderStats, SourceInfo,
};
use revizor_transport::discovery::{broadcast_targets, scan, Found};
use revizor_transport::UdpTransport;
use serde::Serialize;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[derive(Serialize, Clone)]
pub struct ReceiverView {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub pairing_open: bool,
    pub trusted: bool,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    pub codecs: Vec<String>,
}

#[derive(Serialize, Clone, Default)]
pub struct StreamView {
    pub state: String,
    pub error: Option<String>,
    pub receiver: String,
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
    params: Option<StreamParams>,
    limited_by: Option<String>,
}

struct Stream {
    session: Arc<SenderSession>,
    receiver_name: String,
    live: Arc<Mutex<Live>>,
    #[cfg(windows)]
    pipeline: Arc<Mutex<Option<crate::win::WinPipeline>>>,
}

pub struct Controller {
    pub device_name: String,
    identity: Arc<Identity>,
    trust: Arc<FileTrustStore>,
    found: Mutex<Vec<Found>>,
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
        Ok(Self { device_name, identity: Arc::new(identity), trust: Arc::new(trust), found: Mutex::new(vec![]), stream: Mutex::new(None) })
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

    pub fn scan(&self, ms: u64) -> Res<()> {
        let found = scan(&broadcast_targets(revizor_proto::discovery::DISCOVERY_PORT), Duration::from_millis(ms))?;
        *self.found.lock().unwrap() = found;
        Ok(())
    }

    pub fn receivers(&self) -> Vec<ReceiverView> {
        let trusted: Vec<String> = self.trust.list().iter().map(|d| d.id()).collect();
        self.found
            .lock()
            .unwrap()
            .iter()
            .map(|f| {
                let a = &f.announcement;
                ReceiverView {
                    id: a.device_id.clone(),
                    name: a.name.clone(),
                    ip: f.addr.to_string(),
                    port: a.media_port,
                    pairing_open: a.pairing_open,
                    trusted: trusted.contains(&a.device_id),
                    max_width: a.max_width,
                    max_height: a.max_height,
                    max_fps: a.max_fps,
                    codecs: a.codecs.iter().map(|c| format!("{c:?}")).collect(),
                }
            })
            .collect()
    }

    fn find(&self, id: &str) -> Res<Found> {
        self.found.lock().unwrap().iter().find(|f| f.announcement.device_id == id).cloned().ok_or_else(|| ControlError::Msg("Receiver not found. Scan again.".into()))
    }

    /// Manual connection for advanced users (no discovery).
    pub fn add_manual(&self, ip: &str) -> Res<()> {
        let ip: IpAddr = ip.parse().map_err(|_| ControlError::Msg("Not a valid IP address".into()))?;
        let found = scan(&[SocketAddr::new(ip, revizor_proto::discovery::DISCOVERY_PORT)], Duration::from_millis(1500))?;
        let mut g = self.found.lock().unwrap();
        let mut hit = false;
        for f in found {
            hit = true;
            g.retain(|x| x.announcement.device_id != f.announcement.device_id);
            g.push(f);
        }
        if hit { Ok(()) } else { err("No Revizor receiver answered at that address") }
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
        let (hw, _name) = match crate::win::probe_hardware_h264() {
            Some(n) => (true, n),
            None => (false, "Software H.264".to_string()),
        };
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
        let mut slot = self.stream.lock().unwrap();
        if slot.is_some() {
            return err("A stream is already running");
        }
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
                                *g = Some(crate::win::WinPipeline::start(
                                    source_id.clone(),
                                    refresh,
                                    s.clone(),
                                    clock_cb.clone(),
                                    params,
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
            session,
            receiver_name: f.announcement.name.clone(),
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
            if let Ok(sess) = Arc::try_unwrap(s.session) {
                sess.stop();
            }
        }
    }

    pub fn stream_view(&self) -> Option<StreamView> {
        let g = self.stream.lock().unwrap();
        let s = g.as_ref()?;
        let l = s.live.lock().unwrap();
        let st = s.session.stats();
        let mut v = StreamView { state: l.state.clone(), error: l.error.clone(), receiver: s.receiver_name.clone(), ..Default::default() };
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
        v.stats = Some(stats_json(&st));
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
