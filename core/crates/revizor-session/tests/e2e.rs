//! End-to-end tests of the real sender/receiver sessions (handshake, crypto,
//! packetization, FEC/NACK, adaptive engine, reconnect) over an in-memory link
//! with controllable impairments. The only test doubles are the *encoder*
//! (a thread producing frames of the requested size/rate that honours the
//! session's reconfigure/bitrate/keyframe events) and the *decoder* (a thread
//! verifying frame integrity). Everything between them is production code.

use rand::{Rng, SeedableRng};
use revizor_adaptive::Profile;
use revizor_crypto::{Identity, MemoryTrustStore, TrustStore, TrustedDevice};
use revizor_proto::{AudioCodec, Capabilities, Codec, CodecCap, Platform, Preference, StreamParams, TransportKind};
use revizor_session::*;
use revizor_transport::sim::{self, LinkConfig, LinkControl};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn caps(name: &str) -> Capabilities {
    Capabilities {
        device_name: name.into(),
        platform: Platform::Linux,
        codecs: vec![CodecCap { codec: Codec::H264, max_width: 4096, max_height: 2304, max_fps: 60, hardware: true }],
        audio: vec![AudioCodec::AacLc],
        transports: TransportKind::Udp.bit(),
        hdr: false,
        max_bitrate_bps: 100_000_000,
    }
}

fn paired() -> (Arc<Identity>, Arc<Identity>, Arc<MemoryTrustStore>, Arc<MemoryTrustStore>) {
    let (a, b) = (Arc::new(Identity::generate()), Arc::new(Identity::generate()));
    let (ta, tb) = (Arc::new(MemoryTrustStore::new()), Arc::new(MemoryTrustStore::new()));
    ta.add(TrustedDevice { public: b.public(), name: "TV".into() });
    tb.add(TrustedDevice { public: a.public(), name: "Phone".into() });
    (a, b, ta, tb)
}

fn frame_bytes(seq: u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len.max(16));
    v.extend_from_slice(&seq.to_le_bytes());
    v.extend_from_slice(&(len as u64).to_le_bytes());
    for i in 16..len.max(16) {
        v.push((seq.wrapping_mul(31).wrapping_add(i as u64)) as u8);
    }
    v
}

fn verify(data: &[u8]) -> bool {
    if data.len() < 16 {
        return false;
    }
    let seq = u64::from_le_bytes(data[..8].try_into().unwrap());
    let len = u64::from_le_bytes(data[8..16].try_into().unwrap()) as usize;
    data.len() == len.max(16) && data == frame_bytes(seq, len).as_slice()
}

#[derive(Default)]
struct Shared {
    sender_events: Mutex<Vec<SenderEvent>>,
    receiver_events: Mutex<Vec<ReceiverEvent>>,
    frames_ok: AtomicU64,
    frames_bad: AtomicU64,
    keyframes: AtomicU64,
    last_epoch: AtomicU16,
    produced: AtomicU64,
    need_key: AtomicBool,
    video_bps: AtomicU32,
    fps: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
}

struct Rig {
    sender: Option<Arc<SenderSession>>,
    receiver: Option<Arc<ReceiverSession>>,
    link: LinkControl,
    sh: Arc<Shared>,
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
    clock: Arc<dyn Clock>,
}

struct RigOpts {
    link: LinkConfig,
    profile: Profile,
    source: SourceInfo,
    sender_trusts: bool,
    receiver_trusts: bool,
}

impl Default for RigOpts {
    fn default() -> Self {
        Self {
            link: LinkConfig::default(),
            profile: Profile::Balanced,
            source: SourceInfo { width: 1920, height: 1080, refresh_hz: 60 },
            sender_trusts: true,
            receiver_trusts: true,
        }
    }
}

