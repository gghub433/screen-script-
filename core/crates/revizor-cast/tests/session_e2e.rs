//! Whole casting sessions against protocol-speaking fake TVs (see src/testkit.rs).
//! Video comes from a REAL encoded stream (tests/fixtures), fed in real time.

mod common;

use revizor_cast::testkit::*;
use revizor_cast::{CastConfig, CastEvent, CastSession, CastState, Method, Tv};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Log = Arc<Mutex<Vec<CastEvent>>>;

fn lo() -> std::net::IpAddr {
    "127.0.0.1".parse().unwrap()
}

fn cfg() -> CastConfig {
    CastConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertise_ip: Some(lo()),
        tv_start_timeout: Duration::from_secs(4),
        first_frame_timeout: Duration::from_secs(2),
        ..Default::default()
    }
}

fn start(tv: &Tv, c: CastConfig) -> (CastSession, Log) {
    let log: Log = Arc::new(Mutex::new(vec![]));
    let l2 = log.clone();
    let s = CastSession::start(tv, c, move |e| l2.lock().unwrap().push(e)).unwrap();
    (s, log)
}

/// Feed the fixture at real-time speed until `stop` or `secs` elapse.
fn feeder(s: Arc<CastSession>, secs: u64, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let v = common::video_frames();
        let start = Instant::now();
        let mut round = 0u64;
        'o: while start.elapsed() < Duration::from_secs(secs) {
            for (pts, au, key) in &v {
                let t = round * 120 * 33_333 + pts + 10_000_000;
                while start.elapsed() < Duration::from_micros(t - 10_000_000) {
                    if stop.load(Ordering::Relaxed) {
                        break 'o;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                s.submit_video(t, *key, au);
            }
            round += 1;
        }
    })
}

