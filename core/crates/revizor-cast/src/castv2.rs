//! Google Cast v2 sender protocol (the subset needed to start a stream on the Default Media Receiver).
//!
//! Framing: `u32 big-endian length ‖ protobuf CastMessage`. The protobuf is tiny and hand-coded here, the payloads are JSON.
//! Flow: TLS connect :8009 → `CONNECT` → `LAUNCH CC1AD845` → wait `RECEIVER_STATUS` (transportId) → `CONNECT` to the
//! app → `LOAD` {HLS url, LIVE} → watch `MEDIA_STATUS`; keep `PING`ing; `STOP` on exit.
//!
//! The device certificate chain cannot be validated against a public CA (it is issued by Google's device CA and
//! tied to the unit), so — like every third-party Cast sender — the TLS layer accepts any certificate. The only
//! information sent is the stream URL; see SECURITY notes in the docs.

use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";
const SENDER: &str = "sender-0";
const RECEIVER: &str = "receiver-0";
const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastMessage {
    pub source: String,
    pub destination: String,
    pub namespace: String,
    pub payload: String,
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_str(out: &mut Vec<u8>, field: u8, s: &str) {
    out.push(field << 3 | 2);
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

pub fn encode_message(m: &CastMessage) -> Vec<u8> {
    let mut b = Vec::with_capacity(64 + m.payload.len());
    b.extend_from_slice(&[0x08, 0x00]); // protocol_version = CASTV2_1_0
    put_str(&mut b, 2, &m.source);
    put_str(&mut b, 3, &m.destination);
    put_str(&mut b, 4, &m.namespace);
    b.extend_from_slice(&[0x28, 0x00]); // payload_type = STRING
    put_str(&mut b, 6, &m.payload);
    b
}

fn get_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let (mut v, mut shift) = (0u64, 0);
    loop {
        let x = *b.get(*i)?;
        *i += 1;
        v |= ((x & 0x7F) as u64) << shift;
        if x & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

pub fn decode_message(b: &[u8]) -> Option<CastMessage> {
    let (mut i, mut m) = (0usize, CastMessage { source: String::new(), destination: String::new(), namespace: String::new(), payload: String::new() });
    while i < b.len() {
        let key = get_varint(b, &mut i)?;
        let (field, wire) = (key >> 3, key & 7);
        match wire {
            0 => {
                get_varint(b, &mut i)?;
            }
            2 => {
                let len = get_varint(b, &mut i)? as usize;
                let data = b.get(i..i.checked_add(len)?)?;
                i += len;
                let s = String::from_utf8_lossy(data).to_string();
                match field {
                    2 => m.source = s,
                    3 => m.destination = s,
                    4 => m.namespace = s,
                    6 => m.payload = s,
                    _ => {} // payload_binary etc.
                }
            }
            _ => return None,
        }
    }
    Some(m)
}

pub fn write_frame<W: Write>(w: &mut W, m: &CastMessage) -> io::Result<()> {
    let body = encode_message(m);
    let mut f = Vec::with_capacity(4 + body.len());
    f.extend_from_slice(&(body.len() as u32).to_be_bytes());
    f.extend_from_slice(&body);
    write_all_retry(w, &f)
}

/// `write_all` that tolerates socket timeouts in the middle of a TLS handshake or a slow flush.
fn write_all_retry<W: Write>(w: &mut W, mut b: &[u8]) -> io::Result<()> {
    let start = Instant::now();
    while !b.is_empty() {
        match w.write(b) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => b = &b[n..],
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) && start.elapsed() < Duration::from_secs(8) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
    loop {
        match w.flush() {
            Ok(()) => return Ok(()),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) && start.elapsed() < Duration::from_secs(8) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(e),
        }
    }
}

/// Accumulates bytes across read timeouts so a frame is never lost when a timeout hits mid-frame.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    fn take_frame(&mut self) -> io::Result<Option<CastMessage>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "cast frame too large"));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg = decode_message(&self.buf[4..4 + len]);
        self.buf.drain(..4 + len);
        Ok(Some(msg.unwrap_or(CastMessage { source: String::new(), destination: String::new(), namespace: String::new(), payload: String::new() })))
    }

    /// One non-blocking-ish step: returns a complete message if available, reading at most once.
    pub fn poll<R: Read>(&mut self, r: &mut R) -> io::Result<Option<CastMessage>> {
        if let Some(m) = self.take_frame()? {
            return Ok(Some(m));
        }
        let mut tmp = [0u8; 8192];
        match r.read(&mut tmp) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => return Ok(None),
            Err(e) => return Err(e),
        }
        self.take_frame()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The Default Media Receiver is running and can be addressed.
    AppReady { transport_id: String, session_id: String },
    AppStopped,
    Media { state: String, idle_reason: Option<String> },
    LoadFailed(String),
    Error(String),
    Closed,
}

