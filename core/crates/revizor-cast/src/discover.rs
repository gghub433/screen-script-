//! One list of TVs from every discovery protocol, merged by IP address.

use crate::mdns::{self, CastDevice};
use crate::ssdp::{self, Renderer};
use std::net::IpAddr;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// Google Cast: Chromecast, Chromecast with Google TV, Android TV / Google TV sets.
    Cast { port: u16 },
    /// DLNA / UPnP MediaRenderer: Samsung, LG, Sony, Philips, Hisense, … smart TVs.
    Dlna { control_url: String, service_type: String },
}

impl Method {
    pub fn label(&self) -> &'static str {
        match self {
            Method::Cast { .. } => "Google Cast",
            Method::Dlna { .. } => "DLNA",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tv {
    pub name: String,
    pub ip: IpAddr,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    /// In the order they are tried: Cast first (robust HLS), then DLNA.
    pub methods: Vec<Method>,
}

pub fn merge(renderers: Vec<Renderer>, cast: Vec<CastDevice>) -> Vec<Tv> {
    let mut tvs: Vec<Tv> = Vec::new();
    for c in cast {
        match tvs.iter_mut().find(|t| t.ip == c.ip) {
            Some(t) => t.methods.push(Method::Cast { port: c.port }),
            None => tvs.push(Tv { name: c.name, ip: c.ip, model: c.model, manufacturer: None, methods: vec![Method::Cast { port: c.port }] }),
        }
    }
    for r in renderers {
        let m = Method::Dlna { control_url: r.control_url, service_type: r.service_type };
        match tvs.iter_mut().find(|t| t.ip == r.ip) {
            Some(t) => {
                t.manufacturer = t.manufacturer.take().or(r.manufacturer);
                if t.model.is_none() {
                    t.model = r.model;
                }
                t.methods.push(m);
            }
            None => tvs.push(Tv { name: r.name, ip: r.ip, model: r.model, manufacturer: r.manufacturer, methods: vec![m] }),
        }
    }
    for t in &mut tvs {
        t.methods.sort_by_key(|m| matches!(m, Method::Dlna { .. }));
    }
    tvs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    tvs
}

/// Looks for TVs for about `duration` using SSDP (DLNA) and mDNS (Google Cast) in parallel on every local network.
pub fn scan_tvs(duration: Duration) -> Vec<Tv> {
    let ips = ssdp::local_ipv4s();
    let ips2 = ips.clone();
    let cast = std::thread::spawn(move || mdns::find_cast_devices(duration, &[mdns::MDNS_ADDR], &ips2));
    let dlna = std::thread::spawn(move || {
        let replies = ssdp::search(duration, &[ssdp::SSDP_ADDR], &ips);
        ssdp::describe_all(replies)
    });
    merge(dlna.join().unwrap_or_default(), cast.join().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rend(name: &str, ip: &str, udn: &str) -> Renderer {
        Renderer { name: name.into(), manufacturer: Some("Sony".into()), model: Some("KD".into()), ip: ip.parse().unwrap(), control_url: format!("http://{ip}:80/ctl"), service_type: "urn:schemas-upnp-org:service:AVTransport:1".into(), udn: udn.into() }
    }
    fn cast(name: &str, ip: &str) -> CastDevice {
        CastDevice { name: name.into(), model: Some("Chromecast".into()), id: "id".into(), ip: ip.parse().unwrap(), port: 8009 }
    }

    #[test]
    fn same_tv_found_by_both_protocols_is_one_entry_with_cast_first() {
        let tvs = merge(vec![rend("Bravia", "192.168.1.9", "u1"), rend("Samsung", "192.168.1.20", "u2")], vec![cast("Living room", "192.168.1.9")]);
        assert_eq!(tvs.len(), 2);
        let living = tvs.iter().find(|t| t.ip.to_string() == "192.168.1.9").unwrap();
        assert_eq!(living.name, "Living room");
        assert_eq!(living.manufacturer.as_deref(), Some("Sony"));
        assert!(matches!(living.methods[0], Method::Cast { port: 8009 }) && matches!(living.methods[1], Method::Dlna { .. }));
        let sam = tvs.iter().find(|t| t.name == "Samsung").unwrap();
        assert_eq!(sam.methods.len(), 1);
    }

    #[test]
    fn empty_in_empty_out() {
        assert!(merge(vec![], vec![]).is_empty());
    }
}
