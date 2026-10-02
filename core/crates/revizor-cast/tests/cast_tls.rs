//! The Cast control channel over a REAL TLS connection (rustls on both sides, self-signed server certificate), proving
//! the accept-any-certificate client handshakes with a standard TLS 1.2/1.3 server and the whole Cast flow works through it.

mod common;

use revizor_cast::testkit::*;
use revizor_cast::{CastConfig, CastEvent, CastSession, CastState, Method, Tv};
use rustls::crypto::ring;
use rustls::pki_types::PrivateKeyDer;
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn cast_flow_over_real_tls_with_a_self_signed_device_certificate() {
    let ck = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let key = PrivateKeyDer::try_from(ck.key_pair.serialize_der()).unwrap();
    let cfg = Arc::new(
        ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![ck.cert.der().clone()], key)
            .unwrap(),
    );
    let cc = FakeChromecast::start(FakeCastConfig::default(), move |tcp| Box::new(StreamOwned::new(ServerConnection::new(cfg.clone()).unwrap(), tcp)));

    let tv = Tv { name: "TLS Chromecast".into(), ip: "127.0.0.1".parse().unwrap(), model: None, manufacturer: None, methods: vec![Method::Cast { port: cc.addr.port() }] };
    let log: Arc<Mutex<Vec<CastEvent>>> = Arc::new(Mutex::new(vec![]));
    let l2 = log.clone();
    // default connector = production TLS client
    let s = Arc::new(
        CastSession::start(&tv, CastConfig { bind: "127.0.0.1:0".parse().unwrap(), advertise_ip: Some("127.0.0.1".parse().unwrap()), ..Default::default() }, move |e| l2.lock().unwrap().push(e)).unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, st2) = (s.clone(), stop.clone());
    let feeder = std::thread::spawn(move || {
        let v = common::video_frames();
        let start = Instant::now();
        'o: for round in 0..10u64 {
            for (pts, au, key) in &v {
                let t = round * 120 * 33_333 + pts;
                while start.elapsed() < Duration::from_micros(t) {
                    if st2.load(Ordering::Relaxed) {
                        break 'o;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                s2.submit_video(t + 10_000_000, *key, au);
            }
        }
    });
    let t0 = Instant::now();
    while !log.lock().unwrap().contains(&CastEvent::State(CastState::Playing)) {
        assert!(t0.elapsed() < Duration::from_secs(25), "never reached Playing: {:?}", log.lock().unwrap());
        std::thread::sleep(Duration::from_millis(50));
    }
    let r = cc.received();
    assert!(r.messages.iter().any(|m| m.contains("LAUNCH")) && r.load_url.is_some());
    assert!(r.segments_ok >= 2);
    stop.store(true, Ordering::SeqCst);
    feeder.join().unwrap();
}
