//! LAN discovery over UDP broadcast. Announcements are unauthenticated hints;
//! trust is established only by the handshake (see revizor-crypto).

use revizor_proto::discovery::{Announcement, DiscoveryMsg, DISCOVERY_PORT};
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

fn socket(bind: SocketAddr) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    let _ = s.set_reuse_port(true);
    s.set_broadcast(true)?;
    s.bind(&bind.into())?;
    Ok(s.into())
}

/// Broadcast targets: the limited broadcast plus every interface's directed broadcast.
pub fn broadcast_targets(port: u16) -> Vec<SocketAddr> {
    let mut v = vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), port)];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            if i.is_loopback() {
                continue;
            }
            if let if_addrs::IfAddr::V4(a) = i.addr {
                if let Some(b) = a.broadcast {
                    v.push(SocketAddr::new(IpAddr::V4(b), port));
                }
            }
        }
    }
    v.dedup();
    v
}

/// Receiver side: answers probes and announces itself periodically.
pub struct DiscoveryResponder {
    stop: Arc<AtomicBool>,
    th: Option<JoinHandle<()>>,
}

impl DiscoveryResponder {
    /// `announcement` is called for every reply so it always reflects current
    /// state (e.g. pairing window open).
    pub fn start(
        bind: SocketAddr,
        announce_to: Vec<SocketAddr>,
        announcement: impl Fn() -> Announcement + Send + 'static,
    ) -> io::Result<Self> {
        let sock = socket(bind)?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let th = std::thread::Builder::new().name("rvz-discovery-resp".into()).spawn(move || {
            let mut buf = [0u8; 512];
            let mut last = Instant::now() - Duration::from_secs(10);
            while !st.load(Ordering::Relaxed) {
                if last.elapsed() >= Duration::from_secs(2) {
                    let m = DiscoveryMsg::Announce(announcement()).encode();
                    for t in &announce_to {
                        let _ = sock.send_to(&m, t);
                    }
                    last = Instant::now();
                }
                match sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if matches!(DiscoveryMsg::decode(&buf[..n]), Ok(DiscoveryMsg::Probe)) {
                            let _ = sock.send_to(&DiscoveryMsg::Announce(announcement()).encode(), from);
                        }
                    }
                    Err(_) => {}
                }
            }
        })?;
        Ok(Self { stop, th: Some(th) })
    }

    pub fn default_bind() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DISCOVERY_PORT)
    }
}

impl Drop for DiscoveryResponder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.th.take() {
            let _ = t.join();
        }
    }
}

#[derive(Debug, Clone)]
pub struct Found {
    pub announcement: Announcement,
    pub addr: IpAddr,
}

/// Sender side: probe and collect announcements for `duration`.
pub fn scan(targets: &[SocketAddr], duration: Duration) -> io::Result<Vec<Found>> {
    let sock = socket(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?;
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    let probe = DiscoveryMsg::Probe.encode();
    let end = Instant::now() + duration;
    let mut last_probe = Instant::now() - Duration::from_secs(1);
    let mut out: Vec<Found> = Vec::new();
    let mut buf = [0u8; 512];
    while Instant::now() < end {
        if last_probe.elapsed() >= Duration::from_millis(500) {
            for t in targets {
                let _ = sock.send_to(&probe, t);
            }
            last_probe = Instant::now();
        }
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            if let Ok(DiscoveryMsg::Announce(a)) = DiscoveryMsg::decode(&buf[..n]) {
                if let Some(f) = out.iter_mut().find(|f| f.announcement.device_id == a.device_id) {
                    f.announcement = a;
                    f.addr = from.ip();
                } else {
                    out.push(Found { announcement: a, addr: from.ip() });
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use revizor_proto::Codec;

    #[test]
    fn probe_finds_responder_on_loopback() {
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        // pick a free port first so the scanner knows where to probe
        let port = UdpSocket::bind(bind).unwrap().local_addr().unwrap().port();
        let bind = SocketAddr::new(bind.ip(), port);
        let ann = Announcement {
            proto_version: 1,
            device_id: "dev1".into(),
            name: "Test TV".into(),
            media_port: 4000,
            transports: 3,
            codecs: vec![Codec::H264],
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
            pairing_open: true,
        };
        let a2 = ann.clone();
        let _r = DiscoveryResponder::start(bind, vec![], move || a2.clone()).unwrap();
        let found = scan(&[bind], Duration::from_millis(1200)).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].announcement, ann);
    }
}
