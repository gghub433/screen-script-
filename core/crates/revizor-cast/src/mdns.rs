//! Minimal multicast-DNS service discovery for Google Cast (`_googlecast._tcp.local`): Chromecast,
//! Chromecast with Google TV, Android TV / Google TV sets, Nest/Google speakers & displays.
//!
//! One-shot "legacy unicast" queries (RFC 6762 §6.7) are sent from an ephemeral port on every local
//! IPv4 interface, so answers come straight back to us and no multicast group membership is needed.

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

pub const MDNS_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353));
pub const CAST_SERVICE: &str = "_googlecast._tcp.local";

pub const T_A: u16 = 1;
pub const T_PTR: u16 = 12;
pub const T_TXT: u16 = 16;
pub const T_SRV: u16 = 33;
pub const T_ANY: u16 = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastDevice {
    pub name: String,
    pub model: Option<String>,
    pub id: String,
    pub ip: IpAddr,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rdata {
    A(Ipv4Addr),
    Ptr(String),
    Srv { port: u16, target: String },
    Txt(Vec<(String, String)>),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub data: Rdata,
}

pub fn encode_name(name: &str, out: &mut Vec<u8>) {
    for label in name.trim_end_matches('.').split('.') {
        let b = label.as_bytes();
        out.push(b.len().min(63) as u8);
        out.extend_from_slice(&b[..b.len().min(63)]);
    }
    out.push(0);
}

pub fn query(name: &str, qtype: u16) -> Vec<u8> {
    let mut p = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    encode_name(name, &mut p);
    p.extend_from_slice(&qtype.to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes());
    p
}

fn read_name(p: &[u8], mut off: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *p.get(off)? as usize;
        if len == 0 {
            off += 1;
            break;
        }
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *p.get(off + 1)? as usize;
            if end.is_none() {
                end = Some(off + 2);
            }
            jumps += 1;
            if jumps > 16 || ptr >= p.len() {
                return None; // loop or bad pointer
            }
            off = ptr;
            continue;
        }
        let l = p.get(off + 1..off + 1 + len)?;
        labels.push(String::from_utf8_lossy(l).to_string());
        off += 1 + len;
    }
    Some((labels.join("."), end.unwrap_or(off)))
}

pub fn parse_records(p: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let Some(h) = p.get(..12) else { return out };
    let qd = u16::from_be_bytes([h[4], h[5]]) as usize;
    let rr_count = u16::from_be_bytes([h[6], h[7]]) as usize + u16::from_be_bytes([h[8], h[9]]) as usize + u16::from_be_bytes([h[10], h[11]]) as usize;
    let mut off = 12;
    for _ in 0..qd {
        let Some((_, n)) = read_name(p, off) else { return out };
        off = n + 4;
    }
    for _ in 0..rr_count {
        let Some((name, n)) = read_name(p, off) else { return out };
        let Some(fixed) = p.get(n..n + 10) else { return out };
        let rtype = u16::from_be_bytes([fixed[0], fixed[1]]);
        let rdlen = u16::from_be_bytes([fixed[8], fixed[9]]) as usize;
        let rd_start = n + 10;
        let Some(rd) = p.get(rd_start..rd_start + rdlen) else { return out };
        let data = match rtype {
            T_A if rdlen == 4 => Rdata::A(Ipv4Addr::new(rd[0], rd[1], rd[2], rd[3])),
            T_PTR => read_name(p, rd_start).map_or(Rdata::Other, |(t, _)| Rdata::Ptr(t)),
            T_SRV if rdlen >= 7 => read_name(p, rd_start + 6).map_or(Rdata::Other, |(t, _)| Rdata::Srv { port: u16::from_be_bytes([rd[4], rd[5]]), target: t }),
            T_TXT => {
                let mut kv = Vec::new();
                let mut i = 0;
                while i < rd.len() {
                    let l = rd[i] as usize;
                    if let Some(s) = rd.get(i + 1..i + 1 + l) {
                        let s = String::from_utf8_lossy(s);
                        if let Some((k, v)) = s.split_once('=') {
                            kv.push((k.to_string(), v.to_string()));
                        }
                    }
                    i += 1 + l;
                }
                Rdata::Txt(kv)
            }
            _ => Rdata::Other,
        };
        out.push(Record { name, data });
        off = rd_start + rdlen;
    }
    out
}