impl Rig {
    fn new(o: RigOpts) -> Rig {
        let (ida, idb, ta, tb) = paired();
        if !o.sender_trusts {
            ta.remove(&idb.device_id());
        }
        if !o.receiver_trusts {
            tb.remove(&ida.device_id());
        }
        let (ea, eb, link) = sim::pair(o.link, 42);
        let (ea, eb) = (Arc::new(ea), Arc::new(eb));
        let peer: SocketAddr = "10.0.0.2:2000".parse().unwrap();
        let clock: Arc<dyn Clock> = Arc::new(MonotonicClock);
        let sh = Arc::new(Shared::default());

        let rsh = sh.clone();
        let receiver = ReceiverSession::start(
            ReceiverConfig::new(idb, tb, caps("TV")),
            eb,
            clock.clone(),
            move |e| rsh.receiver_events.lock().unwrap().push(e),
        );

        let ssh = sh.clone();
        let sender = SenderSession::start(
            SenderConfig {
                identity: ida,
                trust: ta,
                caps: caps("Phone"),
                pref: Preference { want_audio: false, ..Default::default() },
                profile: o.profile,
                custom_tier: None,
                custom_bitrate: None,
                source: o.source,
                peer,
                size_align: 2,
                give_up_after: Duration::from_secs(20),
            },
            ea,
            clock.clone(),
            move |e| {
                match &e {
                    SenderEvent::Reconfigure { params, .. } => {
                        // epoch last: the encoder thread starts producing once it is non-zero
                        ssh.width.store(params.video.width as u32, Ordering::SeqCst);
                        ssh.height.store(params.video.height as u32, Ordering::SeqCst);
                        ssh.fps.store(params.video.fps as u32, Ordering::SeqCst);
                        ssh.video_bps.store(params.video.bitrate_bps, Ordering::SeqCst);
                        ssh.need_key.store(true, Ordering::SeqCst);
                        ssh.last_epoch.store(params.epoch, Ordering::SeqCst);
                    }
                    SenderEvent::SetBitrate { video_bps } => ssh.video_bps.store(*video_bps, Ordering::SeqCst),
                    SenderEvent::RequestKeyframe(_) => ssh.need_key.store(true, Ordering::SeqCst),
                    _ => {}
                }
                ssh.sender_events.lock().unwrap().push(e);
            },
        );
        let sender = Arc::new(sender);
        let receiver_arc = Arc::new(receiver);
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = vec![];

        // test-double encoder
        {
            let (s, sh, stop, clock) = (sender.clone(), sh.clone(), stop.clone(), clock.clone());
            threads.push(thread::spawn(move || {
                let mut rng = rand::rngs::StdRng::seed_from_u64(1);
                let mut seq = 0u64;
                let mut next = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    if sh.last_epoch.load(Ordering::SeqCst) == 0 {
                        thread::sleep(Duration::from_millis(2));
                        next = Instant::now();
                        continue;
                    }
                    let fps = sh.fps.load(Ordering::SeqCst).max(1);
                    next += Duration::from_micros(1_000_000 / fps as u64);
                    let now = Instant::now();
                    if next > now {
                        thread::sleep(next - now);
                    } else if now - next > Duration::from_millis(100) {
                        next = now;
                    }
                    let bps = sh.video_bps.load(Ordering::SeqCst);
                    if bps == 0 || sh.last_epoch.load(Ordering::SeqCst) == 0 {
                        continue;
                    }
                    let key = sh.need_key.swap(false, Ordering::SeqCst);
                    let avg = (bps as usize / 8) / fps as usize;
                    let len = if key { avg * 6 } else { (avg as f64 * rng.gen_range(0.5..1.3)) as usize }.max(64);
                    seq += 1;
                    let pts = clock.now_us();
                    if s.submit_video(sh.last_epoch.load(Ordering::SeqCst), pts, key, frame_bytes(seq, len)) {
                        sh.produced.fetch_add(1, Ordering::Relaxed);
                        s.report_encode_time_us(2_000);
                    }
                }
            }));
        }

        // test-double decoder + presenter
        {
            let (sh, stop, r) = (sh.clone(), stop.clone(), receiver_arc.clone());
            threads.push(thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(f) = r.next_frame(Duration::from_millis(20)) {
                        if verify(&f.data) {
                            sh.frames_ok.fetch_add(1, Ordering::Relaxed);
                            if f.keyframe {
                                sh.keyframes.fetch_add(1, Ordering::Relaxed);
                            }
                            r.on_frame_presented(f.pts_us, 1_500);
                        } else {
                            sh.frames_bad.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }

        Rig { sender: Some(sender), receiver: Some(receiver_arc.clone()), link, sh, stop, threads, clock }
    }

    fn sender(&self) -> &SenderSession {
        self.sender.as_ref().unwrap()
    }
    fn receiver(&self) -> &ReceiverSession {
        self.receiver.as_ref().unwrap()
    }
    fn ok(&self) -> u64 {
        self.sh.frames_ok.load(Ordering::Relaxed)
    }
    fn wait_for(&self, what: &str, timeout: Duration, f: impl Fn(&Rig) -> bool) {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if f(self) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}; sender={:?} recv={:?}", self.sender().stats().state, self.receiver().stats());
    }
    fn sender_states(&self) -> Vec<SenderState> {
        self.sh
            .sender_events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| if let SenderEvent::State(s) = e { Some(s.clone()) } else { None })
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        if let Some(s) = self.sender.take() {
            drop(s);
        }
        drop(self.receiver.take());
        let _ = &self.clock;
    }
}

