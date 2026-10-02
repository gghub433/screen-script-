use crate::clock::Clock;
use crate::events::ReceiverEvent;
use crate::sync::ClockSync;
use revizor_adaptive::ThermalLevel;
use revizor_crypto::pairing::{generate_pin, PairResponder, PairingGuard};
use revizor_crypto::{device_id, Identity, Responder, RxCipher, TrustStore, TxCipher};
use revizor_media::{Assembler, AssemblerConfig, JitterEstimator, LossTracker, RateMeter, ReceivedFrame};
use revizor_proto::discovery::Announcement;
use revizor_proto::wire::Reader;
use revizor_proto::{
    Capabilities, Channel, Codec, Control, KeyframeReason, MediaHeader, PacketHeader, PacketKind, ReceiverReport,
    StreamParams, TransportKind, MEDIA_HEADER_LEN, PROTOCOL_VERSION,
};
use revizor_transport::Transport;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverState {
    Listening,
    Streaming { sender: String },
    Stopped,
}

pub struct ReceiverConfig {
    pub identity: Arc<Identity>,
    pub trust: Arc<dyn TrustStore>,
    pub caps: Capabilities,
    pub assembler: AssemblerConfig,
    /// Frames the app may fall behind before the queue is flushed and a keyframe requested.
    pub max_queued_frames: usize,
}

impl ReceiverConfig {
    pub fn new(identity: Arc<Identity>, trust: Arc<dyn TrustStore>, caps: Capabilities) -> Self {
        Self { identity, trust, caps, assembler: AssemblerConfig::default(), max_queued_frames: 6 }
    }
}

#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub epoch: u16,
    pub pts_us: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct ReceiverStats {
    pub state: Option<ReceiverState>,
    pub sender_name: String,
    pub params: Option<StreamParams>,
    /// Frames per second actually delivered to the decoder over the last interval.
    pub fps: f32,
    pub recv_bitrate_bps: u32,
    pub loss_pct: f32,
    pub jitter_us: u32,
    pub rtt_us: Option<u32>,
    /// Capture → complete-frame-received, needs clock sync.
    pub network_latency_us: Option<u32>,
    /// Capture → on screen, measured when the app calls `on_frame_presented`.
    pub e2e_latency_us: Option<u32>,
    pub decode_us: Option<u32>,
    pub frames_delivered: u64,
    pub frames_abandoned: u64,
    pub frames_discarded: u64,
    pub frames_dropped_app: u64,
    pub packets_recovered_fec: u64,
    pub packets_recovered_retx: u64,
    pub nacks_sent: u64,
    pub keyframe_requests: u64,
    pub buffered_frames: usize,
}

struct Pending {
    from: SocketAddr,
    hello: Vec<u8>,
    reply: Vec<u8>,
    responder: Responder,
    at_us: u64,
}

struct PairWindow {
    pin: String,
    guard: PairingGuard,
    attempt: Option<(SocketAddr, PairResponder, u64)>,
}

struct Active {
    peer: SocketAddr,
    rx: RxCipher,
    tx: Arc<TxCipher>,
    assembler: Assembler,
    loss: LossTracker,
    jitter: JitterEstimator,
    rate: RateMeter,
    sync: ClockSync,
    params: Option<StreamParams>,
    dropped_unknown_epoch: bool,
    last_rx_us: u64,
    last_report_us: u64,
    last_ping_us: u64,
    last_params_req_us: u64,
    last_audio_id: Option<u32>,
    drop_until_key: bool,
    // interval accumulators
    frames_ok: u32,
    frames_bad: u32,
    decode_sum: u64,
    decode_n: u64,
    e2e_sum: u64,
    e2e_n: u64,
    highest_frame: u32,
    recovered_prev: u64,
    expected_prev_loss: (u32, u32),
}

struct RState {
    pending: Option<Pending>,
    last_accept: Option<(SocketAddr, Vec<u8>, u64)>,
    active: Option<Active>,
    pairing: Option<PairWindow>,
    stats: ReceiverStats,
    cpu_pct: u8,
    thermal: ThermalLevel,
    hs_tokens: f64,
    hs_at_us: u64,
}