pub struct CastClient<S: Read + Write> {
    s: S,
    rd: FrameReader,
    request_id: u32,
    last_ping: Instant,
    pub last_rx: Instant,
    pub transport_id: Option<String>,
    pub session_id: Option<String>,
    app_seen: bool,
}

impl<S: Read + Write> CastClient<S> {
    pub fn new(s: S) -> Self {
        Self { s, rd: FrameReader::default(), request_id: 0, last_ping: Instant::now(), last_rx: Instant::now(), transport_id: None, session_id: None, app_seen: false }
    }

    fn next_id(&mut self) -> u32 {
        self.request_id += 1;
        self.request_id
    }

    fn send(&mut self, dest: &str, ns: &str, v: Value) -> io::Result<()> {
        write_frame(&mut self.s, &CastMessage { source: SENDER.into(), destination: dest.into(), namespace: ns.into(), payload: v.to_string() })
    }

    /// Virtual connection to the platform receiver.
    pub fn open(&mut self) -> io::Result<()> {
        self.send(RECEIVER, NS_CONNECTION, json!({"type": "CONNECT", "origin": {}, "userAgent": "Revizor", "senderInfo": {"sdkType": 2, "version": "1.0", "browserVersion": "", "platform": 0, "connectionType": 1}}))
    }

    pub fn launch(&mut self, app_id: &str) -> io::Result<()> {
        let id = self.next_id();
        self.send(RECEIVER, NS_RECEIVER, json!({"type": "LAUNCH", "appId": app_id, "requestId": id}))
    }

    pub fn get_status(&mut self) -> io::Result<()> {
        let id = self.next_id();
        self.send(RECEIVER, NS_RECEIVER, json!({"type": "GET_STATUS", "requestId": id}))
    }

    /// Connect to the launched app and ask it to play a live HLS stream.
    pub fn load_live(&mut self, url: &str, content_type: &str) -> io::Result<()> {
        let (Some(t), Some(sid)) = (self.transport_id.clone(), self.session_id.clone()) else {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "app not launched"));
        };
        self.send(&t, NS_CONNECTION, json!({"type": "CONNECT", "origin": {}}))?;
        let id = self.next_id();
        self.send(
            &t,
            NS_MEDIA,
            json!({
                "type": "LOAD", "requestId": id, "sessionId": sid, "autoplay": true, "currentTime": 0,
                "media": {
                    "contentId": url, "contentType": content_type, "streamType": "LIVE",
                    "hlsSegmentFormat": "ts", "hlsVideoSegmentFormat": "mpeg2_ts",
                    "metadata": {"metadataType": 0, "title": "Revizor"}
                }
            }),
        )
    }

    pub fn stop_app(&mut self) -> io::Result<()> {
        if let Some(sid) = self.session_id.clone() {
            let id = self.next_id();
            self.send(RECEIVER, NS_RECEIVER, json!({"type": "STOP", "sessionId": sid, "requestId": id}))?;
        }
        Ok(())
    }

    pub fn silent_for(&self) -> Duration {
        self.last_rx.elapsed()
    }

    /// Reads whatever has arrived (at most the socket's read timeout), answers heartbeats, returns events.
    pub fn pump(&mut self) -> io::Result<Vec<Event>> {
        let mut events = Vec::new();
        for _ in 0..64 {
            let Some(m) = self.rd.poll(&mut self.s)? else { break };
            self.last_rx = Instant::now();
            let v: Value = serde_json::from_str(&m.payload).unwrap_or(Value::Null);
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            match (m.namespace.as_str(), ty) {
                (NS_HEARTBEAT, "PING") => self.send(&m.source, NS_HEARTBEAT, json!({"type": "PONG"}))?,
                (NS_CONNECTION, "CLOSE") => events.push(Event::Closed),
                (NS_RECEIVER, "RECEIVER_STATUS") => {
                    let app = v["status"]["applications"].as_array().and_then(|a| a.iter().find(|x| x["appId"] == DEFAULT_MEDIA_RECEIVER));
                    match app {
                        Some(a) => {
                            let (t, s) = (a["transportId"].as_str().unwrap_or("").to_string(), a["sessionId"].as_str().unwrap_or("").to_string());
                            if !t.is_empty() {
                                self.transport_id = Some(t.clone());
                                self.session_id = Some(s.clone());
                                if !self.app_seen {
                                    self.app_seen = true;
                                    events.push(Event::AppReady { transport_id: t, session_id: s });
                                }
                            }
                        }
                        None if self.app_seen => {
                            self.app_seen = false;
                            self.transport_id = None;
                            self.session_id = None;
                            events.push(Event::AppStopped);
                        }
                        None => {}
                    }
                }
                (NS_RECEIVER, "LAUNCH_ERROR") => events.push(Event::Error(format!("the TV could not start its media player ({})", v["reason"].as_str().unwrap_or("unknown")))),
                (NS_MEDIA, "MEDIA_STATUS") => {
                    if let Some(s) = v["status"].as_array().and_then(|a| a.first()) {
                        events.push(Event::Media { state: s["playerState"].as_str().unwrap_or("").to_string(), idle_reason: s["idleReason"].as_str().map(str::to_string) });
                    }
                }
                (NS_MEDIA, "LOAD_FAILED") | (NS_MEDIA, "LOAD_CANCELLED") | (NS_MEDIA, "INVALID_REQUEST") => events.push(Event::LoadFailed(ty.to_string())),
                _ => {}
            }
        }
        if self.last_ping.elapsed() >= Duration::from_secs(5) {
            self.last_ping = Instant::now();
            self.send(RECEIVER, NS_HEARTBEAT, json!({"type": "PING"}))?;
        }
        Ok(events)
    }
}