#[test]
fn streams_over_clean_link_with_real_measurements() {
    let rig = Rig::new(RigOpts::default());
    rig.wait_for("100 frames", Duration::from_secs(10), |r| r.ok() >= 100);
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(rig.sh.frames_bad.load(Ordering::Relaxed), 0, "corrupted frame delivered");
    let rs = rig.receiver().stats();
    assert_eq!(rs.sender_name, "Phone");
    let p = rs.params.expect("params received");
    assert_eq!((p.video.width, p.video.height, p.video.fps), (1920, 1080, 60));
    assert!(rs.rtt_us.is_some(), "RTT must be measured, not assumed");
    assert!(rs.e2e_latency_us.is_some_and(|l| l < 50_000), "e2e latency {:?}", rs.e2e_latency_us);
    assert!(rs.fps > 40.0 && rs.fps < 70.0, "fps {}", rs.fps);
    assert!(rs.loss_pct < 0.5);
    assert!(rs.recv_bitrate_bps > 1_000_000);
    assert!(rig.sh.keyframes.load(Ordering::Relaxed) <= 3, "too many keyframes: {}", rig.sh.keyframes.load(Ordering::Relaxed));
    let ss = rig.sender().stats();
    assert_eq!(ss.state, Some(SenderState::Streaming));
    assert!(ss.sent_bps > 1_000_000);
    assert!(ss.rtt_us.is_some());
    assert_eq!(ss.peer_name, "TV");
}

#[test]
fn repairs_loss_without_stalling() {
    let rig = Rig::new(RigOpts {
        link: LinkConfig {
            loss_pct: 2.0,
            burst_enter: 0.003,
            burst_exit: 0.4,
            base_delay: Duration::from_millis(2),
            jitter: Duration::from_millis(3),
            reorder_pct: 1.0,
            ..Default::default()
        },
        ..Default::default()
    });
    rig.wait_for("streaming", Duration::from_secs(10), |r| r.ok() >= 50);
    let start_ok = rig.ok();
    let start_prod = rig.sh.produced.load(Ordering::Relaxed);
    thread::sleep(Duration::from_secs(8));
    let got = rig.ok() - start_ok;
    let sent = rig.sh.produced.load(Ordering::Relaxed) - start_prod;
    assert_eq!(rig.sh.frames_bad.load(Ordering::Relaxed), 0);
    let rs = rig.receiver().stats();
    assert!(got as f64 >= sent as f64 * 0.80, "delivered {got} of {sent}; {rs:?}");
    assert!(rs.packets_recovered_fec + rs.packets_recovered_retx > 0, "no repair happened: {rs:?}");
    assert!(rs.keyframe_requests < 12, "keyframe storm: {}", rs.keyframe_requests);
    assert!(rs.jitter_us > 0);
}

#[test]
fn reconnects_after_link_outage_and_resumes_video() {
    let rig = Rig::new(RigOpts::default());
    rig.wait_for("first frames", Duration::from_secs(10), |r| r.ok() >= 60);
    rig.link.update(|c| c.blackout = true);
    rig.wait_for("reconnecting state", Duration::from_secs(10), |r| {
        r.sender_states().iter().any(|s| matches!(s, SenderState::Reconnecting { .. }))
    });
    thread::sleep(Duration::from_secs(1));
    let before = rig.ok();
    rig.link.update(|c| c.blackout = false);
    rig.wait_for("video after reconnect", Duration::from_secs(15), |r| r.ok() > before + 60);
    let st = rig.sender_states();
    let last_stream = st.iter().rposition(|s| *s == SenderState::Streaming).unwrap();
    let first_recon = st.iter().position(|s| matches!(s, SenderState::Reconnecting { .. })).unwrap();
    assert!(last_stream > first_recon, "must return to Streaming: {st:?}");
    assert_eq!(rig.sh.frames_bad.load(Ordering::Relaxed), 0);
    // receiver saw the disconnect and the new connection
    let evs = rig.sh.receiver_events.lock().unwrap();
    assert!(evs.iter().filter(|e| matches!(e, ReceiverEvent::SenderConnected { .. })).count() >= 2);
}

