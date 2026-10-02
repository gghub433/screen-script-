//! SSDP discovery of UPnP/DLNA `MediaRenderer` devices (smart TVs, AV receivers, game consoles).

use crate::http_client::{self, Url};
use crate::xml;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

pub const SSDP_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900));
const SEARCH_TARGETS: [&str; 2] = ["urn:schemas-upnp-org:device:MediaRenderer:1", "urn:schemas-upnp-org:service:AVTransport:1"];
const AV_TRANSPORT: &str = "AVTransport";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsdpReply {
    pub location: String,
    pub usn: String,
    pub server: String,
    pub from: IpAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renderer {
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub ip: IpAddr,
    /// Absolute URL of the AVTransport SOAP endpoint.
    pub control_url: String,
    /// Full service type URN as advertised (e.g. `urn:schemas-upnp-org:service:AVTransport:1`).
    pub service_type: String,
    pub udn: String,
}

pub fn m_search(st: &str) -> String {
    format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\nUSER-AGENT: Revizor/1.0 UPnP/1.0\r\n\r\n")
}

pub fn parse_reply(data: &[u8], from: IpAddr) -> Option<SsdpReply> {
    let text = String::from_utf8_lossy(data);
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    if !(first.starts_with("HTTP/1.1 200") || first.starts_with("NOTIFY")) {
        return None;
    }
    let (mut location, mut usn, mut server) = (None, String::new(), String::new());
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "location" => location = Some(v.trim().to_string()),
                "usn" => usn = v.trim().to_string(),
                "server" => server = v.trim().to_string(),
                _ => {}
            }
        }
    }
    Some(SsdpReply { location: location?, usn, server, from })
}