/// Accumulates records from one or more packets and turns complete Cast instances into devices.
#[derive(Default)]
pub struct Collector {
    instances: Vec<(String, IpAddr)>, // (instance full name, responder ip)
    srv: HashMap<String, (u16, String)>,
    txt: HashMap<String, Vec<(String, String)>>,
    a: HashMap<String, Ipv4Addr>,
}

impl Collector {
    pub fn add(&mut self, recs: Vec<Record>, from: IpAddr) {
        for r in recs {
            let lname = r.name.to_ascii_lowercase();
            match r.data {
                Rdata::Ptr(inst) if lname == CAST_SERVICE => {
                    if !self.instances.iter().any(|(n, _)| n == &inst) {
                        self.instances.push((inst, from));
                    }
                }
                Rdata::Srv { port, target } => {
                    self.srv.insert(r.name, (port, target));
                }
                Rdata::Txt(kv) => {
                    self.txt.insert(r.name, kv);
                }
                Rdata::A(ip) => {
                    self.a.insert(lname, ip);
                }
                _ => {}
            }
        }
    }

    /// Instances for which SRV/TXT/A are still missing: follow-up questions `(responder, name, type)`.
    pub fn followups(&self) -> Vec<(IpAddr, String, u16)> {
        let mut v = Vec::new();
        for (inst, from) in &self.instances {
            if !self.srv.contains_key(inst) || !self.txt.contains_key(inst) {
                v.push((*from, inst.clone(), T_ANY));
            } else if let Some((_, target)) = self.srv.get(inst) {
                if !self.a.contains_key(&target.to_ascii_lowercase()) {
                    v.push((*from, target.clone(), T_A));
                }
            }
        }
        v
    }

    pub fn devices(&self) -> Vec<CastDevice> {
        let mut out = Vec::new();
        for (inst, from) in &self.instances {
            let txt = self.txt.get(inst);
            let get = |k: &str| txt.and_then(|t| t.iter().find(|(a, _)| a == k)).map(|(_, v)| v.clone());
            let (port, ip) = match self.srv.get(inst) {
                Some((port, target)) => (*port, self.a.get(&target.to_ascii_lowercase()).map(|a| IpAddr::V4(*a)).unwrap_or(*from)),
                None => (8009, *from),
            };
            let label = inst.strip_suffix(&format!(".{CAST_SERVICE}")).unwrap_or(inst).to_string();
            // `ca` bit 4 (value & 4) = video out capable; audio-only speakers are not TVs. Absent = assume capable.
            if let Some(ca) = get("ca").and_then(|v| v.parse::<u32>().ok()) {
                if ca & 1 == 0 {
                    continue; // does not support video out
                }
            }
            out.push(CastDevice { name: get("fn").unwrap_or(label.clone()), model: get("md"), id: get("id").unwrap_or(label), ip, port });
        }
        out
    }
}