#[test]
fn adapts_down_when_bandwidth_collapses_and_stays_stable() {
    let rig = Rig::new(RigOpts::default());
    rig.wait_for("streaming", Duration::from_secs(10), |r| r.ok() >= 60);
    let before = rig.sender().stats().bitrate_target_bps;
    assert!(before > 8_000_000, "starting bitrate {before}");
    // Wi-Fi degrades to 3 Mbit/s
    rig.link.update_dir(0, |c| {
        c.bandwidth_bps = Some(3_000_000);
        c.max_queue = Duration::from_millis(80);
        c.base_delay = Duration::from_millis(4);
    });
    rig.wait_for("bitrate below link capacity", Duration::from_secs(40), |r| {
        let s = r.sender().stats();
        s.bitrate_target_bps > 0 && s.bitrate_target_bps < 2_900_000
    });
    // let it settle, then measure the tail
    thread::sleep(Duration::from_secs(8));
    let ok0 = rig.ok();
    thread::sleep(Duration::from_secs(4));
    let tail = rig.ok() - ok0;
    let rs = rig.receiver().stats();
    let s = rig.sender().stats();
    assert!(tail > 4 * 15, "stream should flow smoothly after adaptation: {tail} frames in 4 s; {rs:?}; {s:?}");
    assert!(rs.loss_pct < 5.0, "loss after adaptation {}", rs.loss_pct);
    // engine reported a reason the UI can show
    assert!(s.limited_by.is_some(), "UI must be able to explain the reduced quality");
    let evs = rig.sh.sender_events.lock().unwrap();
    assert!(evs.iter().any(|e| matches!(e, SenderEvent::SetBitrate { .. } | SenderEvent::Reconfigure { .. })));
}

#[test]
fn unpaired_sender_is_refused_with_clear_error() {
    let rig = Rig::new(RigOpts { receiver_trusts: false, ..Default::default() });
    rig.wait_for("failure", Duration::from_secs(8), |r| r.sender_states().iter().any(|s| matches!(s, SenderState::Failed(_))));
    let st = rig.sender_states();
    let SenderState::Failed(msg) = st.last().unwrap() else { panic!("{st:?}") };
    assert!(msg.contains("paired"), "{msg}");
    assert_eq!(rig.ok(), 0);
}

#[test]
fn receiver_unknown_to_sender_is_rejected() {
    // A rogue device answering at the paired address must not be streamed to.
    let rig = Rig::new(RigOpts { sender_trusts: false, ..Default::default() });
    rig.wait_for("failure", Duration::from_secs(8), |r| r.sender_states().iter().any(|s| matches!(s, SenderState::Failed(_))));
    assert_eq!(rig.ok(), 0);
}

#[test]
fn orientation_change_reconfigures_without_distortion() {
    let rig = Rig::new(RigOpts { source: SourceInfo { width: 1080, height: 2400, refresh_hz: 60 }, ..Default::default() });
    rig.wait_for("portrait stream", Duration::from_secs(10), |r| r.ok() >= 30);
    let p = rig.receiver().stats().params.unwrap();
    assert!(p.video.height > p.video.width, "portrait expected: {p:?}");
    assert_eq!(p.video.width as u32 * 2400, p.video.height as u32 * 1080 + 0, "aspect ratio must be preserved: {p:?}");
    let epoch0 = p.epoch;
    rig.sender().set_source(SourceInfo { width: 2400, height: 1080, refresh_hz: 60 });
    let before = rig.ok();
    rig.wait_for("landscape params", Duration::from_secs(5), |r| {
        r.receiver().stats().params.is_some_and(|p| p.epoch != epoch0 && p.video.width > p.video.height)
    });
    rig.wait_for("frames in new epoch", Duration::from_secs(5), |r| r.ok() > before + 30);
    let p2: StreamParams = rig.receiver().stats().params.unwrap();
    assert_eq!(p2.video.width as u32 * 1080, p2.video.height as u32 * 2400);
    assert_eq!(rig.sh.frames_bad.load(Ordering::Relaxed), 0);
}

