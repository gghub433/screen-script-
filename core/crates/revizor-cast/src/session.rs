//! One casting session: TS muxer + HTTP server + the control conversation with the TV.

use crate::castv2::{self, CastClient, Event as CastEvent2};
use crate::discover::{Method, Tv};
use crate::server::{Hub, Server, ServerConfig, DLNA_FEATURES};
use crate::ts::TsMuxer;
use crate::upnp::{AvTransport, TransportState};
use rand::RngCore;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

pub type Connector = Arc<dyn Fn(SocketAddr) -> std::io::Result<Box<dyn ReadWrite>> + Send + Sync>;

#[derive(Clone)]
pub struct CastConfig {
    pub has_audio: bool,
    /// Bind address of the HTTP server (default `0.0.0.0:0`).
    pub bind: SocketAddr,
    /// Override the address the TV should use to reach us (default: the local address routed towards the TV).
    pub advertise_ip: Option<IpAddr>,
    /// How to open the Cast control connection (default: TLS to the device).
    pub connector: Option<Connector>,
    /// Give up if the capture produces no keyframe within this time.
    pub first_frame_timeout: Duration,
    /// How long to wait for the TV to start fetching before trying the next method.
    pub tv_start_timeout: Duration,
}

impl Default for CastConfig {
    fn default() -> Self {
        Self {
            has_audio: false,
            bind: "0.0.0.0:0".parse().unwrap(),
            advertise_ip: None,
            connector: None,
            first_frame_timeout: Duration::from_secs(15),
            tv_start_timeout: Duration::from_secs(20),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastState {
    /// Waiting for the first keyframe from the encoder.
    Preparing,
    /// Talking to the TV, waiting for it to start fetching the stream.
    Starting,
    Playing,
    /// The TV is (re)buffering.
    Buffering,
    Reconnecting,
    Stopped,
    /// Unrecoverable; text is user-presentable.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastEvent {
    State(CastState),
    /// The first method failed and the next one is being tried.
    TryingNext { method: &'static str },
    /// A viewer joined and the stream needs a keyframe right now.
    NeedKeyframe,
}

#[derive(Debug, Clone, Default)]
pub struct CastStats {
    pub method: &'static str,
    pub frames_in: u64,
    pub bytes_in: u64,
    /// Bitrate of the encoded stream fed in, bits/s (measured over the last stats interval).
    pub in_bps: u64,
    pub bytes_served: u64,
    pub viewers: usize,
    pub segments: u64,
    pub playlist_requests: u64,
    pub segment_requests: u64,
    pub rejected_requests: u64,
    pub slow_viewers_dropped: u64,
    pub buffered_bytes: usize,
    pub uptime_s: u64,
}

struct Inner {
    mux: Mutex<TsMuxer>,
    hub: Arc<Hub>,
    stop: AtomicBool,
    state: Mutex<CastState>,
    events: Box<dyn Fn(CastEvent) + Send + Sync>,
    frames_in: AtomicU64,
    bytes_in: AtomicU64,
    rate: Mutex<(Instant, u64, u64)>,
    method: Mutex<&'static str>,
    started: Instant,
    ever_played: AtomicBool,
}

impl Inner {
    fn set(&self, s: CastState) {
        let mut g = self.state.lock().unwrap();
        if *g == s {
            return;
        }
        if matches!(*g, CastState::Failed(_) | CastState::Stopped) {
            return; // terminal
        }
        if s == CastState::Playing {
            self.ever_played.store(true, Ordering::SeqCst);
        }
        *g = s.clone();
        drop(g);
        (self.events)(CastEvent::State(s));
    }
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
    fn sleep(&self, d: Duration) {
        let end = Instant::now() + d;
        while !self.stopping() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20).min(end - Instant::now()));
        }
    }
}

pub struct CastSession {
    inner: Arc<Inner>,
    server: Option<Server>,
    threads: Vec<JoinHandle<()>>,
    base_url: String,
}

fn local_ip_toward(peer: IpAddr) -> Option<IpAddr> {
    let s = UdpSocket::bind(if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).ok()?;
    s.connect((peer, 9)).ok()?;
    Some(s.local_addr().ok()?.ip())
}

fn token() -> String {
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl CastSession {
    pub fn start(tv: &Tv, cfg: CastConfig, events: impl Fn(CastEvent) + Send + Sync + 'static) -> std::io::Result<Self> {
        if tv.methods.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "this TV offers no supported casting method"));
        }
        let events: Arc<dyn Fn(CastEvent) + Send + Sync> = Arc::new(events);
        let ev2 = events.clone();
        let tok = token();
        let server = Server::start(cfg.bind, ServerConfig::new(tok.clone(), vec![tv.ip]), move || ev2(CastEvent::NeedKeyframe))?;
        let ip = cfg.advertise_ip.or_else(|| local_ip_toward(tv.ip)).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no network route to the TV"))?;
        let base_url = format!("http://{}/{}", SocketAddr::new(ip, server.addr.port()), tok);
        let ev3 = events.clone();
        let inner = Arc::new(Inner {
            mux: Mutex::new(TsMuxer::new(cfg.has_audio)),
            hub: server.hub(),
            stop: AtomicBool::new(false),
            state: Mutex::new(CastState::Preparing),
            events: Box::new(move |e| ev3(e)),
            frames_in: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            rate: Mutex::new((Instant::now(), 0, 0)),
            method: Mutex::new(tv.methods[0].label()),
            started: Instant::now(),
            ever_played: AtomicBool::new(false),
        });
        (inner.events)(CastEvent::State(CastState::Preparing));