struct Out {
    frames: VecDeque<ReceivedFrame>,
    audio: VecDeque<AudioFrame>,
}

type EventFn = Arc<dyn Fn(ReceiverEvent) + Send + Sync>;

struct Inner {
    cfg: ReceiverConfig,
    clock: Arc<dyn Clock>,
    transport: Arc<dyn Transport>,
    events: EventFn,
    stop: AtomicBool,
    st: Mutex<RState>,
    out: Mutex<Out>,
    cv: Condvar,
}

pub struct ReceiverSession {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

const LIVENESS_US: u64 = 3_000_000;
const REPORT_US: u64 = 500_000;

impl ReceiverSession {
    pub fn start(cfg: ReceiverConfig, transport: Arc<dyn Transport>, clock: Arc<dyn Clock>, events: impl Fn(ReceiverEvent) + Send + Sync + 'static) -> Self {
        let inner = Arc::new(Inner {
            cfg,
            clock,
            transport,
            events: Arc::new(events),
            stop: AtomicBool::new(false),
            st: Mutex::new(RState {
                pending: None,
                last_accept: None,
                active: None,
                pairing: None,
                stats: ReceiverStats { state: Some(ReceiverState::Listening), ..Default::default() },
                cpu_pct: 255,
                thermal: ThermalLevel::Unknown,
                hs_tokens: 20.0,
                hs_at_us: 0,
            }),
            out: Mutex::new(Out { frames: VecDeque::new(), audio: VecDeque::new() }),
            cv: Condvar::new(),
        });
        let i = inner.clone();
        let thread = std::thread::Builder::new().name("rvz-recv".into()).spawn(move || i.run()).expect("spawn");
        Self { inner, thread: Some(thread) }
    }

    /// Next reassembled video frame in decode order.
    pub fn next_frame(&self, timeout: Duration) -> Option<ReceivedFrame> {
        let mut o = self.inner.out.lock().unwrap();
        if o.frames.is_empty() {
            let (g, _) = self.inner.cv.wait_timeout(o, timeout).unwrap();
            o = g;
        }
        o.frames.pop_front()
    }

    pub fn next_audio(&self) -> Option<AudioFrame> {
        self.inner.out.lock().unwrap().audio.pop_front()
    }

    /// Call when a frame has actually been shown. `decode_us` is the measured decode time.
    pub fn on_frame_presented(&self, pts_us: u64, decode_us: u32) {
        let now = self.inner.clock.now_us();
        let mut st = self.inner.st.lock().unwrap();
        if let Some(a) = st.active.as_mut() {
            a.decode_sum += decode_us as u64;
            a.decode_n += 1;
            if let Some(off) = a.sync.offset_us() {
                let lat = now as i64 + off - pts_us as i64;
                if lat >= 0 {
                    a.e2e_sum += lat as u64;
                    a.e2e_n += 1;
                }
            }
        }
    }

    /// The hardware decoder failed or produced garbage: ask for a keyframe now.
    pub fn report_decode_error(&self) {
        self.inner.request_keyframe(KeyframeReason::DecoderError);
    }

    pub fn update_device_signals(&self, cpu_pct: Option<u8>, thermal: Option<ThermalLevel>) {
        let mut st = self.inner.st.lock().unwrap();
        st.cpu_pct = cpu_pct.unwrap_or(255);
        st.thermal = thermal.unwrap_or(ThermalLevel::Unknown);
    }

    /// Opens a pairing window and returns the PIN to display.
    pub fn open_pairing(&self) -> String {
        let pin = generate_pin();
        self.inner.st.lock().unwrap().pairing = Some(PairWindow { pin: pin.clone(), guard: PairingGuard::new(5), attempt: None });
        (self.inner.events)(ReceiverEvent::PairingOpened { pin: pin.clone() });
        pin
    }

    pub fn close_pairing(&self) {
        if self.inner.st.lock().unwrap().pairing.take().is_some() {
            (self.inner.events)(ReceiverEvent::PairingClosed);
        }
    }