#[cfg(feature = "tls")]
pub mod tls {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{ring, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
    use std::io;
    use std::net::{SocketAddr, TcpStream};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Debug)]
    struct AcceptAny(Arc<CryptoProvider>);

    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    pub type TlsStream = StreamOwned<ClientConnection, TcpStream>;

    pub fn client_config() -> Result<Arc<ClientConfig>, rustls::Error> {
        let provider = Arc::new(ring::default_provider());
        let cfg = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
            .with_no_client_auth();
        Ok(Arc::new(cfg))
    }

    /// TLS to a Cast device (:8009) with a short read timeout so the caller's loop stays responsive.
    pub fn connect(addr: SocketAddr, timeout: Duration) -> io::Result<TlsStream> {
        let tcp = TcpStream::connect_timeout(&addr, timeout)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(Duration::from_millis(250)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
        let cfg = client_config().map_err(io::Error::other)?;
        let conn = ClientConnection::new(cfg, ServerName::IpAddress(addr.ip().into())).map_err(io::Error::other)?;
        Ok(StreamOwned::new(conn, tcp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protobuf_roundtrip_and_known_bytes() {
        let m = CastMessage { source: "sender-0".into(), destination: "receiver-0".into(), namespace: NS_CONNECTION.into(), payload: "{\"type\":\"CONNECT\"}".into() };
        let b = encode_message(&m);
        assert_eq!(&b[..4], &[0x08, 0x00, 0x12, 0x08]); // version, then field 2 length 8
        assert_eq!(decode_message(&b).unwrap(), m);
        // long payloads need multi-byte varints
        let big = CastMessage { payload: "x".repeat(300), ..m.clone() };
        assert_eq!(decode_message(&encode_message(&big)).unwrap(), big);
    }

    #[test]
    fn decoder_rejects_garbage_without_panicking() {
        for n in 0..64u8 {
            let junk: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37) ^ 0xA5).collect();
            let _ = decode_message(&junk);
        }
        assert!(decode_message(&[0x12, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).is_none());
    }

    struct Chunked {
        data: Vec<u8>,
        pos: usize,
        step: usize,
        stall: bool,
    }
    impl Read for Chunked {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.stall = !self.stall;
            if self.stall {
                return Err(io::ErrorKind::TimedOut.into()); // a timeout between every chunk
            }
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let n = self.step.min(b.len()).min(self.data.len() - self.pos);
            b[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn frame_reader_survives_timeouts_in_the_middle_of_frames() {
        let mut wire = Vec::new();
        for i in 0..5 {
            write_frame(&mut wire, &CastMessage { source: "a".into(), destination: "b".into(), namespace: "n".into(), payload: format!("{{\"i\":{i}}}") }).unwrap();
        }
        let mut src = Chunked { data: wire, pos: 0, step: 3, stall: false };
        let mut rd = FrameReader::default();
        let mut got = vec![];
        for _ in 0..1000 {
            match rd.poll(&mut src) {
                Ok(Some(m)) => got.push(m.payload),
                Ok(None) => {}
                Err(e) => {
                    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
                    break;
                }
            }
        }
        assert_eq!(got, (0..5).map(|i| format!("{{\"i\":{i}}}")).collect::<Vec<_>>());
    }

    #[test]
    fn oversized_frame_is_an_error() {
        let mut rd = FrameReader { buf: vec![0xFF, 0xFF, 0xFF, 0xFF, 1, 2] };
        assert_eq!(rd.poll(&mut io::empty()).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
