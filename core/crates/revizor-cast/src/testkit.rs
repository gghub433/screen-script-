//! Test doubles standing in for real TVs. They speak the real protocols (SSDP + UPnP/SOAP + HTTP fetch for a
//! DLNA renderer; CASTV2 + HLS fetch for a Chromecast) so the session code is exercised end-to-end, but they are
//! of course not real televisions: real devices have quirks that only real hardware reveals.
//! Compiled only with the `testkit` feature (enabled automatically for this crate's own tests).

use crate::castv2::{self, CastMessage, FrameReader, DEFAULT_MEDIA_RECEIVER};
use crate::{http_client, ssdp, xml};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone)]
pub struct TsSummary {
    pub packets: usize,
    pub sync_errors: usize,
    pub video_pes: usize,
    pub starts_with_pat: bool,
}

/// Structural check of a TS byte stream (188-byte packets, sync bytes, PES starts on the video PID).
pub fn ts_summary(b: &[u8]) -> TsSummary {
    let mut s = TsSummary::default();
    for (n, p) in b.chunks_exact(188).enumerate() {
        s.packets += 1;
        if p[0] != 0x47 {
            s.sync_errors += 1;
            continue;
        }
        let pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
        if n == 0 {
            s.starts_with_pat = pid == 0;
        }
        if pid == 0x100 && p[1] & 0x40 != 0 {
            s.video_pes += 1;
        }
    }
    s
}

struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_req(s: &mut TcpStream) -> Option<Req> {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let split = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let mut lines = head.split("\r\n");
    let mut f = lines.next()?.split_whitespace();
    let (method, path) = (f.next()?.to_string(), f.next()?.to_string());
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let clen = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body = buf[split + 4..].to_vec();
    while body.len() < clen {
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some(Req { method, path, headers, body })
}

fn http_reply(s: &mut TcpStream, status: &str, ctype: &str, body: &str) {
    let _ = write!(s, "HTTP/1.1 {status}\r\nCONTENT-TYPE: {ctype}\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{body}", body.len());
}

// ───────────────────────────── fake DLNA renderer ─────────────────────────────

#[derive(Clone, Default)]
pub struct FakeDlnaConfig {
    pub name: String,
    /// Refuse `Play` with this UPnP error (code, description).
    pub reject_play: Option<(u32, String)>,
    /// Accept commands but never fetch the stream.
    pub never_fetch: bool,
    /// Behave like a user pressing STOP on the remote this long after playback started.
    pub user_stops_after: Option<Duration>,
}

#[derive(Default, Debug, Clone)]
pub struct DlnaReceived {
    pub actions: Vec<String>,
    pub uri: Option<String>,
    pub didl: Option<String>,
    pub head_probe: bool,
    pub bytes: usize,
    pub ts: TsSummary,
    pub content_type: Option<String>,
}

pub struct FakeDlnaTv {
    pub ssdp_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub received: Arc<Mutex<DlnaReceived>>,
    stop: Arc<AtomicBool>,
}

const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";

impl FakeDlnaTv {
    pub fn start(cfg: FakeDlnaConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let received = Arc::new(Mutex::new(DlnaReceived::default()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let http_addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(String::from("NO_MEDIA_PRESENT")));
        let player_stop = Arc::new(AtomicBool::new(false));

        // SSDP responder
        let usock = UdpSocket::bind("127.0.0.1:0").unwrap();
        usock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let ssdp_addr = usock.local_addr().unwrap();
        let st = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while !st.load(Ordering::Relaxed) {
                if let Ok((n, from)) = usock.recv_from(&mut buf) {
                    let t = String::from_utf8_lossy(&buf[..n]);
                    if t.starts_with("M-SEARCH") && (t.contains("MediaRenderer") || t.contains("AVTransport")) {
                        let r = format!("HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\nLOCATION: http://{http_addr}/desc.xml\r\nSERVER: FakeTV/1.0 UPnP/1.0\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: uuid:fake-{}::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n", http_addr.port());
                        let _ = usock.send_to(r.as_bytes(), from);
                    }
                }
            }
        });

        // HTTP: description + control
        let (st, rc) = (stop.clone(), received.clone());
        let cfg2 = cfg.clone();
        std::thread::spawn(move || {
            while !st.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        let (rc, state, cfg, pstop) = (rc.clone(), state.clone(), cfg2.clone(), player_stop.clone());
                        std::thread::spawn(move || {
                            let _ = s.set_nonblocking(false);
                            let Some(req) = read_req(&mut s) else { return };
                            if req.method == "GET" && req.path == "/desc.xml" {
                                let d = format!("<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType><friendlyName>{}</friendlyName><manufacturer>FakeCo</manufacturer><modelName>FK-1</modelName><UDN>uuid:fake-{}</UDN><serviceList><service><serviceType>{AVT}</serviceType><controlURL>/ctl/AVT</controlURL></service></serviceList></device></root>", xml::escape(&cfg.name), http_addr.port());
                                http_reply(&mut s, "200 OK", "text/xml", &d);
                                return;
                            }
                            if req.method == "POST" && req.path == "/ctl/AVT" {
                                let action = req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("soapaction")).map(|(_, v)| v.trim_matches('"').to_string()).unwrap_or_default();
                                let name = action.rsplit('#').next().unwrap_or("").to_string();
                                rc.lock().unwrap().actions.push(name.clone());
                                let body = String::from_utf8_lossy(&req.body).to_string();
                                let ok = |inner: &str| format!("<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{name}Response xmlns:u=\"{AVT}\">{inner}</u:{name}Response></s:Body></s:Envelope>");
                                let fault = |code: u32, d: &str| format!("<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode><errorDescription>{d}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>");
                                match name.as_str() {
                                    "SetAVTransportURI" => {
                                        let uri = xml::first_text(&body, "CurrentURI");
                                        let didl = xml::first_text(&body, "CurrentURIMetaData");
                                        {
                                            let mut r = rc.lock().unwrap();
                                            r.uri = uri;
                                            r.didl = didl;
                                        }
                                        *state.lock().unwrap() = "STOPPED".into();
                                        http_reply(&mut s, "200 OK", "text/xml", &ok(""));
                                    }
                                    "Play" => {
                                        if let Some((c, d)) = &cfg.reject_play {
                                            http_reply(&mut s, "500 Internal Server Error", "text/xml", &fault(*c, d));
                                            return;
                                        }
                                        http_reply(&mut s, "200 OK", "text/xml", &ok(""));
                                        if cfg.never_fetch {
                                            return;
                                        }
                                        *state.lock().unwrap() = "TRANSITIONING".into();
                                        pstop.store(false, Ordering::SeqCst);
                                        let uri = rc.lock().unwrap().uri.clone().unwrap_or_default();
                                        let (rc2, state2, pstop2, cfg3) = (rc.clone(), state.clone(), pstop.clone(), cfg.clone());
                                        std::thread::spawn(move || player(uri, rc2, state2, pstop2, cfg3));
                                    }
                                    "Stop" => {
                                        pstop.store(true, Ordering::SeqCst);
                                        *state.lock().unwrap() = "STOPPED".into();
                                        http_reply(&mut s, "200 OK", "text/xml", &ok(""));
                                    }
                                    "GetTransportInfo" => {
                                        let st = state.lock().unwrap().clone();
                                        http_reply(&mut s, "200 OK", "text/xml", &ok(&format!("<CurrentTransportState>{st}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>")));
                                    }
                                    _ => http_reply(&mut s, "500 Internal Server Error", "text/xml", &fault(401, "Invalid Action")),
                                }
                                return;
                            }
                            http_reply(&mut s, "404 Not Found", "text/plain", "no");
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Self { ssdp_addr, http_addr, received, stop }
    }

    pub fn control_url(&self) -> String {
        format!("http://{}/ctl/AVT", self.http_addr)
    }
    pub fn av_transport(&self) -> (String, String) {
        (self.control_url(), AVT.to_string())
    }
    pub fn received(&self) -> DlnaReceived {
        self.received.lock().unwrap().clone()
    }
}

impl Drop for FakeDlnaTv {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// What a DLNA TV does after Play: HEAD probe, then GET and keep reading the live stream.
fn player(uri: String, rc: Arc<Mutex<DlnaReceived>>, state: Arc<Mutex<String>>, stop: Arc<AtomicBool>, cfg: FakeDlnaConfig) {
    if let Ok(r) = http_client::request("HEAD", &uri, &[("getcontentFeatures.dlna.org", "1".into())], &[], Duration::from_secs(3)) {
        if r.status == 200 {
            rc.lock().unwrap().head_probe = true;
        }
    }
    let Some(u) = http_client::Url::parse(&uri) else { return };
    let Ok(mut s) = TcpStream::connect((u.host.as_str(), u.port)) else { return };
    let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = write!(s, "GET {} HTTP/1.1\r\nHOST: {}:{}\r\nUser-Agent: FakeTV\r\ngetcontentFeatures.dlna.org: 1\r\n\r\n", u.path, u.host, u.port);
    let (mut raw, mut hdr_done, mut body) = (Vec::new(), false, Vec::<u8>::new());
    let t0 = Instant::now();
    let mut tmp = [0u8; 16384];
    while !stop.load(Ordering::Relaxed) {
        if let Some(d) = cfg.user_stops_after {
            if t0.elapsed() > d {
                *state.lock().unwrap() = "STOPPED".into();
                return; // closes the connection
            }
        }
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if !hdr_done {
                    raw.extend_from_slice(&tmp[..n]);
                    if let Some(p) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        hdr_done = true;
                        let head = String::from_utf8_lossy(&raw[..p]).to_string();
                        rc.lock().unwrap().content_type = head.lines().find_map(|l| l.strip_prefix("Content-Type: ")).map(str::to_string);
                        body.extend_from_slice(&raw[p + 4..]);
                    }
                } else {
                    body.extend_from_slice(&tmp[..n]);
                }
                let mut r = rc.lock().unwrap();
                r.bytes = body.len();
                if body.len() >= 188 * 40 {
                    r.ts = ts_summary(&body[..body.len() / 188 * 188]);
                    if r.ts.video_pes >= 3 {
                        *state.lock().unwrap() = "PLAYING".into();
                    }
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
}

// ───────────────────────────── fake Chromecast ─────────────────────────────

#[derive(Clone, Default)]
pub struct FakeCastConfig {
    /// Answer `LOAD` with an error instead of playing.
    pub fail_load: bool,
    /// After starting to play, report `IDLE/ERROR` this long after.
    pub error_after: Option<Duration>,
    /// Drop the control connection this long after the app launched (first connection only).
    pub drop_connection_after: Option<Duration>,
}

#[derive(Default, Debug, Clone)]
pub struct CastReceived {
    pub messages: Vec<String>,
    pub load_url: Option<String>,
    pub playlist_fetches: usize,
    pub segments_ok: usize,
    pub segment_sync_errors: usize,
    pub connections: usize,
}

pub struct FakeChromecast {
    pub addr: SocketAddr,
    pub received: Arc<Mutex<CastReceived>>,
    stop: Arc<AtomicBool>,
}

impl FakeChromecast {
    /// Plain-TCP server (tests wrap accepted sockets in TLS themselves when they want to).
    pub fn start(cfg: FakeCastConfig, wrap: impl Fn(TcpStream) -> Box<dyn crate::session::ReadWrite> + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let received = Arc::new(Mutex::new(CastReceived::default()));
        let (st, rc) = (stop.clone(), received.clone());
        let wrap = Arc::new(wrap);
        std::thread::spawn(move || {
            while !st.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((s, _)) => {
                        let _ = s.set_nonblocking(false);
                        let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
                        let n = {
                            let mut r = rc.lock().unwrap();
                            r.connections += 1;
                            r.connections
                        };
                        let (rc2, cfg2, st2, w) = (rc.clone(), cfg.clone(), st.clone(), wrap.clone());
                        std::thread::spawn(move || serve(w(s), rc2, cfg2, st2, n));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Self { addr, received, stop }
    }
    pub fn plain(cfg: FakeCastConfig) -> Self {
        Self::start(cfg, |s| Box::new(s))
    }
    pub fn received(&self) -> CastReceived {
        self.received.lock().unwrap().clone()
    }
}

impl Drop for FakeChromecast {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve(mut s: Box<dyn crate::session::ReadWrite>, rc: Arc<Mutex<CastReceived>>, cfg: FakeCastConfig, stop: Arc<AtomicBool>, conn_no: usize) {
    let mut rd = FrameReader::default();
    let (tx, rx) = std::sync::mpsc::channel::<String>(); // status messages from the player thread
    let player_stop = Arc::new(AtomicBool::new(false));
    let launched_at = Instant::now();
    let mut transport: Option<String> = None;
    let send = |s: &mut Box<dyn crate::session::ReadWrite>, src: &str, ns: &str, v: Value| {
        let _ = castv2::write_frame(s, &CastMessage { source: src.into(), destination: "sender-0".into(), namespace: ns.into(), payload: v.to_string() });
    };
    while !stop.load(Ordering::Relaxed) {
        if conn_no == 1 {
            if let Some(d) = cfg.drop_connection_after {
                if transport.is_some() && launched_at.elapsed() > d {
                    player_stop.store(true, Ordering::SeqCst);
                    return; // connection drops
                }
            }
        }
        while let Ok(state) = rx.try_recv() {
            let t = transport.clone().unwrap_or_default();
            if state == "ERROR" {
                send(&mut s, &t, castv2::NS_MEDIA, json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 1, "playerState": "IDLE", "idleReason": "ERROR"}]}));
            } else {
                send(&mut s, &t, castv2::NS_MEDIA, json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 1, "playerState": state}]}));
            }
        }
        let m = match rd.poll(&mut s) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(_) => break,
        };
        rc.lock().unwrap().messages.push(format!("{}|{}", m.namespace.rsplit('.').next().unwrap_or(""), m.payload));
        let v: Value = serde_json::from_str(&m.payload).unwrap_or(Value::Null);
        let ty = v["type"].as_str().unwrap_or("");
        match (m.namespace.as_str(), ty) {
            (castv2::NS_HEARTBEAT, "PING") => send(&mut s, "receiver-0", castv2::NS_HEARTBEAT, json!({"type": "PONG"})),
            (castv2::NS_RECEIVER, "LAUNCH") if v["appId"] == DEFAULT_MEDIA_RECEIVER => {
                transport = Some("web-1".into());
                send(&mut s, "receiver-0", castv2::NS_RECEIVER, json!({"type": "RECEIVER_STATUS", "requestId": v["requestId"], "status": {"applications": [{"appId": DEFAULT_MEDIA_RECEIVER, "displayName": "Default Media Receiver", "transportId": "web-1", "sessionId": "session-1"}]}}));
            }
            (castv2::NS_RECEIVER, "STOP") => {
                player_stop.store(true, Ordering::SeqCst);
                send(&mut s, "receiver-0", castv2::NS_RECEIVER, json!({"type": "RECEIVER_STATUS", "status": {"applications": []}}));
            }
            (castv2::NS_MEDIA, "LOAD") => {
                let url = v["media"]["contentId"].as_str().unwrap_or("").to_string();
                rc.lock().unwrap().load_url = Some(url.clone());
                if cfg.fail_load {
                    send(&mut s, "web-1", castv2::NS_MEDIA, json!({"type": "LOAD_FAILED", "requestId": v["requestId"]}));
                    continue;
                }
                send(&mut s, "web-1", castv2::NS_MEDIA, json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 1, "playerState": "BUFFERING"}]}));
                let (rc2, tx2, ps, err_after) = (rc.clone(), tx.clone(), player_stop.clone(), cfg.error_after);
                std::thread::spawn(move || hls_player(url, rc2, tx2, ps, err_after));
            }
            _ => {}
        }
    }
    player_stop.store(true, Ordering::SeqCst);
}

