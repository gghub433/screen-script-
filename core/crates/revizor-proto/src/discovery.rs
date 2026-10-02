//! LAN discovery messages. Announcements are *hints*: they are unauthenticated,
//! so a discovered device is never trusted until the handshake proves its
//! identity against the trust store.

use crate::wire::{Reader, Writer};
use crate::{Codec, ProtoError, PROTOCOL_VERSION};

pub const DISCOVERY_PORT: u16 = 47720;
pub const DISCOVERY_MAGIC: &[u8; 4] = b"RVZD";
/// Default media port a receiver listens on.
pub const DEFAULT_MEDIA_PORT: u16 = 47721;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub proto_version: u16,
    /// `hex(sha256(pubkey)[..16])` — see revizor-crypto `device_id`.
    pub device_id: String,
    pub name: String,
    pub media_port: u16,
    /// Bitmask of `TransportKind::bit()`.
    pub transports: u8,
    pub codecs: Vec<Codec>,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    /// Receiver currently shows a PIN and accepts new pairings.
    pub pairing_open: bool,
}

/// Discovery datagrams: `RVZD` + kind (1 = probe, 2 = announce) + body.
pub enum DiscoveryMsg {
    Probe,
    Announce(Announcement),
}

impl DiscoveryMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.raw(DISCOVERY_MAGIC);
        match self {
            DiscoveryMsg::Probe => {
                w.u8(1).u16(PROTOCOL_VERSION);
            }
            DiscoveryMsg::Announce(a) => {
                w.u8(2).u16(a.proto_version).str(&a.device_id).str(&a.name).u16(a.media_port).u8(a.transports);
                w.u8(a.codecs.len() as u8);
                for c in &a.codecs {
                    w.u8(*c as u8);
                }
                w.u16(a.max_width).u16(a.max_height).u16(a.max_fps).u8(a.pairing_open as u8);
            }
        }
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ProtoError> {
        let mut r = Reader::new(b);
        if r.bytes(4)? != DISCOVERY_MAGIC {
            return Err(ProtoError::BadMagic);
        }
        match r.u8()? {
            1 => {
                let _v = r.u16()?;
                Ok(DiscoveryMsg::Probe)
            }
            2 => {
                let proto_version = r.u16()?;
                let device_id = r.str()?;
                let name = r.str()?;
                let media_port = r.u16()?;
                let transports = r.u8()?;
                let n = r.u8()? as usize;
                let mut codecs = Vec::new();
                for _ in 0..n {
                    // Unknown codecs from a newer peer are skipped, not fatal.
                    if let Ok(c) = Codec::from_u8(r.u8()?) {
                        codecs.push(c);
                    }
                }
                Ok(DiscoveryMsg::Announce(Announcement {
                    proto_version,
                    device_id,
                    name,
                    media_port,
                    transports,
                    codecs,
                    max_width: r.u16()?,
                    max_height: r.u16()?,
                    max_fps: r.u16()?,
                    pairing_open: r.u8()? != 0,
                }))
            }
            _ => Err(ProtoError::Invalid("discovery kind")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_roundtrip() {
        let a = Announcement {
            proto_version: 1,
            device_id: "abcd".into(),
            name: "Living room".into(),
            media_port: DEFAULT_MEDIA_PORT,
            transports: 3,
            codecs: vec![Codec::H264, Codec::H265],
            max_width: 3840,
            max_height: 2160,
            max_fps: 60,
            pairing_open: true,
        };
        match DiscoveryMsg::decode(&DiscoveryMsg::Announce(a.clone()).encode()).unwrap() {
            DiscoveryMsg::Announce(b) => assert_eq!(a, b),
            _ => panic!(),
        }
        assert!(matches!(DiscoveryMsg::decode(&DiscoveryMsg::Probe.encode()).unwrap(), DiscoveryMsg::Probe));
        assert!(DiscoveryMsg::decode(b"XXXX\x01\x01\x00").is_err());
    }
}