        let mut threads = Vec::new();
        // keep the TV's clock running while the screen is static
        {
            let i = inner.clone();
            threads.push(std::thread::Builder::new().name("rvz-cast-keepalive".into()).spawn(move || {
                while !i.stopping() {
                    let ts = i.mux.lock().unwrap().keepalive();
                    if !ts.is_empty() {
                        i.hub.push_other(ts);
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
            })?);
        }
        {
            let (i, tv, base, cfg) = (inner.clone(), tv.clone(), base_url.clone(), cfg.clone());
            threads.push(std::thread::Builder::new().name("rvz-cast-control".into()).spawn(move || control(i, tv, base, cfg))?);
        }
        Ok(Self { inner, server: Some(server), threads, base_url })
    }

    pub fn stream_base_url(&self) -> &str {
        &self.base_url
    }

    /// One H.264 access unit (Annex-B) in capture-clock microseconds.
    pub fn submit_video(&self, pts_us: u64, keyframe: bool, annexb: &[u8]) {
        if self.inner.stopping() {
            return;
        }
        let ts = self.inner.mux.lock().unwrap().video(pts_us, keyframe, annexb);
        if ts.is_empty() {
            return;
        }
        self.inner.frames_in.fetch_add(1, Ordering::Relaxed);
        self.inner.bytes_in.fetch_add(annexb.len() as u64, Ordering::Relaxed);
        self.inner.hub.push_video(pts_us, keyframe, ts);
    }

    /// One AAC frame with ADTS header.
    pub fn submit_audio(&self, pts_us: u64, adts: &[u8]) {
        if self.inner.stopping() {
            return;
        }
        let ts = self.inner.mux.lock().unwrap().audio(pts_us, adts);
        self.inner.hub.push_other(ts);
    }

    pub fn state(&self) -> CastState {
        self.inner.state.lock().unwrap().clone()
    }

    pub fn stats(&self) -> CastStats {
        let i = &self.inner;
        let s = &i.hub.stats;
        let now = Instant::now();
        let bytes = i.bytes_in.load(Ordering::Relaxed);
        let in_bps = {
            let mut r = i.rate.lock().unwrap();
            let dt = now.duration_since(r.0).as_secs_f64();
            if dt >= 1.0 {
                r.2 = ((bytes - r.1) as f64 * 8.0 / dt) as u64;
                r.0 = now;
                r.1 = bytes;
            }
            r.2
        };
        CastStats {
            method: *i.method.lock().unwrap(),
            frames_in: i.frames_in.load(Ordering::Relaxed),
            bytes_in: bytes,
            in_bps,
            bytes_served: s.bytes_sent.load(Ordering::Relaxed),
            viewers: i.hub.viewers(),
            segments: i.hub.segments_made(),
            playlist_requests: s.playlist_requests.load(Ordering::Relaxed),
            segment_requests: s.segment_requests.load(Ordering::Relaxed),
            rejected_requests: s.rejected.load(Ordering::Relaxed),
            slow_viewers_dropped: s.slow_viewers_dropped.load(Ordering::Relaxed),
            buffered_bytes: i.hub.buffered_bytes(),
            uptime_s: i.started.elapsed().as_secs(),
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        if let Some(s) = self.server.take() {
            s.stop();
        }
        let terminal = matches!(*self.inner.state.lock().unwrap(), CastState::Failed(_) | CastState::Stopped);
        if !terminal {
            *self.inner.state.lock().unwrap() = CastState::Stopped;
            (self.inner.events)(CastEvent::State(CastState::Stopped));
        }
    }
}

impl Drop for CastSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ───────────────────────────── control thread ─────────────────────────────

fn control(i: Arc<Inner>, tv: Tv, base: String, cfg: CastConfig) {
    // 1. wait for the encoder
    let t0 = Instant::now();
    while !i.mux.lock().unwrap().started() {
        if i.stopping() {
            return;
        }
        if t0.elapsed() > cfg.first_frame_timeout {
            i.set(CastState::Failed("Screen capture produced no picture, so there is nothing to send to the TV.".into()));
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // 2. try each method in turn until one plays
    let mut errors: Vec<(&'static str, String)> = Vec::new();
    for (n, m) in tv.methods.iter().enumerate() {
        if i.stopping() {
            return;
        }
        *i.method.lock().unwrap() = m.label();
        if n > 0 {
            (i.events)(CastEvent::TryingNext { method: m.label() });
        }
        i.set(CastState::Starting);
        let r = match m {
            Method::Dlna { control_url, service_type } => run_dlna(&i, &tv, &base, &cfg, control_url, service_type),
            Method::Cast { port } => run_cast(&i, &tv, &base, &cfg, *port),
        };
        match r {
            Outcome::Stopped => return,
            Outcome::Failed(msg) => {
                errors.push((m.label(), msg));
                if i.ever_played.load(Ordering::SeqCst) {
                    break; // it worked and then broke: do not switch methods mid-session
                }
            }
        }
    }
    i.set(CastState::Failed(failure_text(&errors)));
}

/// One method: its message as is. Several methods tried: every message with its method, so the user sees all of it.
fn failure_text(errors: &[(&'static str, String)]) -> String {
    match errors {
        [] => "Casting failed.".to_string(),
        [(_, msg)] => msg.clone(),
        many => many.iter().map(|(m, e)| format!("{m}: {e}")).collect::<Vec<_>>().join(" "),
    }
}

enum Outcome {
    Stopped,
    Failed(String),
}

fn run_dlna(i: &Arc<Inner>, tv: &Tv, base: &str, cfg: &CastConfig, control_url: &str, service_type: &str) -> Outcome {
    let av = AvTransport::new(control_url, service_type);
    let url = format!("{base}/live.ts");
    let tvname = &tv.name;
    let _ = av.stop(); // some renderers refuse a new URI while "playing" something else
    let attempt = |av: &AvTransport| -> Result<(), String> {
        av.set_uri(&url, "Revizor", DLNA_FEATURES).map_err(|e| format!("{tvname}: {e}"))?;
        av.play().map_err(|e| format!("{tvname}: {e}"))
    };
    if let Err(e) = attempt(&av) {
        return Outcome::Failed(e);
    }
    let started = Instant::now();
    let mut attempts = 1;
    let mut net_errors = 0;
    let mut last_viewer = Instant::now();
    loop {
        if i.stopping() {
            let _ = av.stop();
            return Outcome::Stopped;
        }
        i.sleep(Duration::from_millis(1000));
        if i.stopping() {
            continue;
        }
        let viewers = i.hub.viewers();
        let st = av.state();
        match &st {
            Ok(_) => net_errors = 0,
            Err(_) => net_errors += 1,
        }
        if viewers > 0 {
            last_viewer = Instant::now();
            let buffering = matches!(st, Ok(TransportState::Transitioning));
            i.set(if buffering { CastState::Buffering } else { CastState::Playing });
            continue;
        }
        if i.ever_played.load(Ordering::SeqCst) {
            // the TV closed its connection
            match st {
                Ok(TransportState::Stopped) | Ok(TransportState::NoMedia) => return Outcome::Failed(format!("{tvname} stopped playing.")),
                _ if last_viewer.elapsed() > Duration::from_secs(15) => return Outcome::Failed(format!("{tvname} stopped receiving the picture.")),
                _ => i.set(CastState::Reconnecting),
            }
        } else if started.elapsed() > cfg.tv_start_timeout * attempts as u32 {
            if attempts >= 2 || i.hub.stats.rejected.load(Ordering::Relaxed) > 0 {
                let blocked = i.hub.stats.rejected.load(Ordering::Relaxed) > 0;
                return Outcome::Failed(if blocked {
                    format!("{tvname} tried to fetch the picture from an unexpected address and was blocked.")
                } else {
                    format!("{tvname} accepted the command but never started playing. It may not support live network streams, or a firewall on this device is blocking it.")
                });
            }
            attempts += 1;
            let _ = av.stop();
            if let Err(e) = attempt(&av) {
                return Outcome::Failed(e);
            }
        }
        if net_errors >= 10 {
            return Outcome::Failed(format!("Lost contact with {tvname}."));
        }
    }
}

fn run_cast(i: &Arc<Inner>, tv: &Tv, base: &str, cfg: &CastConfig, port: u16) -> Outcome {
    let url = format!("{base}/live.m3u8");
    let addr = SocketAddr::new(tv.ip, port);
    let tvname = &tv.name;
    let connect = || -> std::io::Result<CastClient<Box<dyn ReadWrite>>> {
        let stream: Box<dyn ReadWrite> = match &cfg.connector {
            Some(c) => c(addr)?,
            #[cfg(feature = "tls")]
            None => Box::new(castv2::tls::connect(addr, Duration::from_secs(5))?),
            #[cfg(not(feature = "tls"))]
            None => return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "built without TLS support")),
        };
        let mut c = CastClient::new(stream);
        c.open()?;
        c.launch(castv2::DEFAULT_MEDIA_RECEIVER)?;
        Ok(c)
    };

    let mut client = match connect() {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(format!("Cannot reach {tvname} ({e}). Is it on the same Wi-Fi and is Chromecast built-in enabled?")),
    };
    let mut loaded = false;
    let mut load_at: Option<Instant> = None;
    let started = Instant::now();
    let mut reconnects = 0;
    loop {
        if i.stopping() {
            let _ = client.stop_app();
            return Outcome::Stopped;
        }
        let events = match client.pump() {
            Ok(e) => e,
            Err(e) => {
                // control channel lost
                if reconnects >= 4 || !i.ever_played.load(Ordering::SeqCst) && reconnects >= 1 {
                    return Outcome::Failed(format!("Lost contact with {tvname} ({e})."));
                }
                reconnects += 1;
                i.set(CastState::Reconnecting);
                i.sleep(Duration::from_secs(2));
                match connect() {
                    Ok(c) => {
                        client = c;
                        loaded = false;
                        continue;
                    }
                    Err(_) => continue,
                }
            }
        };
        for ev in events {
            match ev {
                CastEvent2::AppReady { .. } => {}
                CastEvent2::Media { state, idle_reason } => match state.as_str() {
                    "PLAYING" => i.set(CastState::Playing),
                    "BUFFERING" | "LOADING" => i.set(if i.ever_played.load(Ordering::SeqCst) { CastState::Buffering } else { CastState::Starting }),
                    "IDLE" => match idle_reason.as_deref() {
                        Some("ERROR") => return Outcome::Failed(format!("{tvname} could not play the stream.")),
                        Some(_) if loaded => return Outcome::Failed(format!("Casting to {tvname} was ended on the TV.")),
                        _ => {}
                    },
                    _ => {}
                },
                CastEvent2::LoadFailed(_) => return Outcome::Failed(format!("{tvname} refused the stream.")),
                CastEvent2::Error(m) => return Outcome::Failed(m),
                CastEvent2::AppStopped if loaded => return Outcome::Failed(format!("Casting to {tvname} was ended on the TV.")),
                CastEvent2::AppStopped | CastEvent2::Closed => {
                    if loaded {
                        return Outcome::Failed(format!("{tvname} closed the connection."));
                    }
                }
            }
        }
        // once the app is up and the playlist has enough segments, tell it what to play
        if !loaded && client.transport_id.is_some() && i.hub.hls_ready() {
            match client.load_live(&url, "application/x-mpegURL") {
                Ok(()) => {
                    loaded = true;
                    load_at = Some(Instant::now());
                }
                Err(e) => return Outcome::Failed(format!("Lost contact with {tvname} ({e}).")),
            }
        }
        if client.silent_for() > Duration::from_secs(20) {
            return Outcome::Failed(format!("{tvname} stopped answering."));
        }
        if !i.ever_played.load(Ordering::SeqCst) {
            if let Some(t) = load_at {
                if t.elapsed() > cfg.tv_start_timeout {
                    return Outcome::Failed(format!("{tvname} did not start playing the stream. It may not support live streams, or a firewall on this device is blocking it."));
                }
            } else if started.elapsed() > cfg.tv_start_timeout + Duration::from_secs(10) {
                return Outcome::Failed(format!("{tvname} did not start its media player."));
            }
        }
        if i.ever_played.load(Ordering::SeqCst) && i.hub.stats.playlist_requests.load(Ordering::Relaxed) > 0 {
            reconnects = reconnects.min(1);
        }
    }
}