fn wait_for(what: &str, secs: u64, f: impl Fn() -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn states(log: &Log) -> Vec<CastState> {
    log.lock().unwrap().iter().filter_map(|e| if let CastEvent::State(s) = e { Some(s.clone()) } else { None }).collect()
}

fn dlna_tv(fake: &FakeDlnaTv) -> Tv {
    let (control_url, service_type) = fake.av_transport();
    Tv { name: "Fake DLNA TV".into(), ip: lo(), model: None, manufacturer: None, methods: vec![Method::Dlna { control_url, service_type }] }
}

fn cast_tv(addr: SocketAddr) -> Tv {
    Tv { name: "Fake Chromecast".into(), ip: lo(), model: None, manufacturer: None, methods: vec![Method::Cast { port: addr.port() }] }
}

#[test]
fn dlna_tv_plays_the_stream_end_to_end() {
    let fake = FakeDlnaTv::start(FakeDlnaConfig { name: "Fake DLNA TV".into(), ..Default::default() });
    let (s, log) = start(&dlna_tv(&fake), cfg());
    let s = Arc::new(s);
    let stop = Arc::new(AtomicBool::new(false));
    let f = feeder(s.clone(), 8, stop.clone());
    wait_for("Playing", 12, || states(&log).contains(&CastState::Playing));

    let r = fake.received();
    assert_eq!(r.actions.iter().filter(|a| *a == "SetAVTransportURI").count(), 1, "{:?}", r.actions);
    assert!(r.actions.contains(&"Play".to_string()));
    let uri = r.uri.clone().unwrap();
    assert!(uri.starts_with(s.stream_base_url()) && uri.ends_with("/live.ts"), "{uri}");
    let didl = r.didl.clone().unwrap();
    assert!(didl.contains("object.item.videoItem") && didl.contains("http-get:*:video/mpeg") && didl.contains("DLNA.ORG_OP=00"), "{didl}");
    assert!(r.head_probe, "TV probed with HEAD first");
    assert_eq!(r.content_type.as_deref(), Some("video/mpeg"));
    wait_for("enough data", 8, || fake.received().ts.video_pes >= 30);
    let r = fake.received();
    assert_eq!(r.ts.sync_errors, 0);
    assert!(r.ts.starts_with_pat, "the TV's stream must start with PAT (keyframe join)");
    assert!(log.lock().unwrap().contains(&CastEvent::NeedKeyframe), "joining TV must trigger a keyframe request");

    let st = s.stats();
    assert_eq!(st.method, "DLNA");
    assert!(st.viewers == 1 && st.bytes_served > 10_000 && st.frames_in > 30 && st.in_bps > 0, "{st:?}");

    stop.store(true, Ordering::SeqCst);
    f.join().unwrap();
    let s = Arc::try_unwrap(s).ok().unwrap();
    s.stop();
    assert!(fake.received().actions.contains(&"Stop".to_string()));
    assert_eq!(states(&log).last(), Some(&CastState::Stopped));
}

#[test]
fn dlna_tv_that_refuses_play_gives_a_clear_error() {
    let fake = FakeDlnaTv::start(FakeDlnaConfig { name: "Fake DLNA TV".into(), reject_play: Some((701, "Transition not available".into())), ..Default::default() });
    let (s, log) = start(&dlna_tv(&fake), cfg());
    let s = Arc::new(s);
    let stop = Arc::new(AtomicBool::new(false));
    let f = feeder(s.clone(), 4, stop.clone());
    wait_for("failure", 8, || states(&log).iter().any(|x| matches!(x, CastState::Failed(_))));
    let CastState::Failed(msg) = states(&log).into_iter().find(|x| matches!(x, CastState::Failed(_))).unwrap() else { unreachable!() };
    assert!(msg.contains("Fake DLNA TV") && msg.contains("701"), "{msg}");
    stop.store(true, Ordering::SeqCst);
    f.join().unwrap();
}

#[test]
fn dlna_tv_that_never_fetches_times_out_with_an_honest_message() {
    let fake = FakeDlnaTv::start(FakeDlnaConfig { name: "Fake DLNA TV".into(), never_fetch: true, ..Default::default() });
    let mut c = cfg();
    c.tv_start_timeout = Duration::from_secs(1);
    let (s, log) = start(&dlna_tv(&fake), c);
    let s = Arc::new(s);
    let stop = Arc::new(AtomicBool::new(false));
    let f = feeder(s.clone(), 8, stop.clone());
    wait_for("failure", 12, || states(&log).iter().any(|x| matches!(x, CastState::Failed(_))));
    let CastState::Failed(msg) = states(&log).into_iter().find(|x| matches!(x, CastState::Failed(_))).unwrap() else { unreachable!() };
    assert!(msg.contains("never started playing"), "{msg}");
    assert!(fake.received().actions.iter().filter(|a| *a == "SetAVTransportURI").count() >= 2, "one retry before giving up");
    stop.store(true, Ordering::SeqCst);
    f.join().unwrap();
}

#[test]
fn user_pressing_stop_on_the_tv_remote_ends_the_session() {
    let fake = FakeDlnaTv::start(FakeDlnaConfig { name: "Fake DLNA TV".into(), user_stops_after: Some(Duration::from_secs(3)), ..Default::default() });
    let (s, log) = start(&dlna_tv(&fake), cfg());
    let s = Arc::new(s);
    let stop = Arc::new(AtomicBool::new(false));
    let f = feeder(s.clone(), 12, stop.clone());
    wait_for("Playing", 10, || states(&log).contains(&CastState::Playing));
    wait_for("failure", 12, || states(&log).iter().any(|x| matches!(x, CastState::Failed(_))));
    let CastState::Failed(msg) = states(&log).into_iter().find(|x| matches!(x, CastState::Failed(_))).unwrap() else { unreachable!() };
    assert!(msg.contains("stopped"), "{msg}");
    stop.store(true, Ordering::SeqCst);
    f.join().unwrap();
}

#[test]
fn no_picture_from_the_encoder_is_reported_not_hidden() {
    let fake = FakeDlnaTv::start(FakeDlnaConfig { name: "T".into(), ..Default::default() });
    let mut c = cfg();
    c.first_frame_timeout = Duration::from_millis(600);
    let (_s, log) = start(&dlna_tv(&fake), c);
    wait_for("failure", 5, || states(&log).iter().any(|x| matches!(x, CastState::Failed(_))));
    assert!(fake.received().actions.is_empty(), "the TV must not be contacted before there is a picture");
}

#[test]
fn chromecast_plays_the_hls_stream_end_to_end() {
    let cc = FakeChromecast::plain(FakeCastConfig::default());
    let mut c = cfg();
    c.connector = Some(Arc::new(|a| Ok(Box::new(std::net::TcpStream::connect(a).map(|s| {
        s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        s
    })?) as Box<dyn revizor_cast::session::ReadWrite>)));
    let (s, log) = start(&cast_tv(cc.addr), c);
    let s = Arc::new(s);
    let stop = Arc::new(AtomicBool::new(false));
    let f = feeder(s.clone(), 14, stop.clone());
    wait_for("Playing", 20, || states(&log).contains(&CastState::Playing));

    let r = cc.received();
    let load = r.load_url.clone().unwrap();
    assert!(load.starts_with(s.stream_base_url()) && load.ends_with("/live.m3u8"), "{load}");
    assert!(r.messages.iter().any(|m| m.contains("\"type\":\"LAUNCH\"") && m.contains("CC1AD845")));
    assert!(r.messages.iter().any(|m| m.contains("\"type\":\"LOAD\"") && m.contains("\"streamType\":\"LIVE\"") && m.contains("application/x-mpegURL")));
    assert!(r.segments_ok >= 2 && r.segment_sync_errors == 0, "{r:?}");
    assert_eq!(s.stats().method, "Google Cast");

    stop.store(true, Ordering::SeqCst);
    f.join().unwrap();
    Arc::try_unwrap(s).ok().unwrap().stop();
    wait_for("STOP sent", 3, || cc.received().messages.iter().any(|m| m.contains("\"type\":\"STOP\"")));
}

#[test]
fn chromecast_error_and_load_failure_are_reported() {
    for (cfgc, expect) in [
        (FakeCastConfig { fail_load: true, ..Default::default() }, "refused"),
        (FakeCastConfig { error_after: Some(Duration::from_millis(100)), ..Default::default() }, "could not play"),
    ] {
        let cc = FakeChromecast::plain(cfgc);
        let mut c = cfg();
        c.connector = Some(Arc::new(|a| Ok(Box::new(std::net::TcpStream::connect(a).map(|s| {
            s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
            s
        })?) as Box<dyn revizor_cast::session::ReadWrite>)));
        let (s, log) = start(&cast_tv(cc.addr), c);
        let s = Arc::new(s);
        let stop = Arc::new(AtomicBool::new(false));
        let f = feeder(s.clone(), 10, stop.clone());
        wait_for("failure", 20, || states(&log).iter().any(|x| matches!(x, CastState::Failed(_))));
        let CastState::Failed(msg) = states(&log).into_iter().find(|x| matches!(x, CastState::Failed(_))).unwrap() else { unreachable!() };
        assert!(msg.contains(expect), "{msg}");
        stop.store(true, Ordering::SeqCst);
        f.join().unwrap();
    }
}

#[test]
fn unreachable_chromecast_falls_back_to_dlna() {
    let dlna = FakeDlnaTv::start(FakeDlnaConfig { name: "Both".into(), ..Default::default() });
    let closed_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (control_url, service_type) = dlna.av_transport();
    let tv = Tv { name: "Both".into(), ip: lo(), model: None, manufacturer: None, methods: vec![Method::Cast { port: closed_port }, Method::Dlna { control_url, service_type }] };
    let mut c = cfg();
    c.connector = Some(Arc::new(|a| Ok(Box::new(std::net::TcpStream::connect_timeout(&a, Duration::from_secs(1))?) as Box<dyn revizor_cast::session::ReadWrite>)));
    let (s, log) = start(&tv, c);
    let s = Arc::new(s);
    let stop = Arc::new(AtomicBool::new(false));
    let f = feeder(s.clone(), 10, stop.clone());
    wait_for("Playing via the second method", 15, || states(&log).contains(&CastState::Playing));
    assert!(log.lock().unwrap().contains(&CastEvent::TryingNext { method: "DLNA" }));
    assert_eq!(s.stats().method, "DLNA");
    stop.store(true, Ordering::SeqCst);
    f.join().unwrap();
}

#[test]
fn when_every_method_fails_the_user_sees_why_each_one_failed() {
    // Cast: nothing listens on the port. DLNA: the TV refuses Play. Both reasons must reach the user.
    let dlna = FakeDlnaTv::start(FakeDlnaConfig { name: "Both".into(), reject_play: Some((701, "Transition not available".into())), ..Default::default() });
    let closed_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (control_url, service_type) = dlna.av_transport();
    let tv = Tv { name: "Both".into(), ip: lo(), model: None, manufacturer: None, methods: vec![Method::Cast { port: closed_port }, Method::Dlna { control_url, service_type }] };
    let mut c = cfg();
    c.connector = Some(Arc::new(|a| Ok(Box::new(std::net::TcpStream::connect_timeout(&a, Duration::from_secs(1))?) as Box<dyn revizor_cast::session::ReadWrite>)));
    let (s, log) = start(&tv, c);
    let s = Arc::new(s);
    let stop = Arc::new(AtomicBool::new(false));
    let f = feeder(s.clone(), 8, stop.clone());
    wait_for("failure", 15, || states(&log).iter().any(|x| matches!(x, CastState::Failed(_))));
    let CastState::Failed(msg) = states(&log).into_iter().find(|x| matches!(x, CastState::Failed(_))).unwrap() else { unreachable!() };
    assert!(msg.contains("Google Cast: ") && msg.contains("Cannot reach Both"), "{msg}");
    assert!(msg.contains("DLNA: ") && msg.contains("701"), "{msg}");
    stop.store(true, Ordering::SeqCst);
    f.join().unwrap();
}

#[test]
fn a_tv_fetching_from_an_unexpected_address_is_blocked_and_explained() {
    // The TV is declared at 127.0.0.2 but the fake fetches from 127.0.0.1: the address filter must refuse it.
    let fake = FakeDlnaTv::start(FakeDlnaConfig { name: "Odd TV".into(), ..Default::default() });
    let (control_url, service_type) = fake.av_transport();
    let tv = Tv { name: "Odd TV".into(), ip: "127.0.0.2".parse().unwrap(), model: None, manufacturer: None, methods: vec![Method::Dlna { control_url, service_type }] };
    let mut c = cfg();
    c.tv_start_timeout = Duration::from_secs(1);
    let (s, log) = start(&tv, c);
    let s = Arc::new(s);
    let stop = Arc::new(AtomicBool::new(false));
    let f = feeder(s.clone(), 8, stop.clone());
    wait_for("failure", 12, || states(&log).iter().any(|x| matches!(x, CastState::Failed(_))));
    let CastState::Failed(msg) = states(&log).into_iter().find(|x| matches!(x, CastState::Failed(_))).unwrap() else { unreachable!() };
    assert!(msg.contains("blocked"), "{msg}");
    assert!(s.stats().rejected_requests >= 1);
    stop.store(true, Ordering::SeqCst);
    f.join().unwrap();
}

#[test]
fn discovery_finds_the_fake_dlna_tv_over_real_sockets() {
    let fake = FakeDlnaTv::start(FakeDlnaConfig { name: "Kitchen TV".into(), ..Default::default() });
    let replies = revizor_cast::ssdp::search(Duration::from_millis(1500), &[fake.ssdp_addr], &["127.0.0.1".parse().unwrap()]);
    assert_eq!(replies.len(), 1);
    let found = revizor_cast::ssdp::describe_all(replies);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Kitchen TV");
    assert_eq!(found[0].control_url, fake.control_url());
    let tvs = revizor_cast::discover::merge(found, vec![]);
    assert_eq!(tvs[0].methods.len(), 1);
}