/// Queries `targets` (default: the mDNS multicast group) from each local address and collects Cast devices.
pub fn find_cast_devices(duration: Duration, targets: &[SocketAddr], local_ips: &[Ipv4Addr]) -> Vec<CastDevice> {
    let mut socks: Vec<Socket> = Vec::new();
    for ip in local_ips {
        let Ok(s) = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) else { continue };
        if s.bind(&SockAddr::from(SocketAddr::new(IpAddr::V4(*ip), 0))).is_err() {
            continue;
        }
        let _ = s.set_multicast_if_v4(ip);
        let _ = s.set_multicast_ttl_v4(255);
        let _ = s.set_nonblocking(true);
        socks.push(s);
    }
    let end = Instant::now() + duration;
    let mut col = Collector::default();
    let mut sent = 0;
    let mut last_send: Option<Instant> = None;
    let mut last_follow = Instant::now();
    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
    while Instant::now() < end {
        if sent < 3 && last_send.map_or(true, |t| t.elapsed() >= Duration::from_millis(600)) {
            for s in &socks {
                for t in targets {
                    let _ = s.send_to(&query(CAST_SERVICE, T_PTR), &SockAddr::from(*t));
                }
            }
            sent += 1;
            last_send = Some(Instant::now());
        }
        let mut got = false;
        for s in &socks {
            while let Ok((n, from)) = s.recv_from(&mut buf) {
                got = true;
                let data: Vec<u8> = buf[..n].iter().map(|b| unsafe { b.assume_init() }).collect();
                if let Some(a) = from.as_socket() {
                    col.add(parse_records(&data), a.ip());
                }
            }
        }
        if last_follow.elapsed() >= Duration::from_millis(400) {
            last_follow = Instant::now();
            for (resp, name, qt) in col.followups() {
                // direct unicast query to the responder's mDNS port
                for s in &socks {
                    let dst = SocketAddr::new(resp, if targets.iter().any(|t| t.ip() == resp) { targets[0].port() } else { 5353 });
                    let dst = targets.iter().find(|t| t.ip() == resp).copied().unwrap_or(dst);
                    let _ = s.send_to(&query(&name, qt), &SockAddr::from(dst));
                }
            }
        }
        if !got {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    col.devices()
}

#[cfg(test)]
pub(crate) mod build {
    //! Encoders for DNS responses used by tests and the fake Chromecast.
    use super::*;

    fn rr(out: &mut Vec<u8>, name: &str, t: u16, rdata: &[u8]) {
        encode_name(name, out);
        out.extend_from_slice(&t.to_be_bytes());
        out.extend_from_slice(&0x8001u16.to_be_bytes()); // cache-flush + IN
        out.extend_from_slice(&120u32.to_be_bytes());
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(rdata);
    }

    pub fn cast_response(instance: &str, host: &str, ip: Ipv4Addr, port: u16, txt: &[(&str, &str)], with_extras: bool) -> Vec<u8> {
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        let full = format!("{instance}.{CAST_SERVICE}");
        let mut ptr = Vec::new();
        encode_name(&full, &mut ptr);
        rr(&mut p, CAST_SERVICE, T_PTR, &ptr);
        if with_extras {
            p[10..12].copy_from_slice(&3u16.to_be_bytes());
            let mut srv = vec![0, 0, 0, 0];
            srv.extend_from_slice(&port.to_be_bytes());
            encode_name(host, &mut srv);
            rr(&mut p, &full, T_SRV, &srv);
            let mut t = Vec::new();
            for (k, v) in txt {
                let s = format!("{k}={v}");
                t.push(s.len() as u8);
                t.extend_from_slice(s.as_bytes());
            }
            rr(&mut p, &full, T_TXT, &t);
            // A record for the host goes in as a 4th additional
            p[10..12].copy_from_slice(&4u16.to_be_bytes());
            rr(&mut p, host, T_A, &ip.octets());
        }
        p
    }

    /// Response to a follow-up ANY/A question.
    pub fn followup_response(instance_full: &str, host: &str, ip: Ipv4Addr, port: u16, txt: &[(&str, &str)]) -> Vec<u8> {
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 3, 0, 0, 0, 0];
        let mut srv = vec![0, 0, 0, 0];
        srv.extend_from_slice(&port.to_be_bytes());
        encode_name(host, &mut srv);
        rr(&mut p, instance_full, T_SRV, &srv);
        let mut t = Vec::new();
        for (k, v) in txt {
            let s = format!("{k}={v}");
            t.push(s.len() as u8);
            t.extend_from_slice(s.as_bytes());
        }
        rr(&mut p, instance_full, T_TXT, &t);
        rr(&mut p, host, T_A, &ip.octets());
        p
    }
}

