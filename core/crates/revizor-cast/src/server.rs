//! Tiny HTTP/1.1 server that serves the live stream to the TV.
//!
//! * `GET /<token>/live.m3u8` + `/<token>/seg-<n>.ts` — HLS (Google Cast / Default Media Receiver, many TVs)
//! * `GET /<token>/live.ts` — progressive MPEG-TS (DLNA renderers); close-delimited body, no chunking, for the
//!   widest TV compatibility; a new viewer starts at a fresh keyframe.
//! * CORS is enabled because Cast receivers fetch HLS with XHR.
//!
//! Access control: only the TV's IP address (plus loopback in tests) and only with the random token in the path.
//! Everything is bounded: ≤ `max_clients` viewers, ≤ 16 concurrent requests, each viewer has a fixed-size queue and
//! is disconnected rather than allowed to slow the encoder down.

use crate::hls::Segmenter;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DLNA_FEATURES: &str = "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

#[derive(Clone)]
pub struct ServerConfig {
    pub token: String,
    /// Peers allowed to fetch the stream. Empty = nobody.
    pub allow: Vec<IpAddr>,
    pub max_clients: usize,
    pub hls_target_us: u64,
    pub hls_window: usize,
    pub hls_min_segments: usize,
    /// Bounded per-viewer queue (TS chunks, one per video frame).
    pub client_queue: usize,
}

impl ServerConfig {
    pub fn new(token: String, allow: Vec<IpAddr>) -> Self {
        Self { token, allow, max_clients: 4, hls_target_us: 1_000_000, hls_window: 6, hls_min_segments: 2, client_queue: 120 }
    }
}

#[derive(Default)]
pub struct Stats {
    pub bytes_sent: AtomicU64,
    pub playlist_requests: AtomicU64,
    pub segment_requests: AtomicU64,
    pub progressive_connections: AtomicU64,
    pub viewers_now: AtomicUsize,
    pub slow_viewers_dropped: AtomicU64,
    pub rejected: AtomicU64,
}

struct Client {
    tx: SyncSender<Arc<Vec<u8>>>,
    need_key: bool,
}

struct State {
    seg: Segmenter,
    clients: Vec<Client>,
    last_kf_request: Option<Instant>,
}

pub struct Hub {
    cfg: ServerConfig,
    state: Mutex<State>,
    on_need_keyframe: Box<dyn Fn() + Send + Sync>,
    pub stats: Stats,
    stop: AtomicBool,
}

impl Hub {
    fn new(cfg: ServerConfig, on_need_keyframe: Box<dyn Fn() + Send + Sync>) -> Self {
        let seg = Segmenter::new(cfg.hls_target_us, cfg.hls_window, cfg.hls_min_segments);
        Self { cfg, state: Mutex::new(State { seg, clients: vec![], last_kf_request: None }), on_need_keyframe, stats: Stats::default(), stop: AtomicBool::new(false) }
    }

