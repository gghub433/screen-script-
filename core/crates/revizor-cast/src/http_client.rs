//! Minimal blocking HTTP/1.1 client (plain `http://` only) for UPnP device descriptions and SOAP control.
//! Bounded: response bodies are capped, every operation has a timeout.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(u: &str) -> Option<Url> {
        let rest = u.strip_prefix("http://")?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (h.trim_matches(|c| c == '[' || c == ']').to_string(), p.parse().ok()?),
            _ => (hostport.to_string(), 80),
        };
        Some(Url { host, port, path: path.to_string() })
    }

    pub fn origin(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Resolves an absolute URL, an absolute path or a relative path against `self`.
    pub fn join(&self, reference: &str) -> String {
        if reference.starts_with("http://") || reference.starts_with("https://") {
            reference.to_string()
        } else if reference.starts_with('/') {
            format!("{}{}", self.origin(), reference)
        } else {
            let dir = self.path.rsplit_once('/').map_or("", |(d, _)| d);
            format!("{}{}/{}", self.origin(), dir, reference)
        }
    }
}

pub fn request(method: &str, url: &str, extra_headers: &[(&str, String)], body: &[u8], timeout: Duration) -> std::io::Result<Response> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, m.to_string());
    let u = Url::parse(url).ok_or_else(|| bad("only http:// URLs are supported"))?;
    let addr: SocketAddr = (u.host.as_str(), u.port).to_socket_addrs()?.next().ok_or_else(|| bad("cannot resolve host"))?;
    let mut s = TcpStream::connect_timeout(&addr, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let mut req = format!("{method} {} HTTP/1.1\r\nHOST: {}:{}\r\nUSER-AGENT: Revizor/1.0 UPnP/1.0\r\nCONNECTION: close\r\n", u.path, u.host, u.port);
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        req.push_str(&format!("CONTENT-LENGTH: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    s.write_all(body)?;

    let mut raw = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&tmp[..n]);
                if raw.len() > 2 << 20 {
                    return Err(bad("response too large"));
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) && !raw.is_empty() => break,
            Err(e) => return Err(e),
        }
    }
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| bad("malformed HTTP response"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).ok_or_else(|| bad("bad status line"))?;
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let mut body = raw[split + 4..].to_vec();
    if headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")) {
        body = dechunk(&body);
    }
    Ok(Response { status, headers, body })
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(nl) = b.windows(2).position(|w| w == b"\r\n") else { break };
        let size = usize::from_str_radix(String::from_utf8_lossy(&b[..nl]).split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
        b = &b[nl + 2..];
        if size == 0 || b.len() < size {
            if size > 0 {
                out.extend_from_slice(b);
            }
            break;
        }
        out.extend_from_slice(&b[..size]);
        b = b.get(size + 2..).unwrap_or(&[]);
    }
    out
}

pub fn get(url: &str, timeout: Duration) -> std::io::Result<Response> {
    request("GET", url, &[], &[], timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parse_and_join() {
        let u = Url::parse("http://192.168.1.20:9197/dmr/desc.xml").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("192.168.1.20", 9197, "/dmr/desc.xml"));
        assert_eq!(u.join("/upnp/control/AVTransport1"), "http://192.168.1.20:9197/upnp/control/AVTransport1");
        assert_eq!(u.join("ctl"), "http://192.168.1.20:9197/dmr/ctl");
        assert_eq!(u.join("http://x:1/y"), "http://x:1/y");
        assert_eq!(Url::parse("http://tv/").unwrap().port, 80);
        assert!(Url::parse("https://tv/").is_none());
    }

    #[test]
    fn dechunk_works() {
        assert_eq!(dechunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"), b"hello world");
    }
}