#[cfg(test)]
mod tests {
    use super::build::*;
    use super::*;
    use std::net::UdpSocket;

    #[test]
    fn query_packet_layout() {
        let q = query("_googlecast._tcp.local", T_PTR);
        assert_eq!(&q[..12], &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(q[12], 11); // "_googlecast"
        assert_eq!(&q[q.len() - 4..], &[0, 12, 0, 1]);
    }

    #[test]
    fn full_response_is_parsed_into_a_device() {
        let pkt = cast_response("Chromecast-abcdef", "abcdef.local", Ipv4Addr::new(192, 168, 1, 77), 8009, &[("id", "abcdef"), ("fn", "Living Room TV"), ("md", "Chromecast"), ("ca", "4101")], true);
        let mut c = Collector::default();
        c.add(parse_records(&pkt), "192.168.1.77".parse().unwrap());
        let d = c.devices();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "Living Room TV");
        assert_eq!(d[0].model.as_deref(), Some("Chromecast"));
        assert_eq!((d[0].ip, d[0].port), ("192.168.1.77".parse().unwrap(), 8009));
        assert!(c.followups().is_empty());
    }

    #[test]
    fn audio_only_speakers_are_not_listed_as_tvs() {
        let pkt = cast_response("Speaker", "spk.local", Ipv4Addr::new(10, 0, 0, 5), 8009, &[("fn", "Kitchen speaker"), ("ca", "4")], true);
        let mut c = Collector::default();
        c.add(parse_records(&pkt), "10.0.0.5".parse().unwrap());
        assert!(c.devices().is_empty());
    }

    #[test]
    fn name_compression_pointers_and_loops() {
        // pointer loop must not hang
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        p.extend_from_slice(&[0xC0, 12]); // name = pointer to itself
        p.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 10, 0, 0]);
        assert!(parse_records(&p).is_empty());
        // truncated packets never panic
        for n in 0..40 {
            let full = cast_response("X", "x.local", Ipv4Addr::LOCALHOST, 8009, &[("fn", "a")], true);
            let _ = parse_records(&full[..n.min(full.len())]);
        }
    }

    #[test]
    fn end_to_end_discovery_with_followups_over_udp() {
        // A responder that first answers only the PTR, then provides SRV/TXT/A when asked directly.
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let end = Instant::now() + Duration::from_secs(4);
            let (mut sent_ptr, mut sent_follow) = (0, 0);
            while Instant::now() < end {
                if let Ok((n, from)) = sock.recv_from(&mut buf) {
                    let q = &buf[..n];
                    let qtype = u16::from_be_bytes([q[n - 4], q[n - 3]]);
                    if qtype == T_PTR {
                        sock.send_to(&cast_response("Chromecast-1", "host1.local", Ipv4Addr::new(192, 168, 7, 7), 8009, &[], false), from).unwrap();
                        sent_ptr += 1;
                    } else if qtype == T_ANY {
                        sock.send_to(&followup_response(&format!("Chromecast-1.{CAST_SERVICE}"), "host1.local", Ipv4Addr::new(192, 168, 7, 7), 8009, &[("fn", "Bedroom"), ("md", "Chromecast Ultra"), ("id", "id1")]), from).unwrap();
                        sent_follow += 1;
                    }
                }
            }
            (sent_ptr, sent_follow)
        });
        let devs = find_cast_devices(Duration::from_millis(1800), &[addr], &[Ipv4Addr::LOCALHOST]);
        let (p, f) = t.join().unwrap();
        assert!(p >= 1 && f >= 1, "ptr={p} follow={f}");
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].name, "Bedroom");
        assert_eq!(devs[0].ip, "192.168.7.7".parse::<IpAddr>().unwrap());
    }
}