    /// TS packets of one video access unit.
    pub fn push_video(&self, pts_us: u64, keyframe: bool, ts: Vec<u8>) {
        if ts.is_empty() {
            return;
        }
        let chunk = Arc::new(ts);
        let mut st = self.state.lock().unwrap();
        st.seg.push_video(pts_us, keyframe, &chunk);
        let mut dropped = 0u64;
        st.clients.retain_mut(|c| {
            if c.need_key {
                if !keyframe {
                    return true;
                }
                c.need_key = false;
            }
            match c.tx.try_send(chunk.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    dropped += 1;
                    false
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
        self.stats.slow_viewers_dropped.fetch_add(dropped, Ordering::Relaxed);
    }

    /// Audio or keep-alive TS packets.
    pub fn push_other(&self, ts: Vec<u8>) {
        if ts.is_empty() {
            return;
        }
        let chunk = Arc::new(ts);
        let mut st = self.state.lock().unwrap();
        st.seg.push_other(&chunk);
        st.clients.retain_mut(|c| {
            if c.need_key {
                return true;
            }
            !matches!(c.tx.try_send(chunk.clone()), Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)))
        });
    }

    pub fn hls_ready(&self) -> bool {
        self.state.lock().unwrap().seg.ready()
    }
    pub fn segments_made(&self) -> u64 {
        self.state.lock().unwrap().seg.segments_made
    }
    pub fn viewers(&self) -> usize {
        self.stats.viewers_now.load(Ordering::Relaxed)
    }
    pub fn buffered_bytes(&self) -> usize {
        self.state.lock().unwrap().seg.buffered_bytes()
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.state.lock().unwrap().clients.clear();
    }

    fn add_client(&self) -> Option<std::sync::mpsc::Receiver<Arc<Vec<u8>>>> {
        let mut st = self.state.lock().unwrap();
        if st.clients.len() >= self.cfg.max_clients {
            return None;
        }
        let (tx, rx) = sync_channel(self.cfg.client_queue);
        st.clients.push(Client { tx, need_key: true });
        let ask = st.last_kf_request.map_or(true, |t| t.elapsed() > Duration::from_millis(300));
        if ask {
            st.last_kf_request = Some(Instant::now());
        }
        drop(st);
        if ask {
            (self.on_need_keyframe)();
        }
        Some(rx)
    }
}

pub struct Server {
    pub addr: SocketAddr,
    hub: Arc<Hub>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub fn start(bind: SocketAddr, cfg: ServerConfig, on_need_keyframe: impl Fn() + Send + Sync + 'static) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let hub = Arc::new(Hub::new(cfg, Box::new(on_need_keyframe)));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s) = (hub.clone(), stop.clone());
        let active = Arc::new(AtomicUsize::new(0));
        let thread = std::thread::Builder::new().name("rvz-cast-http".into()).spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((sock, peer)) => {
                        if active.load(Ordering::Relaxed) >= 16 {
                            h.stats.rejected.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        active.fetch_add(1, Ordering::Relaxed);
                        let (h2, a2) = (h.clone(), active.clone());
                        let _ = std::thread::Builder::new().name("rvz-cast-conn".into()).spawn(move || {
                            let _ = sock.set_nonblocking(false);
                            handle(sock, peer.ip(), &h2);
                            a2.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(15)),
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })?;
        Ok(Self { addr, hub, stop, thread: Some(thread) })
    }

    pub fn hub(&self) -> Arc<Hub> {
        self.hub.clone()
    }

    pub fn token(&self) -> &str {
        &self.hub.cfg.token
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.hub.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn read_request(sock: &mut TcpStream) -> Option<Request> {
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > 8192 {
            return None;
        }
        let n = sock.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers = lines.take_while(|l| !l.is_empty()).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    Some(Request { method, path, headers })
}

fn cors() -> &'static str {
    "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, HEAD, OPTIONS\r\nAccess-Control-Allow-Headers: Range, Content-Type, Origin, Accept\r\nAccess-Control-Expose-Headers: Content-Length, Content-Range\r\n"
}

fn respond(sock: &mut TcpStream, hub: &Hub, status: &str, ctype: &str, extra: &str, body: &[u8], head_only: bool) {
    let hdr = format!(
        "HTTP/1.1 {status}\r\nServer: Revizor\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-cache, no-store\r\nConnection: close\r\n{}{extra}\r\n",
        body.len(),
        cors()
    );
    let _ = sock.set_write_timeout(Some(Duration::from_secs(10)));
    if sock.write_all(hdr.as_bytes()).is_ok() && !head_only && sock.write_all(body).is_ok() {
        hub.stats.bytes_sent.fetch_add(body.len() as u64, Ordering::Relaxed);
    }
}

fn handle(mut sock: TcpStream, peer: IpAddr, hub: &Arc<Hub>) {
    let Some(req) = read_request(&mut sock) else { return };
    let head = req.method == "HEAD";
    if req.method == "OPTIONS" {
        respond(&mut sock, hub, "204 No Content", "text/plain", "", b"", true);
        return;
    }
    if req.method != "GET" && !head {
        respond(&mut sock, hub, "405 Method Not Allowed", "text/plain", "", b"", true);
        return;
    }
    if !hub.cfg.allow.contains(&peer) {
        hub.stats.rejected.fetch_add(1, Ordering::Relaxed);
        respond(&mut sock, hub, "403 Forbidden", "text/plain", "", b"forbidden", head);
        return;
    }
    let path = req.path.split('?').next().unwrap_or("");
    let prefix = format!("/{}/", hub.cfg.token);
    let Some(rest) = path.strip_prefix(&prefix) else {
        respond(&mut sock, hub, "404 Not Found", "text/plain", "", b"not found", head);
        return;
    };
    match rest {
        "live.m3u8" => {
            hub.stats.playlist_requests.fetch_add(1, Ordering::Relaxed);
            let pl = hub.state.lock().unwrap().seg.playlist();
            match pl {
                Some(p) => respond(&mut sock, hub, "200 OK", "application/vnd.apple.mpegurl", "", p.as_bytes(), head),
                None => respond(&mut sock, hub, "503 Service Unavailable", "text/plain", "Retry-After: 1\r\n", b"stream is starting", head),
            }
        }
        "live.ts" => progressive(sock, hub, &req, head),
        r if r.starts_with("seg-") && r.ends_with(".ts") => {
            hub.stats.segment_requests.fetch_add(1, Ordering::Relaxed);
            let seq = r["seg-".len()..r.len() - 3].parse::<u64>().ok();
            let data = seq.and_then(|s| hub.state.lock().unwrap().seg.segment(s));
            match data {
                Some(d) => respond(&mut sock, hub, "200 OK", "video/MP2T", "", &d, head),
                None => respond(&mut sock, hub, "404 Not Found", "text/plain", "", b"segment expired", head),
            }
        }
        _ => respond(&mut sock, hub, "404 Not Found", "text/plain", "", b"not found", head),
    }
}

fn progressive(mut sock: TcpStream, hub: &Arc<Hub>, req: &Request, head: bool) {
    let hdr = format!(
        "HTTP/1.1 200 OK\r\nServer: Revizor\r\nContent-Type: video/mpeg\r\nAccept-Ranges: none\r\nCache-Control: no-cache, no-store\r\nConnection: close\r\ntransferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {DLNA_FEATURES}\r\n{}\r\n",
        cors()
    );
    let _ = sock.set_write_timeout(Some(Duration::from_secs(10)));
    if sock.write_all(hdr.as_bytes()).is_err() || head {
        return;
    }
    let _ = req.header("getcontentFeatures.dlna.org"); // accepted; contentFeatures is always sent
    let Some(rx) = hub.add_client() else { return };
    hub.stats.progressive_connections.fetch_add(1, Ordering::Relaxed);
    hub.stats.viewers_now.fetch_add(1, Ordering::Relaxed);
    loop {
        if hub.stop.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => {
                if sock.write_all(&chunk).is_err() {
                    break;
                }
                hub.stats.bytes_sent.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break, // dropped by the hub (too slow) or shutdown
        }
    }
    hub.stats.viewers_now.fetch_sub(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts::{parse_for_tests, TsMuxer};
    use std::io::{BufRead, BufReader};

    fn lo() -> IpAddr {
        "127.0.0.1".parse().unwrap()
    }

    fn start(allow: Vec<IpAddr>) -> (Server, Arc<std::sync::atomic::AtomicUsize>) {
        let kf = Arc::new(AtomicUsize::new(0));
        let k2 = kf.clone();
        let s = Server::start("127.0.0.1:0".parse().unwrap(), ServerConfig::new("tok".into(), allow), move || {
            k2.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        (s, kf)
    }

    fn get(addr: SocketAddr, method: &str, path: &str) -> (String, Vec<(String, String)>, Vec<u8>) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(s, "{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut r = BufReader::new(s);
        let mut status = String::new();
        r.read_line(&mut status).unwrap();
        let mut headers = vec![];
        loop {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            if l == "\r\n" || l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.trim_end().split_once(':') {
                headers.push((k.to_string(), v.trim().to_string()));
            }
        }
        let mut body = vec![];
        let _ = r.read_to_end(&mut body);
        (status.trim().to_string(), headers, body)
    }

    fn header<'a>(h: &'a [(String, String)], k: &str) -> Option<&'a str> {
        h.iter().find(|(a, _)| a.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }

    fn feed_gop_stream(hub: &Hub, seconds: u64) {
        let mut m = TsMuxer::new(false);
        for i in 0..seconds * 30 {
            let key = i % 30 == 0;
            let mut au = vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }];
            au.extend(std::iter::repeat(0x77).take(500));
            let pts = i * 33_333;
            let ts = m.video(pts + 1, key, &au);
            hub.push_video(pts, key, ts);
        }
    }

    #[test]
    fn hls_flow_with_cors_and_valid_segments() {
        let (s, _) = start(vec![lo()]);
        let hub = s.hub();
        let (st, h, _) = get(s.addr, "GET", "/tok/live.m3u8");
        assert!(st.contains("503"), "{st}");
        assert_eq!(header(&h, "Retry-After"), Some("1"));

        feed_gop_stream(&hub, 4);
        let (st, h, body) = get(s.addr, "GET", "/tok/live.m3u8");
        assert!(st.contains("200"), "{st}");
        assert_eq!(header(&h, "Content-Type"), Some("application/vnd.apple.mpegurl"));
        assert_eq!(header(&h, "Access-Control-Allow-Origin"), Some("*"));
        let pl = String::from_utf8(body).unwrap();
        let first = pl.lines().find(|l| l.starts_with("seg-")).unwrap();
        let (st, h, seg) = get(s.addr, "GET", &format!("/tok/{first}"));
        assert!(st.contains("200"));
        assert_eq!(header(&h, "Content-Type"), Some("video/MP2T"));
        assert_eq!(header(&h, "Content-Length").unwrap().parse::<usize>().unwrap(), seg.len());
        let r = parse_for_tests(&seg);
        assert_eq!((r.sync_errors, r.cc_errors), (0, 0));
        assert!(r.first_is_psi, "segment must start with PAT");
        assert_eq!(r.video_pes.len(), 30);
        assert!(r.video_pes[0].1, "segment starts with a random-access frame");
        let (st, _, _) = get(s.addr, "GET", "/tok/seg-9999.ts");
        assert!(st.contains("404"));
        let (st, h, _) = get(s.addr, "OPTIONS", "/tok/live.m3u8");
        assert!(st.contains("204") && header(&h, "Access-Control-Allow-Headers").unwrap().contains("Range"));
    }

    #[test]
    fn token_and_ip_allowlist_are_enforced() {
        let (s, _) = start(vec![lo()]);
        assert!(get(s.addr, "GET", "/wrong/live.m3u8").0.contains("404"));
        assert!(get(s.addr, "GET", "/live.m3u8").0.contains("404"));
        assert!(get(s.addr, "POST", "/tok/live.m3u8").0.contains("405"));
        let (s2, _) = start(vec!["10.9.9.9".parse().unwrap()]);
        assert!(get(s2.addr, "GET", "/tok/live.m3u8").0.contains("403"));
        assert!(s2.hub().stats.rejected.load(Ordering::Relaxed) >= 1);
        let (s3, _) = start(vec![]);
        assert!(get(s3.addr, "GET", "/tok/live.ts").0.contains("403"));
    }

    #[test]
    fn progressive_viewer_starts_at_a_keyframe_after_requesting_one() {
        let (s, kf) = start(vec![lo()]);
        let hub = s.hub();
        let mut m = TsMuxer::new(false);
        let mut sock = TcpStream::connect(s.addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(sock, "GET /tok/live.ts HTTP/1.1\r\nHost: x\r\ngetcontentFeatures.dlna.org: 1\r\n\r\n").unwrap();
        // wait for the viewer to register, then feed delta frames (must be skipped) and an IDR
        let t0 = Instant::now();
        while kf.load(Ordering::SeqCst) == 0 && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(kf.load(Ordering::SeqCst), 1, "joining must ask the encoder for a keyframe");
        let idr = |m: &mut TsMuxer, pts: u64| m.video(pts, true, &[0, 0, 0, 1, 0x65, 1, 2, 3]);
        idr(&mut m, 0); // establishes time base (viewer not yet fed)
        for i in 1..5u64 {
            hub.push_video(i * 33_000, false, vec![0xAA; 188]); // deltas before the IDR: never delivered
        }
        let key_ts = idr(&mut m, 200_000);
        hub.push_video(200_000, true, key_ts);
        let mut got = vec![];
        let mut buf = [0u8; 4096];
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(2) {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    got.extend_from_slice(&buf[..n]);
                    if let Some(p) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        if got.len() >= p + 4 + 188 * 3 {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&got).to_string();
        assert!(text.starts_with("HTTP/1.1 200 OK"));
        assert!(text.contains("Content-Type: video/mpeg") && text.contains("transferMode.dlna.org: Streaming"));
        assert!(text.contains(&format!("contentFeatures.dlna.org: {DLNA_FEATURES}")));
        let body = &got[got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4..];
        assert_eq!(body[0], 0x47, "body is TS starting at a packet boundary");
        assert!(!body.windows(4).any(|w| w == [0xAA; 4]), "no pre-keyframe delta data");
        assert_eq!(hub.viewers(), 1);
    }

    #[test]
    fn head_request_returns_dlna_headers_without_body_or_viewer() {
        let (s, kf) = start(vec![lo()]);
        let (st, h, body) = get(s.addr, "HEAD", "/tok/live.ts");
        assert!(st.contains("200"));
        assert!(header(&h, "contentFeatures.dlna.org").unwrap().contains("DLNA.ORG_OP=00"));
        assert!(body.is_empty());
        assert_eq!(kf.load(Ordering::SeqCst), 0, "a probe must not trigger a keyframe or occupy a viewer slot");
        assert_eq!(s.hub().viewers(), 0);
    }

    #[test]
    fn slow_viewer_is_dropped_and_memory_stays_bounded() {
        let (s, _) = start(vec![lo()]);
        let hub = s.hub();
        let mut sock = TcpStream::connect(s.addr).unwrap();
        write!(sock, "GET /tok/live.ts HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let t0 = Instant::now();
        while hub.state.lock().unwrap().clients.is_empty() && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
        }
        // never read from `sock`: the TCP buffers fill, the writer blocks, the hub queue fills and the viewer is dropped
        let chunk = vec![0x47u8; 188 * 700];
        for i in 0..4000u64 {
            hub.push_video(i * 33_000, true, chunk.clone());
        }
        assert!(hub.stats.slow_viewers_dropped.load(Ordering::Relaxed) >= 1);
        assert!(hub.state.lock().unwrap().clients.is_empty());
        assert!(hub.buffered_bytes() < 40 << 20, "HLS buffer must stay bounded");
    }

    #[test]
    fn viewer_limit() {
        let (s, _) = start(vec![lo()]);
        let hub = s.hub();
        let mut socks = vec![];
        for _ in 0..4 {
            let mut c = TcpStream::connect(s.addr).unwrap();
            write!(c, "GET /tok/live.ts HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            socks.push(c);
        }
        let t0 = Instant::now();
        while hub.state.lock().unwrap().clients.len() < 4 && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(hub.state.lock().unwrap().clients.len(), 4);
        let mut extra = TcpStream::connect(s.addr).unwrap();
        write!(extra, "GET /tok/live.ts HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        extra.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut b = Vec::new();
        let _ = extra.read_to_end(&mut b); // headers then immediate close
        assert_eq!(hub.state.lock().unwrap().clients.len(), 4);
    }
}
