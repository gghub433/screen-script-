use crate::clock::Clock;
use crate::events::{ReconfigReason, SenderEvent, SenderState};
use crate::sync::ClockSync;
use revizor_adaptive::{Battery, Ceiling, Config, Engine, Profile, Reason, Signals, ThermalLevel, Tier};
use revizor_crypto::handshake::HandshakeError;
use revizor_crypto::{Identity, Initiator, RxCipher, TrustStore, TxCipher};
use revizor_media::{EncodedFrame, Pacer, PacketizedFrame, RateMeter, SendHistory};
use revizor_proto::geometry::fit_short_side;
use revizor_proto::{
    negotiate, ByeReason, Capabilities, Channel, Control, KeyframeReason, Negotiated, PacketHeader, PacketKind, Preference,
    ReceiverReport, StreamParams, VideoParams, FLAG_CONFIG, FLAG_KEYFRAME, MAX_DATAGRAM,
};
use revizor_transport::Transport;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct SourceInfo {
    pub width: u16,
    pub height: u16,
    /// Refresh rate of the captured display (caps the stream fps).
    pub refresh_hz: u16,
}

pub struct SenderConfig {
    pub identity: Arc<Identity>,
    pub trust: Arc<dyn TrustStore>,
    pub caps: Capabilities,
    pub pref: Preference,
    pub profile: Profile,
    /// For `Profile::Custom`: pinned tier and/or bitrate.
    pub custom_tier: Option<Tier>,
    pub custom_bitrate: Option<u32>,
    pub source: SourceInfo,
    pub peer: SocketAddr,
    /// Encoder dimension alignment (2 is fine for most; some SoCs need 16).
    pub size_align: u16,
    /// Stop trying to reconnect after this long and report `Failed`.
    pub give_up_after: Duration,
}

/// Live measurements from the platform that only the app can take.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceSignals {
    pub thermal: Option<ThermalLevel>,
    pub battery: Option<Battery>,
}

#[derive(Debug, Clone, Default)]
pub struct SenderStats {
    pub state: Option<SenderState>,
    pub peer_name: String,
    pub codec: Option<revizor_proto::Codec>,
    pub width: u16,
    pub height: u16,
    pub fps_target: u16,
    pub bitrate_target_bps: u32,
    /// Bits/s actually put on the wire during the last interval.
    pub sent_bps: u32,
    pub fec_k: u8,
    pub rtt_us: Option<u32>,
    pub clock_offset_us: Option<i64>,
    pub frames_sent: u64,
    /// Frames the sender had to drop because the queue to the network stalled.
    pub frames_dropped_sender: u64,
    pub retransmitted_packets: u64,
    pub keyframes_sent: u64,
    pub send_errors: u64,
    pub encode_us_avg: Option<u32>,
    pub last_report: Option<ReceiverReport>,
    pub limited_by: Option<Reason>,
    pub hardware_codecs: bool,
}

enum QItem {
    Video { epoch: u16, pts_us: u64, keyframe: bool, data: Vec<u8> },
    Audio { pts_us: u64, data: Vec<u8> },
}

struct Live {
    tx: Arc<TxCipher>,
    negotiated: Negotiated,
    params: StreamParams,
    engine: Engine,
    history: SendHistory,
    next_video_id: u32,
    next_audio_id: u32,
}

struct State {
    phase: SenderState,
    live: Option<Live>,
    sync: ClockSync,
    last_rx_us: u64,
    source: SourceInfo,
    dev: DeviceSignals,
    stats: SenderStats,
    rate: RateMeter,
    enc_sum_us: u64,
    enc_n: u64,
    interval_drops: u32,
    next_epoch: u16,
    last_kf_event_us: u64,
    retx_budget_bytes: f64,
    retx_budget_at_us: u64,
    params_resend: VecDeque<u64>,
}

type EventFn = Arc<dyn Fn(SenderEvent) + Send + Sync>;