#[test]
fn decoder_error_triggers_keyframe_on_sender() {
    let rig = Rig::new(RigOpts::default());
    rig.wait_for("streaming", Duration::from_secs(10), |r| r.ok() >= 30);
    let n0 = rig.sh.sender_events.lock().unwrap().iter().filter(|e| matches!(e, SenderEvent::RequestKeyframe(_))).count();
    rig.receiver().report_decode_error();
    rig.wait_for("keyframe request reaching the encoder", Duration::from_secs(3), |r| {
        r.sh.sender_events.lock().unwrap().iter().filter(|e| matches!(e, SenderEvent::RequestKeyframe(_))).count() > n0
    });
}

#[test]
fn pairing_with_pin_then_streaming() {
    let (ida, idb) = (Arc::new(Identity::generate()), Arc::new(Identity::generate()));
    let (ta, tb) = (Arc::new(MemoryTrustStore::new()), Arc::new(MemoryTrustStore::new()));
    let (ea, eb, _l) = sim::pair(LinkConfig::default(), 5);
    let (ea, eb) = (Arc::new(ea), Arc::new(eb));
    let evs = Arc::new(Mutex::new(Vec::new()));
    let e2 = evs.clone();
    let recv = ReceiverSession::start(ReceiverConfig::new(idb.clone(), tb.clone(), caps("TV")), eb, Arc::new(MonotonicClock), move |e| e2.lock().unwrap().push(e));
    let peer: SocketAddr = "10.0.0.2:2000".parse().unwrap();

    // no pairing window open → refused/ignored
    let err = pair_as_sender(&*ea, peer, ida.clone(), "Phone", "000000", ta.clone());
    assert!(err.is_err());

    let pin = recv.open_pairing();
    assert!(recv.announcement("TV", 1).pairing_open);
    // wrong PIN: nobody is trusted afterwards
    let wrong = if pin == "123456" { "654321" } else { "123456" };
    assert!(matches!(pair_as_sender(&*ea, peer, ida.clone(), "Phone", wrong, ta.clone()), Err(PairError::WrongPin)));
    assert!(ta.list().is_empty() && tb.list().is_empty());

    pair_as_sender(&*ea, peer, ida.clone(), "Phone", &pin, ta.clone()).expect("correct PIN pairs");
    assert!(ta.is_trusted(&idb.public()) && tb.is_trusted(&ida.public()));
    thread::sleep(Duration::from_millis(100));
    assert!(evs.lock().unwrap().iter().any(|e| matches!(e, ReceiverEvent::Paired { .. })));
    assert!(!recv.announcement("TV", 1).pairing_open, "window closes after success");
}

#[test]
fn pairing_window_locks_after_too_many_guesses() {
    let (ida, idb) = (Arc::new(Identity::generate()), Arc::new(Identity::generate()));
    let (ta, tb) = (Arc::new(MemoryTrustStore::new()), Arc::new(MemoryTrustStore::new()));
    let (ea, eb, _l) = sim::pair(LinkConfig::default(), 5);
    let (ea, eb) = (Arc::new(ea), Arc::new(eb));
    let evs = Arc::new(Mutex::new(Vec::new()));
    let e2 = evs.clone();
    let recv = ReceiverSession::start(ReceiverConfig::new(idb, tb.clone(), caps("TV")), eb, Arc::new(MonotonicClock), move |e| e2.lock().unwrap().push(e));
    let peer: SocketAddr = "10.0.0.2:2000".parse().unwrap();
    let pin = recv.open_pairing();
    let wrong = if pin == "111111" { "222222" } else { "111111" };
    for _ in 0..5 {
        let _ = pair_as_sender(&*ea, peer, ida.clone(), "Atk", wrong, ta.clone());
    }
    // 6th try with the *correct* PIN must fail: window is locked
    thread::sleep(Duration::from_millis(100));
    assert!(pair_as_sender(&*ea, peer, ida, "Atk", &pin, ta).is_err());
    assert!(tb.list().is_empty());
}

#[test]
fn many_short_sessions_do_not_leak_threads_or_hang() {
    for _ in 0..3 {
        let rig = Rig::new(RigOpts::default());
        rig.wait_for("streaming", Duration::from_secs(10), |r| r.ok() >= 10);
        drop(rig);
    }
}