    /// Discovery announcement reflecting the current pairing state.
    pub fn announcement(&self, name: &str, media_port: u16) -> Announcement {
        let caps = &self.inner.cfg.caps;
        let best = |f: fn(&revizor_proto::CodecCap) -> u16| caps.codecs.iter().map(f).max().unwrap_or(0);
        Announcement {
            proto_version: PROTOCOL_VERSION,
            device_id: self.inner.cfg.identity.device_id(),
            name: name.to_string(),
            media_port,
            transports: caps.transports,
            codecs: caps.codecs.iter().map(|c| c.codec).collect::<Vec<Codec>>(),
            max_width: best(|c| c.max_width),
            max_height: best(|c| c.max_height),
            max_fps: best(|c| c.max_fps),
            pairing_open: self.inner.st.lock().unwrap().pairing.is_some(),
        }
    }

    pub fn stats(&self) -> ReceiverStats {
        let st = self.inner.st.lock().unwrap();
        let mut s = st.stats.clone();
        if let Some(a) = &st.active {
            let c = a.assembler.counters();
            s.frames_delivered = c.frames_delivered;
            s.frames_abandoned = c.frames_abandoned;
            s.frames_discarded = c.frames_discarded;
            s.packets_recovered_fec = c.packets_recovered_fec;
            s.packets_recovered_retx = c.packets_recovered_retx;
            s.nacks_sent = c.nacks_sent;
            s.keyframe_requests = c.keyframe_requests;
            s.buffered_frames = a.assembler.buffered_frames();
            s.rtt_us = a.sync.srtt_us();
        }
        s
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ReceiverSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn wrap(kind: PacketKind, body: &[u8]) -> Vec<u8> {
    let mut d = PacketHeader::new(kind, 0, 0).encode().to_vec();
    d.extend_from_slice(body);
    d
}

impl Inner {
    fn emit(&self, evs: Vec<ReceiverEvent>) {
        for e in evs {
            (self.events)(e);
        }
    }

    fn send_to(&self, d: &[u8], to: SocketAddr) {
        let _ = self.transport.send_to(d, to);
    }

    fn send_ctl(a: &Active, tr: &dyn Transport, c: &Control) {
        let body = c.encode();
        let _ = tr.send_to(&a.tx.seal(Channel::Control, &[&body]), a.peer);
    }

    fn request_keyframe(&self, reason: KeyframeReason) {
        let now = self.clock.now_us();
        let mut st = self.st.lock().unwrap();
        if let Some(a) = st.active.as_mut() {
            let epoch = a.params.map_or(0, |p| p.epoch);
            Self::send_ctl(a, &*self.transport, &Control::KeyframeRequest { epoch, reason });
            a.assembler.note_keyframe_requested(now);
        }
    }

    fn run(self: Arc<Self>) {
        let mut buf = vec![0u8; 2048];
        let mut last_tick = 0u64;
        while !self.stop.load(Ordering::Relaxed) {
            let got = self.transport.recv_from(&mut buf, Duration::from_millis(2));
            let now = self.clock.now_us();
            let mut evs = Vec::new();
            if let Ok(Some((n, from))) = got {
                self.handle(&buf[..n], from, now, &mut evs);
            } else if let Err(_) = got {
                std::thread::sleep(Duration::from_millis(2));
            }
            if now.saturating_sub(last_tick) >= 1_000 {
                last_tick = now;
                self.tick(now, &mut evs);
            }
            self.emit(evs);
        }
        // tell the sender we are going away
        let mut st = self.st.lock().unwrap();
        if let Some(a) = st.active.as_ref() {
            Self::send_ctl(a, &*self.transport, &Control::Bye(revizor_proto::ByeReason::UserStopped));
        }
        st.stats.state = Some(ReceiverState::Stopped);
        drop(st);
        (self.events)(ReceiverEvent::State(ReceiverState::Stopped));
    }

    fn handle(&self, data: &[u8], from: SocketAddr, now: u64, evs: &mut Vec<ReceiverEvent>) {
        let Ok((h, body)) = PacketHeader::decode(data) else { return };
        match h.kind {
            PacketKind::Handshake => self.on_handshake(body, from, now, evs),
            PacketKind::Pairing => self.on_pairing(body, from, now, evs),
            PacketKind::Data => self.on_data(data, from, now, evs),
        }
    }

    // ───────────────────────────── handshake ─────────────────────────────

    fn on_handshake(&self, body: &[u8], from: SocketAddr, now: u64, evs: &mut Vec<ReceiverEvent>) {
        let mut st = self.st.lock().unwrap();
        // Signature verification is the expensive part: cap handshake attempts per second.
        let dt = now.saturating_sub(st.hs_at_us) as f64 / 1e6;
        st.hs_at_us = now;
        st.hs_tokens = (st.hs_tokens + dt * 10.0).min(20.0);
        if st.hs_tokens < 1.0 {
            return;
        }
        match body.first() {
            Some(1) => {
                if let Some(p) = &st.pending {
                    if p.from == from && p.hello == body {
                        let r = wrap(PacketKind::Handshake, &p.reply);
                        drop(st);
                        self.send_to(&r, from);
                        return;
                    }
                }
                st.hs_tokens -= 1.0;
                let mut resp = Responder::new(self.cfg.identity.clone(), Arc::new(self.cfg.caps.clone()), self.cfg.trust.clone());
                match resp.on_client_hello(body) {
                    Ok(reply) => {
                        let pkt = wrap(PacketKind::Handshake, &reply);
                        st.pending = Some(Pending { from, hello: body.to_vec(), reply, responder: resp, at_us: now });
                        drop(st);
                        self.send_to(&pkt, from);
                    }
                    Err((e, rej)) => {
                        log::info!("handshake from {from} refused: {e}");
                        drop(st);
                        self.send_to(&wrap(PacketKind::Handshake, &rej), from);
                    }
                }
            }
            Some(3) => {
                // retransmitted Finish of an already established session → resend Accept
                if let Some((peer, acc, at)) = &st.last_accept {
                    if *peer == from && now.saturating_sub(*at) < 10_000_000 && st.pending.is_none() {
                        let pkt = wrap(PacketKind::Handshake, acc);
                        drop(st);
                        self.send_to(&pkt, from);
                        return;
                    }
                }
                st.hs_tokens -= 1.0;
                let Some(p) = st.pending.take() else { return };
                if p.from != from {
                    st.pending = Some(p);
                    return;
                }
                let mut responder = p.responder;
                match responder.on_client_finish(body) {
                    Ok((accept, est)) => {
                        let pkt = wrap(PacketKind::Handshake, &accept);
                        st.last_accept = Some((from, accept, now));
                        let sender_name = est.peer_caps.device_name.clone();
                        let id = device_id(&est.peer_identity);
                        let was_active = st.active.is_some();
                        let tx = Arc::new(est.tx);
                        st.active = Some(Active {
                            peer: from,
                            rx: est.rx,
                            tx,
                            assembler: Assembler::new(self.cfg.assembler.clone()),
                            loss: LossTracker::default(),
                            jitter: JitterEstimator::default(),
                            rate: RateMeter::default(),
                            sync: ClockSync::default(),
                            params: None,
                            dropped_unknown_epoch: false,
                            last_rx_us: now,
                            last_report_us: now,
                            last_ping_us: 0,
                            last_params_req_us: 0,
                            last_audio_id: None,
                            drop_until_key: true,
                            frames_ok: 0,
                            frames_bad: 0,
                            decode_sum: 0,
                            decode_n: 0,
                            e2e_sum: 0,
                            e2e_n: 0,
                            highest_frame: 0,
                            recovered_prev: 0,
                            expected_prev_loss: (0, 0),
                        });
                        st.stats = ReceiverStats {
                            state: Some(ReceiverState::Streaming { sender: sender_name.clone() }),
                            sender_name: sender_name.clone(),
                            ..Default::default()
                        };
                        drop(st);
                        self.out.lock().unwrap().frames.clear();
                        self.send_to(&pkt, from);
                        if was_active {
                            evs.push(ReceiverEvent::SenderDisconnected);
                        }
                        evs.push(ReceiverEvent::SenderConnected { name: sender_name.clone(), device_id: id });
                        evs.push(ReceiverEvent::State(ReceiverState::Streaming { sender: sender_name }));
                    }
                    Err(e) => log::info!("handshake finish from {from} failed: {e}"),
                }
            }
            _ => {}
        }
    }

    // ───────────────────────────── pairing ─────────────────────────────

    fn on_pairing(&self, body: &[u8], from: SocketAddr, now: u64, evs: &mut Vec<ReceiverEvent>) {
        let mut st = self.st.lock().unwrap();
        let Some(win) = st.pairing.as_mut() else { return };
        match body.first() {
            Some(1) => {
                if win.guard.exhausted() {
                    return;
                }
                // Every start counts as a guess; at most 5 per displayed PIN.
                win.guard.record_failure();
                let mut r = PairResponder::new(self.cfg.identity.clone(), "Revizor", &win.pin, self.cfg.trust.clone());
                if let Ok(reply) = r.on_p1(body) {
                    win.attempt = Some((from, r, now));
                    let pkt = wrap(PacketKind::Pairing, &reply);
                    drop(st);
                    self.send_to(&pkt, from);
                }
            }
            Some(3) => {
                let Some((peer, mut r, _)) = win.attempt.take() else { return };
                if peer != from {
                    return;
                }
                match r.on_p3(body) {
                    Ok((p4, dev)) => {
                        st.pairing = None;
                        drop(st);
                        self.send_to(&wrap(PacketKind::Pairing, &p4), from);
                        evs.push(ReceiverEvent::Paired { name: dev.name.clone(), device_id: dev.id() });
                        evs.push(ReceiverEvent::PairingClosed);
                    }
                    Err(_) => {
                        if win.guard.exhausted() {
                            st.pairing = None;
                            evs.push(ReceiverEvent::PairingLocked);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ───────────────────────────── data ─────────────────────────────

    fn on_data(&self, data: &[u8], from: SocketAddr, now: u64, evs: &mut Vec<ReceiverEvent>) {
        let mut st = self.st.lock().unwrap();
        let tr = &*self.transport;
        let Some(a) = st.active.as_mut() else { return };
        let Ok(op) = a.rx.open(data) else { return };
        a.last_rx_us = now;
        if a.peer != from {
            a.peer = from; // authenticated packet from a new address: the sender roamed
        }
        a.loss.on_packet(op.seq);
        a.rate.add(now, data.len());
        match op.channel {
            Channel::Control => {
                let Ok(c) = Control::decode(&op.payload) else { return };
                match c {
                    Control::Params(p) => {
                        let changed = a.params.map_or(true, |o| o != p);
                        let epoch_changed = a.params.map_or(true, |o| o.epoch != p.epoch);
                        a.params = Some(p);
                        if changed {
                            if epoch_changed && a.dropped_unknown_epoch {
                                a.dropped_unknown_epoch = false;
                                Self::send_ctl(a, tr, &Control::KeyframeRequest { epoch: p.epoch, reason: KeyframeReason::ConfigChange });
                                a.assembler.note_keyframe_requested(now);
                            }
                            st.stats.params = Some(p);
                            evs.push(ReceiverEvent::Params(p));
                        }
                    }
                    Control::Ping { id, t0_us } => {
                        let t2 = self.clock.now_us();
                        Self::send_ctl(a, tr, &Control::Pong { id, t0_us, t1_us: now, t2_us: t2 });
                    }
                    Control::Pong { t0_us, t1_us, t2_us, .. } => a.sync.on_pong(t0_us, t1_us, t2_us, now),
                    Control::Bye(_) => {
                        st.active = None;
                        st.stats.state = Some(ReceiverState::Listening);
                        evs.push(ReceiverEvent::SenderDisconnected);
                        evs.push(ReceiverEvent::State(ReceiverState::Listening));
                    }
                    _ => {}
                }
            }
            Channel::Video => {
                let mut r = Reader::new(&op.payload);
                let Ok(hdr) = MediaHeader::decode(&mut r) else { return };
                let Some(p) = a.params else {
                    a.dropped_unknown_epoch = true;
                    if now.saturating_sub(a.last_params_req_us) > 100_000 {
                        a.last_params_req_us = now;
                        Self::send_ctl(a, tr, &Control::ParamsRequest);
                    }
                    return;
                };
                if hdr.epoch != p.epoch && revizor_media::newer_u16(hdr.epoch, p.epoch) {
                    a.dropped_unknown_epoch = true;
                    if now.saturating_sub(a.last_params_req_us) > 100_000 {
                        a.last_params_req_us = now;
                        Self::send_ctl(a, tr, &Control::ParamsRequest);
                    }
                    return;
                }
                if hdr.pkt_idx == 0 && hdr.flags & revizor_proto::FLAG_RETRANSMIT == 0 {
                    a.jitter.update(now, hdr.pts_us);
                }
                a.assembler.on_media(now, &hdr, r.rest());
            }
            Channel::VideoFec => a.assembler.on_fec(now, &op.payload),
            Channel::Audio => {
                let mut r = Reader::new(&op.payload);
                let Ok(hdr) = MediaHeader::decode(&mut r) else { return };
                if hdr.pkt_count != 1 {
                    return;
                }
                if a.last_audio_id.is_some_and(|l| !revizor_media::newer_u32(hdr.frame_id, l)) {
                    return;
                }
                a.last_audio_id = Some(hdr.frame_id);
                let f = AudioFrame { epoch: hdr.epoch, pts_us: hdr.pts_us, data: r.rest().to_vec() };
                drop(st);
                let mut o = self.out.lock().unwrap();
                if o.audio.len() > 64 {
                    o.audio.pop_front();
                }
                o.audio.push_back(f);
            }
        }
    }

    // ───────────────────────────── timers ─────────────────────────────

    fn tick(&self, now: u64, evs: &mut Vec<ReceiverEvent>) {
        let mut st = self.st.lock().unwrap();
        let (cpu, thermal) = (st.cpu_pct, st.thermal);
        // expire half-finished handshakes / pairing attempts
        if st.pending.as_ref().is_some_and(|p| now.saturating_sub(p.at_us) > 5_000_000) {
            st.pending = None;
        }
        if let Some(w) = st.pairing.as_mut() {
            if w.attempt.as_ref().is_some_and(|(_, _, t)| now.saturating_sub(*t) > 5_000_000) {
                w.attempt = None;
                if w.guard.exhausted() {
                    st.pairing = None;
                    evs.push(ReceiverEvent::PairingLocked);
                }
            }
        }
        let RState { active, stats, .. } = &mut *st;
        let Some(a) = active.as_mut() else { return };
        let tr = &*self.transport;

        if now.saturating_sub(a.last_rx_us) > LIVENESS_US {
            *active = None;
            stats.state = Some(ReceiverState::Listening);
            evs.push(ReceiverEvent::SenderDisconnected);
            evs.push(ReceiverEvent::State(ReceiverState::Listening));
            return;
        }

        // assemble / deliver / repair
        let out = a.assembler.poll(now);
        for n in &out.nacks {
            Self::send_ctl(a, tr, &Control::Nack { epoch: n.epoch, frame_id: n.frame_id, missing: n.missing.clone() });
        }
        if let Some(reason) = out.keyframe_request {
            let epoch = a.params.map_or(0, |p| p.epoch);
            Self::send_ctl(a, tr, &Control::KeyframeRequest { epoch, reason });
        }
        let mut deliver: Vec<ReceivedFrame> = Vec::new();
        for f in out.frames {
            a.frames_ok += 1;
            a.highest_frame = f.frame_id;
            if a.drop_until_key && !f.keyframe {
                a.frames_bad += 1;
                continue;
            }
            if f.keyframe {
                a.drop_until_key = false;
            }
            if let Some(off) = a.sync.offset_us() {
                let lat = now as i64 + off - f.pts_us as i64;
                if lat >= 0 {
                    stats.network_latency_us = Some(lat as u32);
                }
            }
            deliver.push(f);
        }

        if now.saturating_sub(a.last_ping_us) >= 500_000 {
            a.last_ping_us = now;
            Self::send_ctl(a, tr, &Control::Ping { id: (now / 1000) as u32, t0_us: now });
            if let Some(rtt) = a.sync.srtt_us() {
                a.assembler.set_nack_interval_us(rtt as u64 * 3 / 2 + 2_000);
            }
        }

        if now.saturating_sub(a.last_report_us) >= REPORT_US {
            let dt_ms = (now - a.last_report_us) / 1000;
            a.last_report_us = now;
            let (expected, lost) = a.loss.take();
            let c = a.assembler.counters();
            let recovered_total = c.packets_recovered_fec + c.packets_recovered_retx;
            let recovered = (recovered_total - a.recovered_prev) as u32;
            a.recovered_prev = recovered_total;
            let abandoned_total = (c.frames_abandoned + c.frames_discarded) as u32;
            let dropped = abandoned_total.wrapping_sub(a.expected_prev_loss.0) + a.frames_bad;
            a.expected_prev_loss.0 = abandoned_total;
            let bps = a.rate.take_bps(now);
            let report = ReceiverReport {
                interval_ms: dt_ms as u32,
                packets_expected: expected,
                packets_lost: lost,
                packets_recovered: recovered,
                jitter_us: a.jitter.jitter_us(),
                recv_bitrate_bps: bps,
                frames_complete: a.frames_ok,
                frames_dropped: dropped,
                decode_us: if a.decode_n > 0 { (a.decode_sum / a.decode_n) as u32 } else { 0 },
                e2e_latency_us: if a.e2e_n > 0 { (a.e2e_sum / a.e2e_n) as u32 } else { 0 },
                highest_frame_id: a.highest_frame,
                cpu_pct: cpu,
                thermal: thermal.to_wire(),
            };
            Self::send_ctl(a, tr, &Control::Report(report));
            let secs = dt_ms.max(1) as f32 / 1000.0;
            let (fps, sdecode, se2e) = (a.frames_ok as f32 / secs, report.decode_us, report.e2e_latency_us);
            a.frames_ok = 0;
            a.frames_bad = 0;
            a.decode_sum = 0;
            a.decode_n = 0;
            a.e2e_sum = 0;
            a.e2e_n = 0;
            stats.fps = fps;
            stats.recv_bitrate_bps = bps;
            stats.loss_pct = if expected > 0 { 100.0 * lost as f32 / expected as f32 } else { 0.0 };
            stats.jitter_us = report.jitter_us;
            stats.decode_us = (sdecode > 0).then_some(sdecode);
            stats.e2e_latency_us = (se2e > 0).then_some(se2e);
        }
        drop(st);

        if !deliver.is_empty() {
            let mut overflow = false;
            {
                let mut o = self.out.lock().unwrap();
                for f in deliver {
                    if o.frames.len() >= self.cfg.max_queued_frames {
                        // The decoder cannot keep up. Dropping single deltas would corrupt the
                        // picture, so flush to the next keyframe.
                        let n = o.frames.len() as u64;
                        o.frames.clear();
                        overflow = true;
                        self.st.lock().unwrap().stats.frames_dropped_app += n + 1;
                        continue;
                    }
                    if overflow && !f.keyframe {
                        self.st.lock().unwrap().stats.frames_dropped_app += 1;
                        continue;
                    }
                    if f.keyframe {
                        overflow = false;
                    }
                    o.frames.push_back(f);
                }
            }
            self.cv.notify_all();
            if overflow {
                if let Some(a) = self.st.lock().unwrap().active.as_mut() {
                    a.drop_until_key = true;
                }
                self.request_keyframe(KeyframeReason::DecoderError);
            }
        }
    }
}

#[allow(dead_code)]
const _: (usize, TransportKind) = (MEDIA_HEADER_LEN, TransportKind::Udp);