struct Inner {
    cfg: SenderConfig,
    clock: Arc<dyn Clock>,
    transport: Arc<dyn Transport>,
    events: EventFn,
    stop: AtomicBool,
    pace_rate: AtomicU64,
    st: Mutex<State>,
    queue: Mutex<VecDeque<QItem>>,
    qcv: Condvar,
}

pub struct SenderSession {
    inner: Arc<Inner>,
    threads: Vec<JoinHandle<()>>,
}

const QUEUE_LIMIT: usize = 8;
const LIVENESS_US: u64 = 2_500_000;

impl SenderSession {
    pub fn start(cfg: SenderConfig, transport: Arc<dyn Transport>, clock: Arc<dyn Clock>, events: impl Fn(SenderEvent) + Send + Sync + 'static) -> Self {
        let source = cfg.source;
        let inner = Arc::new(Inner {
            cfg,
            clock,
            transport,
            events: Arc::new(events),
            stop: AtomicBool::new(false),
            pace_rate: AtomicU64::new(10_000_000),
            st: Mutex::new(State {
                phase: SenderState::Connecting,
                live: None,
                sync: ClockSync::default(),
                last_rx_us: 0,
                source,
                dev: DeviceSignals::default(),
                stats: SenderStats::default(),
                rate: RateMeter::default(),
                enc_sum_us: 0,
                enc_n: 0,
                interval_drops: 0,
                next_epoch: 1,
                last_kf_event_us: 0,
                retx_budget_bytes: 0.0,
                retx_budget_at_us: 0,
                params_resend: VecDeque::new(),
            }),
            queue: Mutex::new(VecDeque::new()),
            qcv: Condvar::new(),
        });
        let rx = {
            let i = inner.clone();
            std::thread::Builder::new().name("rvz-send-ctl".into()).spawn(move || i.control_thread()).expect("spawn")
        };
        let tx = {
            let i = inner.clone();
            std::thread::Builder::new().name("rvz-send-media".into()).spawn(move || i.media_thread()).expect("spawn")
        };
        Self { inner, threads: vec![rx, tx] }
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.inner.clock
    }

    /// Hand an encoded video access unit to the network. `pts_us` is the capture
    /// time in the session clock domain. Never blocks; returns false if dropped.
    pub fn submit_video(&self, epoch: u16, pts_us: u64, keyframe: bool, data: Vec<u8>) -> bool {
        self.inner.submit(QItem::Video { epoch, pts_us, keyframe, data })
    }

    pub fn submit_audio(&self, pts_us: u64, data: Vec<u8>) -> bool {
        self.inner.submit(QItem::Audio { pts_us, data })
    }

    /// Measured time one frame spent in the encoder (input→output). Averaged per interval.
    pub fn report_encode_time_us(&self, us: u32) {
        let mut st = self.inner.st.lock().unwrap();
        st.enc_sum_us += us as u64;
        st.enc_n += 1;
    }

    /// The capture/encoder layer dropped a frame (e.g. encoder input busy).
    pub fn report_capture_drop(&self) {
        let mut st = self.inner.st.lock().unwrap();
        st.interval_drops += 1;
        st.stats.frames_dropped_sender += 1;
    }

    pub fn update_device_signals(&self, f: impl FnOnce(&mut DeviceSignals)) {
        f(&mut self.inner.st.lock().unwrap().dev);
    }

    /// Rotation / window resize / monitor switch.
    pub fn set_source(&self, source: SourceInfo) {
        let mut st = self.inner.st.lock().unwrap();
        let changed = st.source.width != source.width || st.source.height != source.height || st.source.refresh_hz != source.refresh_hz;
        st.source = source;
        if changed && st.live.is_some() {
            drop(st);
            self.inner.reconfigure(None, ReconfigReason::SourceChanged);
        }
    }

