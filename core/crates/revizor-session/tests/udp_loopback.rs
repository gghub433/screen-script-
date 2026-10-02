//! Same sessions as the e2e tests but over real UDP sockets on loopback, to
//! exercise the production transport (peer address handling, socket tuning).

use revizor_adaptive::Profile;
use revizor_crypto::{Identity, MemoryTrustStore, TrustStore, TrustedDevice};
use revizor_proto::{Capabilities, Codec, CodecCap, Platform, Preference, TransportKind};
use revizor_session::*;
use revizor_transport::{Transport, UdpTransport};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn caps(name: &str) -> Capabilities {
    Capabilities {
        device_name: name.into(),
        platform: Platform::Linux,
        codecs: vec![CodecCap { codec: Codec::H264, max_width: 4096, max_height: 2304, max_fps: 60, hardware: true }],
        audio: vec![],
        transports: TransportKind::Udp.bit(),
        hdr: false,
        max_bitrate_bps: 100_000_000,
    }
}

#[test]
fn streams_over_real_udp_loopback() {
    let (ida, idb) = (Arc::new(Identity::generate()), Arc::new(Identity::generate()));
    let (ta, tb) = (Arc::new(MemoryTrustStore::new()), Arc::new(MemoryTrustStore::new()));
    ta.add(TrustedDevice { public: idb.public(), name: "rx".into() });
    tb.add(TrustedDevice { public: ida.public(), name: "tx".into() });

    let rx_t = Arc::new(UdpTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let peer: SocketAddr = rx_t.local_addr().unwrap();
    let receiver = ReceiverSession::start(ReceiverConfig::new(idb, tb, caps("rx")), rx_t, Arc::new(MonotonicClock), |_| {});

    let epoch = Arc::new(AtomicU64::new(0));
    let key = Arc::new(AtomicU64::new(0));
    let (e2, k2) = (epoch.clone(), key.clone());
    let tx_t = Arc::new(UdpTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let sender = SenderSession::start(
        SenderConfig {
            identity: ida,
            trust: ta,
            caps: caps("tx"),
            pref: Preference { want_audio: false, ..Default::default() },
            profile: Profile::Balanced,
            custom_tier: None,
            custom_bitrate: None,
            source: SourceInfo { width: 1920, height: 1080, refresh_hz: 60 },
            peer,
            size_align: 2,
            give_up_after: Duration::from_secs(10),
        },
        tx_t,
        Arc::new(MonotonicClock),
        move |e| match e {
            SenderEvent::Reconfigure { params, .. } => {
                e2.store(params.epoch as u64, Ordering::SeqCst);
                k2.store(1, Ordering::SeqCst);
            }
            SenderEvent::RequestKeyframe(_) => k2.store(1, Ordering::SeqCst),
            _ => {}
        },
    );

    let start = Instant::now();
    let mut got = 0;
    let mut sent = 0u64;
    while start.elapsed() < Duration::from_secs(3) {
        let ep = epoch.load(Ordering::SeqCst) as u16;
        if ep != 0 {
            let is_key = key.swap(0, Ordering::SeqCst) == 1;
            let data = vec![(sent % 251) as u8; if is_key { 60_000 } else { 12_000 }];
            sender.submit_video(ep, sender.clock().now_us(), is_key, data);
            sent += 1;
        }
        while let Some(f) = receiver.next_frame(Duration::from_millis(1)) {
            assert!(f.data.iter().all(|b| *b == f.data[0]));
            got += 1;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(got > 80, "received {got} of {sent}");
    let st = receiver.stats();
    assert!(st.rtt_us.is_some() && st.rtt_us.unwrap() < 20_000);
    sender.stop();
    receiver.stop();
}
