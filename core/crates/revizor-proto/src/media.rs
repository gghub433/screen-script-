//! Media payload headers (inside the encrypted `Data` body).
//!
//! Video frames are split into `pkt_count` data packets; every `fec_k` data
//! packets get one XOR parity packet on `Channel::VideoFec`, which lets the
//! receiver rebuild any single lost packet per group without a round trip.

use crate::wire::{Reader, Writer};
use crate::ProtoError;

pub const FLAG_KEYFRAME: u8 = 1;
/// Frame starts with codec config (SPS/PPS/VPS) in-band.
pub const FLAG_CONFIG: u8 = 2;
/// Retransmitted copy (receiver must not count it for jitter).
pub const FLAG_RETRANSMIT: u8 = 4;

pub const MEDIA_HEADER_LEN: usize = 20;
pub const FEC_HEADER_LEN: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub epoch: u16,
    pub frame_id: u32,
    pub pkt_idx: u16,
    pub pkt_count: u16,
    pub flags: u8,
    /// Capture timestamp on the sender's monotonic clock, microseconds.
    pub pts_us: u64,
    /// FEC group size used for this frame (0 = no FEC).
    pub fec_k: u8,
}

impl MediaHeader {
    pub fn encode(&self, w: &mut Writer) {
        w.u16(self.epoch).u32(self.frame_id).u16(self.pkt_idx).u16(self.pkt_count).u8(self.flags).u64(self.pts_us).u8(self.fec_k);
    }
    pub fn decode(r: &mut Reader) -> Result<Self, ProtoError> {
        let h = Self {
            epoch: r.u16()?,
            frame_id: r.u32()?,
            pkt_idx: r.u16()?,
            pkt_count: r.u16()?,
            flags: r.u8()?,
            pts_us: r.u64()?,
            fec_k: r.u8()?,
        };
        if h.pkt_count == 0 || h.pkt_idx >= h.pkt_count {
            return Err(ProtoError::Invalid("packet index"));
        }
        Ok(h)
    }
    pub fn is_key(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
}

/// XOR parity over one *interleaved* group of data packets of a frame.
///
/// A frame with `pkt_count` packets and `groups` groups puts packet `i` in group
/// `i % groups`. Consecutive packets therefore land in different groups, so a
/// burst of up to `groups` consecutive losses is fully recoverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FecHeader {
    pub epoch: u16,
    pub frame_id: u32,
    pub group: u16,
    pub groups: u16,
    pub pkt_count: u16,
    /// XOR of the payload lengths of the covered packets.
    pub xor_len: u16,
}

impl FecHeader {
    pub fn encode(&self, w: &mut Writer) {
        w.u16(self.epoch).u32(self.frame_id).u16(self.group).u16(self.groups).u16(self.pkt_count).u16(self.xor_len);
    }
    pub fn decode(r: &mut Reader) -> Result<Self, ProtoError> {
        let h = Self {
            epoch: r.u16()?,
            frame_id: r.u32()?,
            group: r.u16()?,
            groups: r.u16()?,
            pkt_count: r.u16()?,
            xor_len: r.u16()?,
        };
        if h.groups == 0 || h.group >= h.groups || h.pkt_count == 0 {
            return Err(ProtoError::Invalid("fec group"));
        }
        Ok(h)
    }
    /// Data packet indexes protected by this parity packet.
    pub fn members(&self) -> impl Iterator<Item = u16> {
        let (g, n, c) = (self.group, self.groups, self.pkt_count);
        (g..c).step_by(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_header_roundtrip_and_len() {
        let h = MediaHeader { epoch: 3, frame_id: 99, pkt_idx: 2, pkt_count: 9, flags: FLAG_KEYFRAME, pts_us: 123456789, fec_k: 8 };
        let mut w = Writer::new();
        h.encode(&mut w);
        let v = w.finish();
        assert_eq!(v.len(), MEDIA_HEADER_LEN);
        assert_eq!(MediaHeader::decode(&mut Reader::new(&v)).unwrap(), h);
    }

    #[test]
    fn media_header_rejects_bad_index() {
        let h = MediaHeader { epoch: 0, frame_id: 0, pkt_idx: 5, pkt_count: 5, flags: 0, pts_us: 0, fec_k: 0 };
        let mut w = Writer::new();
        h.encode(&mut w);
        assert!(MediaHeader::decode(&mut Reader::new(&w.finish())).is_err());
    }

    #[test]
    fn fec_header_len() {
        let h = FecHeader { epoch: 1, frame_id: 2, group: 1, groups: 3, pkt_count: 8, xor_len: 7 };
        let mut w = Writer::new();
        h.encode(&mut w);
        let v = w.finish();
        assert_eq!(v.len(), FEC_HEADER_LEN);
        assert_eq!(FecHeader::decode(&mut Reader::new(&v)).unwrap(), h);
        assert_eq!(h.members().collect::<Vec<_>>(), vec![1, 4, 7]);
    }
}
