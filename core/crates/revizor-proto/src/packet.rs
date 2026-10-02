//! Outer packet envelope.
//!
//! ```text
//! 0      1     2     3      4            8                 16
//! +------+-----+-----+------+------------+-----------------+----------------
//! | 0x52 | ver | knd | flg  | session_id |    packet_seq   | body ...
//! +------+-----+-----+------+------------+-----------------+----------------
//! ```
//! For `Data` packets the body is an AEAD ciphertext (ChaCha20-Poly1305) with the
//! 16 header bytes as associated data and nonce = salt(4) || packet_seq(8).
//! For `Handshake`/`Pairing` packets the body is plaintext (authenticated by the
//! handshake itself).

use crate::{ProtoError, PROTOCOL_VERSION};

pub const MAGIC: u8 = 0x52; // 'R'
pub const HEADER_LEN: usize = 16;
pub const AEAD_TAG_LEN: usize = 16;
/// Largest datagram we ever emit (fits in a 1500 MTU path with IPv4/UDP headers).
pub const MAX_DATAGRAM: usize = 1400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketKind {
    Handshake = 1,
    Pairing = 2,
    Data = 3,
}

impl PacketKind {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            1 => Ok(Self::Handshake),
            2 => Ok(Self::Pairing),
            3 => Ok(Self::Data),
            _ => Err(ProtoError::Invalid("packet kind")),
        }
    }
}

/// Channel byte: first byte of the *decrypted* body of a `Data` packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Channel {
    Control = 1,
    Video = 2,
    Audio = 3,
    VideoFec = 4,
}

impl Channel {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            1 => Ok(Self::Control),
            2 => Ok(Self::Video),
            3 => Ok(Self::Audio),
            4 => Ok(Self::VideoFec),
            _ => Err(ProtoError::Invalid("channel")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub version: u8,
    pub kind: PacketKind,
    pub flags: u8,
    pub session_id: u32,
    pub seq: u64,
}

impl PacketHeader {
    pub fn new(kind: PacketKind, session_id: u32, seq: u64) -> Self {
        Self { version: PROTOCOL_VERSION as u8, kind, flags: 0, session_id, seq }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0] = MAGIC;
        h[1] = self.version;
        h[2] = self.kind as u8;
        h[3] = self.flags;
        h[4..8].copy_from_slice(&self.session_id.to_le_bytes());
        h[8..16].copy_from_slice(&self.seq.to_le_bytes());
        h
    }

    /// Parses the header and returns it together with the remaining body.
    pub fn decode(buf: &[u8]) -> Result<(Self, &[u8]), ProtoError> {
        if buf.len() < HEADER_LEN {
            return Err(ProtoError::Truncated);
        }
        if buf[0] != MAGIC {
            return Err(ProtoError::BadMagic);
        }
        let h = Self {
            version: buf[1],
            kind: PacketKind::from_u8(buf[2])?,
            flags: buf[3],
            session_id: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            seq: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        };
        Ok((h, &buf[HEADER_LEN..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = PacketHeader::new(PacketKind::Data, 0xdead_beef, 0x0102_0304_0506);
        let mut v = h.encode().to_vec();
        v.extend_from_slice(b"body");
        let (d, body) = PacketHeader::decode(&v).unwrap();
        assert_eq!(d, h);
        assert_eq!(body, b"body");
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(PacketHeader::decode(&[0u8; 4]), Err(ProtoError::Truncated));
        assert_eq!(PacketHeader::decode(&[0u8; 16]).unwrap_err(), ProtoError::BadMagic);
    }
}