    pub fn stats(&self) -> SenderStats {
        let st = self.inner.st.lock().unwrap();
        let mut s = st.stats.clone();
        s.state = Some(st.phase.clone());
        s.rtt_us = st.sync.srtt_us();
        s.clock_offset_us = st.sync.offset_us();
        s
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.qcv.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for SenderSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Inner {
    fn emit(&self, e: SenderEvent) {
        (self.events)(e);
    }

    fn set_phase(&self, p: SenderState) {
        {
            let mut st = self.st.lock().unwrap();
            if st.phase == p {
                return;
            }
            st.phase = p.clone();
        }
        self.emit(SenderEvent::State(p));
    }

    fn submit(&self, item: QItem) -> bool {
        if self.st.lock().unwrap().live.is_none() {
            return false;
        }
        let mut q = self.queue.lock().unwrap();
        if q.len() >= QUEUE_LIMIT {
            drop(q);
            // The network side stalled. Dropping a delta frame breaks the reference
            // chain, so ask the encoder for a fresh keyframe once things move again.
            let mut st = self.st.lock().unwrap();
            st.stats.frames_dropped_sender += 1;
            st.interval_drops += 1;
            let now = self.clock.now_us();
            let ask = now.saturating_sub(st.last_kf_event_us) > 200_000;
            if ask {
                st.last_kf_event_us = now;
            }
            drop(st);
            if ask {
                self.emit(SenderEvent::RequestKeyframe(KeyframeReason::PacketLoss));
            }
            return false;
        }
        q.push_back(item);
        drop(q);
        self.qcv.notify_one();
        true
    }

    fn send_raw(&self, d: &[u8]) {
        if self.transport.send_to(d, self.cfg.peer).is_err() {
            self.st.lock().unwrap().stats.send_errors += 1;
        }
    }

    fn send_control(&self, tx: &TxCipher, c: &Control) {
        let body = c.encode();
        self.send_raw(&tx.seal(Channel::Control, &[&body]));
    }

    fn tx_cipher(&self) -> Option<Arc<TxCipher>> {
        self.st.lock().unwrap().live.as_ref().map(|l| l.tx.clone())
    }

    // ───────────────────────────── control thread ─────────────────────────────

    fn control_thread(self: Arc<Self>) {
        let started = self.clock.now_us();
        let mut attempt = 0u32;
        while !self.stop.load(Ordering::Relaxed) {
            if attempt == 0 {
                self.set_phase(SenderState::Connecting);
            } else {
                self.set_phase(SenderState::Reconnecting { attempt });
            }
            match self.handshake() {
                Ok(est) => {
                    let Some(rx) = self.establish(est) else { break };
                    let why = self.stream_loop(rx);
                    self.teardown();
                    if self.stop.load(Ordering::Relaxed) || why == LoopExit::Stopped {
                        break;
                    }
                    if why == LoopExit::Bye {
                        self.set_phase(SenderState::Failed("The receiver ended the session".into()));
                        break;
                    }
                    attempt = 1;
                }
                Err(HsFail::Fatal(msg)) => {
                    self.set_phase(SenderState::Failed(msg));
                    break;
                }
                Err(HsFail::Retry) => {
                    attempt += 1;
                    if self.clock.now_us().saturating_sub(started) > self.cfg.give_up_after.as_micros() as u64 && attempt > 1 {
                        self.set_phase(SenderState::Failed("Receiver is unreachable".into()));
                        break;
                    }
                    self.sleep_interruptible(Duration::from_millis(300 * attempt.min(6) as u64));
                }
            }
            if matches!(self.st.lock().unwrap().phase, SenderState::Reconnecting { .. }) {
                let waited = self.clock.now_us().saturating_sub(self.st.lock().unwrap().last_rx_us);
                if waited > self.cfg.give_up_after.as_micros() as u64 {
                    self.set_phase(SenderState::Failed("Connection to the receiver could not be restored".into()));
                    break;
                }
            }
        }
        if !matches!(self.st.lock().unwrap().phase, SenderState::Failed(_)) {
            self.set_phase(SenderState::Stopped);
        }
    }

    fn sleep_interruptible(&self, d: Duration) {
        let end = std::time::Instant::now() + d;
        while !self.stop.load(Ordering::Relaxed) && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn handshake(&self) -> Result<revizor_crypto::Established, HsFail> {
        let (mut init, hello) = Initiator::start(self.cfg.identity.clone(), &self.cfg.caps, self.cfg.trust.clone());
        let wrap = |body: &[u8]| {
            let mut d = PacketHeader::new(PacketKind::Handshake, 0, 0).encode().to_vec();
            d.extend_from_slice(body);
            d
        };
        let mut buf = vec![0u8; 2048];
        let mut stage = 0; // 0 = awaiting ServerHello, 1 = awaiting Accept
        let mut last_tx = std::time::Instant::now() - Duration::from_secs(1);
        let mut finish: Option<Vec<u8>> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline && !self.stop.load(Ordering::Relaxed) {
            if last_tx.elapsed() >= Duration::from_millis(250) {
                let m = if stage == 0 { &hello } else { finish.as_ref().unwrap() };
                self.send_raw(&wrap(m));
                last_tx = std::time::Instant::now();
            }
            let Ok(Some((n, from))) = self.transport.recv_from(&mut buf, Duration::from_millis(20)) else { continue };
            if from.ip() != self.cfg.peer.ip() && from.port() != 0 && from != self.cfg.peer {
                continue;
            }
            let Ok((h, body)) = PacketHeader::decode(&buf[..n]) else { continue };
            if h.kind != PacketKind::Handshake {
                continue;
            }
            if stage == 0 {
                match init.on_server_hello(body) {
                    Ok(f) => {
                        finish = Some(f);
                        stage = 1;
                        last_tx = std::time::Instant::now() - Duration::from_secs(1);
                    }
                    Err(e) => return Err(classify(e)),
                }
            } else {
                match init.on_accept(body) {
                    Ok(est) => return Ok(est),
                    // A stale ServerHello retransmission can arrive here; ignore and keep waiting.
                    Err(HandshakeError::Unexpected) | Err(HandshakeError::Malformed(_)) => {}
                    Err(e) => return Err(classify(e)),
                }
            }
        }
        Err(HsFail::Retry)
    }

    fn establish(&self, est: revizor_crypto::Established) -> Option<RxCipher> {
        let revizor_crypto::Established { session_id: _, version: _, peer_identity, peer_caps, tx, rx } = est;
        let now = self.clock.now_us();
        let (source, epoch) = {
            let mut st = self.st.lock().unwrap();
            let e = st.next_epoch;
            st.next_epoch = st.next_epoch.wrapping_add(1).max(1);
            (st.source, e)
        };
        let neg = match negotiate(&self.cfg.caps, &peer_caps, &self.cfg.pref, source.width, source.height, epoch) {
            Ok(n) => n,
            Err(e) => {
                let t = Arc::new(tx);
                self.send_control(&t, &Control::Bye(ByeReason::IncompatibleVersion));
                self.set_phase(SenderState::Failed(format!("The receiver cannot decode this stream ({e})")));
                return None;
            }
        };
        let ceiling = Ceiling {
            max_short_side: {
                let (w, h) = (neg.params.video.width, neg.params.video.height);
                w.min(h)
            },
            max_fps: neg.params.video.fps.min(source.refresh_hz.max(30)),
            max_bitrate_bps: self.cfg.caps.max_bitrate_bps.min(peer_caps.max_bitrate_bps),
        };
        let ecfg = if self.cfg.profile == Profile::Custom {
            Config::custom(ceiling, self.cfg.custom_tier, self.cfg.custom_bitrate)
        } else {
            Config::for_profile(self.cfg.profile, ceiling)
        };
        let engine = Engine::new(ecfg);
        let tier = engine.tier();
        let params = self.params_for(&neg, epoch, tier, engine.video_bitrate_bps(), source);
        let tx = Arc::new(tx);
        let live = Live {
            tx: tx.clone(),
            negotiated: neg,
            params,
            engine,
            history: SendHistory::new(500_000, 8 << 20),
            next_video_id: 0,
            next_audio_id: 0,
        };
        {
            let mut st = self.st.lock().unwrap();
            st.sync = ClockSync::default();
            st.last_rx_us = now;
            st.stats.peer_name = peer_caps.device_name.clone();
            st.stats.hardware_codecs = neg.hardware_both;
            st.stats.codec = Some(params.video.codec);
            st.stats.width = params.video.width;
            st.stats.height = params.video.height;
            st.stats.fps_target = params.video.fps;
            st.stats.bitrate_target_bps = params.video.bitrate_bps;
            st.live = Some(live);
            st.rate = RateMeter::default();
        }
        self.pace_rate.store(params.video.bitrate_bps as u64 * 3, Ordering::Relaxed);
        self.set_phase(SenderState::Streaming);
        self.emit(SenderEvent::PeerInfo {
            name: peer_caps.device_name,
            device_id: revizor_crypto::device_id(&peer_identity),
            hardware_codecs: neg.hardware_both,
        });
        self.emit(SenderEvent::Reconfigure { params, reason: ReconfigReason::Initial, limited_by: None });
        self.emit(SenderEvent::RequestKeyframe(KeyframeReason::StreamStart));
        // Params are repeated shortly after (UDP); the receiver can also ask (ParamsRequest).
        self.send_control(&tx, &Control::Params(params));
        self.st.lock().unwrap().params_resend.extend([now + 40_000, now + 120_000]);
        Some(rx)
    }

    fn params_for(&self, neg: &Negotiated, epoch: u16, tier: Tier, video_bps: u32, source: SourceInfo) -> StreamParams {
        let (w, h) = fit_short_side(source.width, source.height, tier.short_side, self.cfg.size_align);
        let fps = tier.fps.min(source.refresh_hz.max(1)).max(1);
        StreamParams {
            epoch,
            video: VideoParams {
                codec: neg.params.video.codec,
                width: w,
                height: h,
                fps,
                bitrate_bps: video_bps,
                keyframe_interval_ms: neg.params.video.keyframe_interval_ms,
            },
            audio: neg.params.audio,
        }
    }

    fn stream_loop(&self, mut rx: RxCipher) -> LoopExit {
        let mut buf = vec![0u8; 2048];
        let mut last_ping = 0u64;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                if let Some(tx) = self.tx_cipher() {
                    for _ in 0..2 {
                        self.send_control(&tx, &Control::Bye(ByeReason::UserStopped));
                    }
                }
                return LoopExit::Stopped;
            }
            match self.transport.recv_from(&mut buf, Duration::from_millis(5)) {
                Ok(Some((n, _))) => {
                    let t_recv = self.clock.now_us();
                    if let Ok(op) = rx.open(&buf[..n]) {
                        self.st.lock().unwrap().last_rx_us = t_recv;
                        if op.channel == Channel::Control {
                            if let Ok(c) = Control::decode(&op.payload) {
                                if let Some(exit) = self.on_control(c, t_recv) {
                                    return exit;
                                }
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    log::debug!("recv error: {e}");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let now = self.clock.now_us();
            if now.saturating_sub(last_ping) >= 250_000 {
                last_ping = now;
                if let Some(tx) = self.tx_cipher() {
                    let id = (now / 1000) as u32;
                    self.send_control(&tx, &Control::Ping { id, t0_us: now });
                }
            }
            self.resend_params_if_due(now);
            if now.saturating_sub(self.st.lock().unwrap().last_rx_us) > LIVENESS_US {
                log::warn!("receiver silent for {} ms, reconnecting", LIVENESS_US / 1000);
                return LoopExit::Lost;
            }
        }
    }

    fn resend_params_if_due(&self, now: u64) {
        let (due, tx, params) = {
            let mut st = self.st.lock().unwrap();
            let due = st.params_resend.front().is_some_and(|t| *t <= now);
            if due {
                st.params_resend.pop_front();
            }
            match &st.live {
                Some(l) if due => (true, Some(l.tx.clone()), Some(l.params)),
                _ => (false, None, None),
            }
        };
        if due {
            if let (Some(tx), Some(p)) = (tx, params) {
                self.send_control(&tx, &Control::Params(p));
            }
        }
    }

    fn on_control(&self, c: Control, t_recv: u64) -> Option<LoopExit> {
        match c {
            Control::Ping { id, t0_us } => {
                if let Some(tx) = self.tx_cipher() {
                    self.send_control(&tx, &Control::Pong { id, t0_us, t1_us: t_recv, t2_us: self.clock.now_us() });
                }
            }
            Control::Pong { t0_us, t1_us, t2_us, .. } => {
                self.st.lock().unwrap().sync.on_pong(t0_us, t1_us, t2_us, t_recv);
            }
            Control::Report(r) => self.on_report(r),
            Control::Nack { epoch, frame_id, missing } => self.retransmit(epoch, frame_id, &missing),
            Control::KeyframeRequest { reason, .. } => {
                let now = self.clock.now_us();
                let mut st = self.st.lock().unwrap();
                if now.saturating_sub(st.last_kf_event_us) >= 100_000 {
                    st.last_kf_event_us = now;
                    drop(st);
                    self.emit(SenderEvent::RequestKeyframe(reason));
                }
            }
            Control::ParamsRequest => {
                let (tx, p) = {
                    let st = self.st.lock().unwrap();
                    match &st.live {
                        Some(l) => (Some(l.tx.clone()), Some(l.params)),
                        None => (None, None),
                    }
                };
                if let (Some(tx), Some(p)) = (tx, p) {
                    self.send_control(&tx, &Control::Params(p));
                }
            }
            Control::Bye(_) => return Some(LoopExit::Bye),
            Control::Params(_) => {}
        }
        None
    }

    fn retransmit(&self, epoch: u16, frame_id: u32, missing: &[u16]) {
        let now = self.clock.now_us();
        let (tx, pf) = {
            let mut st = self.st.lock().unwrap();
            // Retransmissions may use at most ~25% of the stream bitrate, otherwise a
            // lossy link would be flooded by its own repair traffic.
            let rate = st.live.as_ref().map_or(0, |l| l.params.video.bitrate_bps) as f64 / 8.0 * 0.25;
            let dt = now.saturating_sub(st.retx_budget_at_us) as f64 / 1e6;
            st.retx_budget_at_us = now;
            st.retx_budget_bytes = (st.retx_budget_bytes + rate * dt).min(rate * 0.25 + 4.0 * MAX_DATAGRAM as f64);
            match &st.live {
                Some(l) => (l.tx.clone(), l.history.get(epoch, frame_id)),
                None => return,
            }
        };
        let Some(pf) = pf else { return };
        for &idx in missing {
            {
                let mut st = self.st.lock().unwrap();
                if st.retx_budget_bytes < MAX_DATAGRAM as f64 {
                    return;
                }
                st.retx_budget_bytes -= MAX_DATAGRAM as f64;
                st.stats.retransmitted_packets += 1;
            }
            if let Some((hdr, body)) = pf.data_packet(idx, true) {
                self.send_raw(&tx.seal(Channel::Video, &[&hdr, body]));
            }
        }
    }

    fn on_report(&self, r: ReceiverReport) {
        let now = self.clock.now_us();
        let (act_bitrate, act_tier) = {
            let mut st = self.st.lock().unwrap();
            // minimum over the last ~1.5 s: robust against one-off scheduling spikes
            let rtt = st.sync.recent_min_rtt_us(now, 1_500_000).or_else(|| st.sync.srtt_us());
            let enc = if st.enc_n > 0 { Some((st.enc_sum_us / st.enc_n) as u32) } else { None };
            let total_frames = r.frames_complete + r.frames_dropped + st.interval_drops;
            let dropped_ratio = if total_frames > 0 { (r.frames_dropped + st.interval_drops) as f32 / total_frames as f32 } else { 0.0 };
            let sent_bps = st.rate.take_bps(now);
            let dev = st.dev;
            let loss_pct = if r.packets_expected > 0 { 100.0 * r.packets_lost as f32 / r.packets_expected as f32 } else { 0.0 };
            let sig = Signals {
                loss_pct,
                rtt_us: rtt,
                jitter_us: r.jitter_us,
                recv_bps: (r.recv_bitrate_bps > 0).then_some(r.recv_bitrate_bps),
                sent_bps,
                dropped_ratio,
                encode_us: enc,
                decode_us: (r.decode_us > 0).then_some(r.decode_us),
                thermal: dev.thermal.unwrap_or(ThermalLevel::Unknown),
                receiver_thermal: ThermalLevel::from_wire(r.thermal),
                battery: dev.battery,
            };
            st.enc_sum_us = 0;
            st.enc_n = 0;
            st.interval_drops = 0;
            st.stats.sent_bps = sent_bps;
            st.stats.encode_us_avg = enc;
            st.stats.last_report = Some(r);
            let Some(live) = st.live.as_mut() else { return };
            let up = live.engine.on_signals(now / 1000, &sig);
            let status = live.engine.status();
            let video_bps = live.engine.video_bitrate_bps();
            live.params.video.bitrate_bps = video_bps;
            self.pace_rate.store(status.bitrate_bps as u64 * 3, Ordering::Relaxed);
            st.stats.fec_k = status.fec_k;
            st.stats.bitrate_target_bps = video_bps;
            st.stats.limited_by = status.limited_by;
            ((up.bitrate_bps.is_some() || up.fec_k.is_some()).then_some(video_bps), up.tier)
        };
        if let Some((tier, reason)) = act_tier {
            self.reconfigure(Some(tier), ReconfigReason::Adaptive(reason));
        } else if let Some(v) = act_bitrate {
            self.emit(SenderEvent::SetBitrate { video_bps: v });
        }
    }

    /// New epoch with (possibly) new tier / source size. Encoder must restart and emit a keyframe.
    fn reconfigure(&self, tier: Option<Tier>, reason: ReconfigReason) {
        let now = self.clock.now_us();
        let (params, tx, limited) = {
            let mut st = self.st.lock().unwrap();
            let source = st.source;
            let epoch = st.next_epoch;
            st.next_epoch = st.next_epoch.wrapping_add(1).max(1);
            let Some(live) = st.live.as_mut() else { return };
            let tier = tier.unwrap_or_else(|| live.engine.tier());
            let video_bps = live.engine.video_bitrate_bps();
            let neg = live.negotiated;
            let params = {
                let (w, h) = fit_short_side(source.width, source.height, tier.short_side, self.cfg.size_align);
                StreamParams {
                    epoch,
                    video: VideoParams {
                        codec: neg.params.video.codec,
                        width: w,
                        height: h,
                        fps: tier.fps.min(source.refresh_hz.max(1)).max(1),
                        bitrate_bps: video_bps,
                        keyframe_interval_ms: neg.params.video.keyframe_interval_ms,
                    },
                    audio: neg.params.audio,
                }
            };
            live.params = params;
            let limited = live.engine.status().limited_by;
            st.stats.width = params.video.width;
            st.stats.height = params.video.height;
            st.stats.fps_target = params.video.fps;
            st.stats.bitrate_target_bps = params.video.bitrate_bps;
            st.last_kf_event_us = now;
            st.params_resend.extend([now + 40_000, now + 120_000]);
            (params, st.live.as_ref().unwrap().tx.clone(), limited)
        };
        // frames still queued from the old configuration are useless
        self.queue.lock().unwrap().clear();
        self.emit(SenderEvent::Reconfigure { params, reason, limited_by: limited });
        self.emit(SenderEvent::RequestKeyframe(KeyframeReason::ConfigChange));
        self.send_control(&tx, &Control::Params(params));
    }

    fn teardown(&self) {
        self.st.lock().unwrap().live = None;
        self.queue.lock().unwrap().clear();
    }

    // ───────────────────────────── media thread ─────────────────────────────

    fn media_thread(self: Arc<Self>) {
        let mut pacer = Pacer::new(30_000_000, 24 * MAX_DATAGRAM as u64);
        loop {
            let item = {
                let mut q = self.queue.lock().unwrap();
                loop {
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(i) = q.pop_front() {
                        break i;
                    }
                    let (g, _) = self.qcv.wait_timeout(q, Duration::from_millis(50)).unwrap();
                    q = g;
                }
            };
            pacer.set_rate(self.pace_rate.load(Ordering::Relaxed));
            match item {
                QItem::Video { epoch, pts_us, keyframe, data } => self.send_video(&mut pacer, epoch, pts_us, keyframe, data),
                QItem::Audio { pts_us, data } => self.send_audio(pts_us, data),
            }
        }
    }

    fn send_video(&self, pacer: &mut Pacer, epoch: u16, pts_us: u64, keyframe: bool, data: Vec<u8>) {
        let (tx, frame_id, fec_k) = {
            let mut st = self.st.lock().unwrap();
            let Some(live) = st.live.as_mut() else { return };
            if live.params.epoch != epoch {
                return; // produced by a superseded encoder configuration
            }
            let id = live.next_video_id;
            live.next_video_id = id.wrapping_add(1);
            (live.tx.clone(), id, live.engine.fec_k())
        };
        let flags = if keyframe { FLAG_KEYFRAME | FLAG_CONFIG } else { 0 };
        let pf = Arc::new(PacketizedFrame::new(EncodedFrame { epoch, frame_id, pts_us, flags, data }, fec_k));
        {
            let mut st = self.st.lock().unwrap();
            let now = self.clock.now_us();
            if let Some(live) = st.live.as_mut() {
                live.history.push(now, pf.clone());
            }
        }
        let mut bytes = 0usize;
        let send = |ch: Channel, hdr: &[u8], body: &[u8], pacer: &mut Pacer, bytes: &mut usize| {
            let d = tx.seal(ch, &[hdr, body]);
            let wait = pacer.delay_us(self.clock.now_us(), d.len());
            if wait >= 500 {
                std::thread::sleep(Duration::from_micros(wait));
            }
            self.send_raw(&d);
            *bytes += d.len();
        };
        for i in 0..pf.pkt_count {
            if let Some((hdr, body)) = pf.data_packet(i, false) {
                send(Channel::Video, &hdr, body, pacer, &mut bytes);
            }
        }
        for (hdr, body) in pf.fec_packets() {
            send(Channel::VideoFec, &hdr, body, pacer, &mut bytes);
        }
        let mut st = self.st.lock().unwrap();
        st.rate.add(self.clock.now_us(), bytes);
        st.stats.frames_sent += 1;
        if keyframe {
            st.stats.keyframes_sent += 1;
        }
    }

    fn send_audio(&self, pts_us: u64, data: Vec<u8>) {
        let (tx, id, epoch) = {
            let mut st = self.st.lock().unwrap();
            let Some(live) = st.live.as_mut() else { return };
            let id = live.next_audio_id;
            live.next_audio_id = id.wrapping_add(1);
            (live.tx.clone(), id, live.params.epoch)
        };
        let pf = PacketizedFrame::new(EncodedFrame { epoch, frame_id: id, pts_us, flags: 0, data }, 0);
        if pf.pkt_count != 1 {
            log::warn!("audio frame of {} bytes does not fit one packet, dropped", pf.frame.data.len());
            return;
        }
        if let Some((hdr, body)) = pf.data_packet(0, false) {
            let d = tx.seal(Channel::Audio, &[&hdr, body]);
            self.send_raw(&d);
            self.st.lock().unwrap().rate.add(self.clock.now_us(), d.len());
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum LoopExit {
    Stopped,
    Lost,
    Bye,
}

enum HsFail {
    Retry,
    Fatal(String),
}

fn classify(e: HandshakeError) -> HsFail {
    match e {
        HandshakeError::UnknownPeer => HsFail::Fatal("This receiver is not paired with this device. Pair it first.".into()),
        HandshakeError::Version => HsFail::Fatal("The receiver runs an incompatible Revizor version. Update both apps.".into()),
        HandshakeError::BadProof => HsFail::Fatal("The receiver failed identity verification. It may not be the paired device.".into()),
        HandshakeError::Rejected(_) | HandshakeError::Malformed(_) | HandshakeError::Unexpected => HsFail::Retry,
    }
}