/// Behaves like the Default Media Receiver: fetch the playlist, then keep fetching new segments.
fn hls_player(url: String, rc: Arc<Mutex<CastReceived>>, tx: std::sync::mpsc::Sender<String>, stop: Arc<AtomicBool>, error_after: Option<Duration>) {
    let base = url.rsplit_once('/').map(|(b, _)| b.to_string()).unwrap_or_default();
    let mut fetched: Vec<String> = Vec::new();
    let mut playing = false;
    let t0 = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if let (true, Some(d)) = (playing, error_after) {
            if t0.elapsed() > d {
                let _ = tx.send("ERROR".into());
                return;
            }
        }
        let Ok(r) = http_client::request("GET", &url, &[("Origin", "https://www.gstatic.com".into())], &[], Duration::from_secs(3)) else {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        };
        if r.status != 200 {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        if r.header("Access-Control-Allow-Origin").is_none() {
            return; // a real receiver could not read the playlist without CORS
        }
        rc.lock().unwrap().playlist_fetches += 1;
        for line in r.text().lines().filter(|l| l.ends_with(".ts")) {
            if fetched.iter().any(|f| f == line) {
                continue;
            }
            if let Ok(seg) = http_client::get(&format!("{base}/{line}"), Duration::from_secs(5)) {
                if seg.status == 200 {
                    let sum = ts_summary(&seg.body);
                    let mut g = rc.lock().unwrap();
                    g.segment_sync_errors += sum.sync_errors;
                    if sum.sync_errors == 0 && sum.video_pes >= 10 && sum.starts_with_pat {
                        g.segments_ok += 1;
                    }
                    fetched.push(line.to_string());
                    if !playing && g.segments_ok >= 2 {
                        playing = true;
                        let _ = tx.send("PLAYING".into());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Convenience: a renderer description for `discover`/`ssdp` tests.
pub fn renderer_for(tv: &FakeDlnaTv, name: &str) -> ssdp::Renderer {
    ssdp::Renderer {
        name: name.into(),
        manufacturer: Some("FakeCo".into()),
        model: Some("FK-1".into()),
        ip: tv.http_addr.ip(),
        control_url: tv.control_url(),
        service_type: AVT.into(),
        udn: format!("uuid:fake-{}", tv.http_addr.port()),
    }
}