/// Sends M-SEARCH from every given local IPv4 address to every target address and collects replies for `duration`.
pub fn search(duration: Duration, targets: &[SocketAddr], local_ips: &[Ipv4Addr]) -> Vec<SsdpReply> {
    let mut socks: Vec<Socket> = Vec::new();
    for ip in local_ips {
        let Ok(s) = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) else { continue };
        if s.bind(&SockAddr::from(SocketAddr::new(IpAddr::V4(*ip), 0))).is_err() {
            continue;
        }
        let _ = s.set_multicast_if_v4(ip);
        let _ = s.set_multicast_ttl_v4(2);
        let _ = s.set_nonblocking(true);
        socks.push(s);
    }
    let end = Instant::now() + duration;
    let mut last_send: Option<Instant> = None;
    let mut sends = 0;
    let mut out: Vec<SsdpReply> = Vec::new();
    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 2048];
    while Instant::now() < end {
        if sends < 3 && last_send.map_or(true, |t| t.elapsed() >= Duration::from_millis(500)) {
            for s in &socks {
                for st in SEARCH_TARGETS {
                    for t in targets {
                        let _ = s.send_to(m_search(st).as_bytes(), &SockAddr::from(*t));
                    }
                }
            }
            last_send = Some(Instant::now());
            sends += 1;
        }
        let mut got = false;
        for s in &socks {
            while let Ok((n, from)) = s.recv_from(&mut buf) {
                got = true;
                // SAFETY: recv_from initialised the first n bytes.
                let data: Vec<u8> = buf[..n].iter().map(|b| unsafe { b.assume_init() }).collect();
                if let Some(r) = from.as_socket().and_then(|a| parse_reply(&data, a.ip())) {
                    if !out.iter().any(|o| o.location == r.location) {
                        out.push(r);
                    }
                }
            }
        }
        if !got {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    out
}

/// Downloads and interprets a device description; `None` when it is not a controllable media renderer.
pub fn describe(reply: &SsdpReply) -> Option<Renderer> {
    let loc = Url::parse(&reply.location)?;
    let resp = http_client::get(&reply.location, Duration::from_secs(3)).ok()?;
    if resp.status != 200 {
        return None;
    }
    parse_description(&resp.text(), &loc, reply.from)
}

pub fn parse_description(xml_text: &str, location: &Url, ip: IpAddr) -> Option<Renderer> {
    let base = xml::first_text(xml_text, "URLBase").and_then(|b| Url::parse(b.trim_end_matches('/'))).unwrap_or_else(|| location.clone());
    for svc in xml::blocks(xml_text, "service") {
        let stype = xml::first_text(svc, "serviceType")?;
        if stype.contains(AV_TRANSPORT) {
            let control = xml::first_text(svc, "controlURL")?;
            return Some(Renderer {
                name: xml::first_text(xml_text, "friendlyName").unwrap_or_else(|| "TV".into()),
                manufacturer: xml::first_text(xml_text, "manufacturer"),
                model: xml::first_text(xml_text, "modelName"),
                ip,
                control_url: base.join(&control),
                service_type: stype,
                udn: xml::first_text(xml_text, "UDN").unwrap_or_default(),
            });
        }
    }
    None
}

/// Finds DLNA renderers on every local network, resolves their descriptions, de-duplicates by UDN.
pub fn find_renderers(duration: Duration) -> Vec<Renderer> {
    let ips = local_ipv4s();
    let replies = search(duration, &[SSDP_ADDR], &ips);
    describe_all(replies)
}

pub fn describe_all(replies: Vec<SsdpReply>) -> Vec<Renderer> {
    let handles: Vec<_> = replies.into_iter().map(|r| std::thread::spawn(move || describe(&r))).collect();
    let mut out: Vec<Renderer> = Vec::new();
    for h in handles {
        if let Ok(Some(r)) = h.join() {
            if !out.iter().any(|o| (!o.udn.is_empty() && o.udn == r.udn) || o.control_url == r.control_url) {
                out.push(r);
            }
        }
    }
    out
}

pub fn local_ipv4s() -> Vec<Ipv4Addr> {
    if_addrs::get_if_addrs()
        .map(|v| {
            v.into_iter()
                .filter(|i| !i.is_loopback())
                .filter_map(|i| match i.addr {
                    if_addrs::IfAddr::V4(a) => Some(a.ip),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESC: &str = r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major><minor>0</minor></specVersion>
<device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType><friendlyName>[TV] Samsung 7 Series</friendlyName>
<manufacturer>Samsung Electronics</manufacturer><modelName>UE55</modelName><UDN>uuid:abcd-1234</UDN>
<serviceList><service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType><controlURL>/upnp/control/RenderingControl1</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>/upnp/control/AVTransport1</controlURL></service></serviceList></device></root>"#;

    #[test]
    fn description_is_parsed() {
        let loc = Url::parse("http://192.168.1.50:9197/dmr").unwrap();
        let r = parse_description(DESC, &loc, "192.168.1.50".parse().unwrap()).unwrap();
        assert_eq!(r.name, "[TV] Samsung 7 Series");
        assert_eq!(r.manufacturer.as_deref(), Some("Samsung Electronics"));
        assert_eq!(r.control_url, "http://192.168.1.50:9197/upnp/control/AVTransport1");
        assert_eq!(r.service_type, "urn:schemas-upnp-org:service:AVTransport:1");
        assert_eq!(r.udn, "uuid:abcd-1234");
    }

    #[test]
    fn urlbase_wins_and_non_renderers_are_ignored() {
        let loc = Url::parse("http://10.0.0.2:1/desc.xml").unwrap();
        let with_base = DESC.replace("<device>", "<URLBase>http://10.0.0.9:7000/</URLBase><device>");
        assert_eq!(parse_description(&with_base, &loc, "10.0.0.9".parse().unwrap()).unwrap().control_url, "http://10.0.0.9:7000/upnp/control/AVTransport1");
        let router = "<root><device><friendlyName>Router</friendlyName><serviceList><service><serviceType>urn:a:WANIPConnection:1</serviceType><controlURL>/x</controlURL></service></serviceList></device></root>";
        assert!(parse_description(router, &loc, "10.0.0.1".parse().unwrap()).is_none());
    }

    #[test]
    fn replies_are_parsed_case_insensitively() {
        let pkt = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLocation: http://192.168.1.50:9197/dmr\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: uuid:abcd::urn:schemas-upnp-org:device:MediaRenderer:1\r\nSERVER: Linux UPnP/1.0\r\n\r\n";
        let r = parse_reply(pkt, "192.168.1.50".parse().unwrap()).unwrap();
        assert_eq!(r.location, "http://192.168.1.50:9197/dmr");
        assert!(parse_reply(b"M-SEARCH * HTTP/1.1\r\n\r\n", "1.1.1.1".parse().unwrap()).is_none());
        assert!(parse_reply(b"HTTP/1.1 200 OK\r\nUSN: x\r\n\r\n", "1.1.1.1".parse().unwrap()).is_none());
    }
}