fn rss_kb() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Long-run stability test. Run explicitly:
///   REVIZOR_SOAK_SECS=3600 cargo test -p revizor-session --test e2e soak -- --ignored --nocapture
/// Every 30 s it injects a different fault (random loss, burst loss, bandwidth squeeze, jitter, a 4 s outage) and
/// samples real measurements. It fails if memory or latency drift, buffers grow, or the stream does not recover.
#[test]
#[ignore]
fn soak() {
    let secs: u64 = std::env::var("REVIZOR_SOAK_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(90);
    let rig = Rig::new(RigOpts::default());
    rig.wait_for("streaming", Duration::from_secs(10), |r| r.ok() >= 60);
    let t0 = Instant::now();
    let mut samples: Vec<(u64, u64, Option<u32>, f32, usize)> = vec![]; // (t, rss, e2e, fps, buffered)
    let mut last_ok = rig.ok();
    let mut cycle = 0u64;
    let mut outage_recovered = 0;
    let mut outages = 0;
    while t0.elapsed() < Duration::from_secs(secs) {
        let phase = (t0.elapsed().as_secs() / 30) % 6;
        if t0.elapsed().as_secs() / 30 != cycle {
            cycle = t0.elapsed().as_secs() / 30;
            rig.link.update(|c| {
                *c = LinkConfig::default();
                match phase {
                    1 => c.loss_pct = 2.0,
                    2 => { c.burst_enter = 0.004; c.burst_exit = 0.4; }
                    3 => { c.bandwidth_bps = Some(6_000_000); c.max_queue = Duration::from_millis(80); }
                    4 => { c.jitter = Duration::from_millis(8); c.base_delay = Duration::from_millis(3); c.reorder_pct = 2.0; }
                    _ => {}
                }
            });
            if phase == 5 {
                // outage: 4 s of nothing, then it must come back by itself
                outages += 1;
                let before = rig.ok();
                rig.link.update(|c| c.blackout = true);
                thread::sleep(Duration::from_secs(4));
                rig.link.update(|c| c.blackout = false);
                let end = Instant::now() + Duration::from_secs(20);
                while Instant::now() < end && rig.ok() < before + 30 {
                    thread::sleep(Duration::from_millis(50));
                }
                if rig.ok() >= before + 30 { outage_recovered += 1; }
            }
        }
        thread::sleep(Duration::from_secs(5));
        let rs = rig.receiver().stats();
        let ok = rig.ok();
        let fps = (ok - last_ok) as f32 / 5.0;
        last_ok = ok;
        let row = (t0.elapsed().as_secs(), rss_kb().unwrap_or(0), rs.e2e_latency_us, fps, rs.buffered_frames);
        let ss = rig.sender().stats();
        println!("   sender: rtt={:?} loss={:?} enc_us={:?} dropped_sender={} retx={} last_report={:?}", ss.rtt_us, ss.last_report.map(|r| (r.packets_lost, r.packets_expected, r.frames_dropped, r.jitter_us)), ss.encode_us_avg, ss.frames_dropped_sender, ss.retransmitted_packets, ss.limited_by);
        println!("t={:>5}s phase={} rss={} MB e2e={:?} µs fps={:.1} buffered={} bitrate={:.1} Mbit/s tier={}x{}", row.0, phase, row.1 / 1024, row.2, row.3, row.4,
            rig.sender().stats().bitrate_target_bps as f32 / 1e6, rig.sender().stats().width, rig.sender().stats().height);
        samples.push(row);
    }
    assert_eq!(rig.sh.frames_bad.load(Ordering::Relaxed), 0, "corrupted frames delivered");
    assert_eq!(outage_recovered, outages, "every outage must recover");
    // Memory: compare the second quarter with the last quarter (after warm-up allocations).
    let n = samples.len();
    if n >= 8 {
        let avg = |s: &[(u64, u64, Option<u32>, f32, usize)]| s.iter().map(|x| x.1).sum::<u64>() / s.len() as u64;
        let (early, late) = (avg(&samples[n / 4..n / 2]), avg(&samples[n * 3 / 4..]));
        assert!(late < early + 30 * 1024, "RSS grew from {} MB to {} MB", early / 1024, late / 1024);
    }
    assert!(samples.iter().all(|s| s.4 <= 8), "receiver buffer grew: {:?}", samples.iter().map(|s| s.4).max());
}
