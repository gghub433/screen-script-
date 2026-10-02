use crate::MAX_MEDIA_PAYLOAD;
use revizor_proto::wire::Writer;
use revizor_proto::{FecHeader, MediaHeader};
use std::sync::Arc;

/// One encoded access unit from the hardware encoder (Annex-B for H.264/H.265,
/// OBU stream for AV1; or one audio frame).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub epoch: u16,
    pub frame_id: u32,
    /// Capture time on the sender's monotonic clock, µs.
    pub pts_us: u64,
    pub flags: u8,
    pub data: Vec<u8>,
}

/// A frame split into packets. Data packets borrow slices of the shared frame,
/// so nothing is copied until the cipher builds the datagram, and the
/// retransmit history can keep the frame once for all of its packets.
pub struct PacketizedFrame {
    pub frame: Arc<EncodedFrame>,
    pub pkt_count: u16,
    pub payload: usize,
    pub fec_k: u8,
    pub groups: u16,
    /// Parity payloads, one per group (empty when FEC is off).
    parity: Vec<(u16, Vec<u8>)>,
}

impl PacketizedFrame {
    /// `fec_k` = data packets per parity packet (0 disables FEC; values 1..=3 are
    /// clamped to 4 because < 4 would exceed 25 % overhead).
    pub fn new(frame: EncodedFrame, fec_k: u8) -> Self {
        Self::with_payload(frame, fec_k, MAX_MEDIA_PAYLOAD)
    }

    pub fn with_payload(frame: EncodedFrame, fec_k: u8, payload: usize) -> Self {
        assert!(payload > 0);
        let len = frame.data.len();
        let pkt_count = len.div_ceil(payload).max(1);
        assert!(pkt_count <= u16::MAX as usize, "frame too large to packetize");
        let pkt_count = pkt_count as u16;
        let fec_k = if fec_k == 0 || pkt_count < 2 { 0 } else { fec_k.max(4) };
        let groups = if fec_k == 0 { 0 } else { (pkt_count as usize).div_ceil(fec_k as usize) as u16 };

        let mut parity: Vec<(u16, Vec<u8>)> = Vec::with_capacity(groups as usize);
        for g in 0..groups {
            let mut buf = vec![0u8; payload.min(len.max(1))];
            let mut xor_len = 0u16;
            for i in (g..pkt_count).step_by(groups as usize) {
                let (s, e) = Self::range(len, payload, i);
                xor_len ^= (e - s) as u16;
                for (b, d) in buf.iter_mut().zip(&frame.data[s..e]) {
                    *b ^= *d;
                }
            }
            parity.push((xor_len, buf));
        }
        Self { frame: Arc::new(frame), pkt_count, payload, fec_k, groups, parity }
    }

    fn range(len: usize, payload: usize, idx: u16) -> (usize, usize) {
        let s = idx as usize * payload;
        (s.min(len), (s + payload).min(len))
    }

    pub fn header(&self, idx: u16, retransmit: bool) -> MediaHeader {
        let mut flags = self.frame.flags;
        if retransmit {
            flags |= revizor_proto::FLAG_RETRANSMIT;
        }
        MediaHeader {
            epoch: self.frame.epoch,
            frame_id: self.frame.frame_id,
            pkt_idx: idx,
            pkt_count: self.pkt_count,
            flags,
            pts_us: self.frame.pts_us,
            fec_k: self.fec_k,
        }
    }

    /// Encoded header bytes + payload slice of data packet `idx`.
    pub fn data_packet(&self, idx: u16, retransmit: bool) -> Option<(Vec<u8>, &[u8])> {
        if idx >= self.pkt_count {
            return None;
        }
        let mut w = Writer::with_capacity(revizor_proto::MEDIA_HEADER_LEN);
        self.header(idx, retransmit).encode(&mut w);
        let (s, e) = Self::range(self.frame.data.len(), self.payload, idx);
        Some((w.finish(), &self.frame.data[s..e]))
    }

    /// Encoded header bytes + parity payload of each FEC group.
    pub fn fec_packets(&self) -> impl Iterator<Item = (Vec<u8>, &[u8])> + '_ {
        self.parity.iter().enumerate().map(move |(g, (xor_len, buf))| {
            let mut w = Writer::with_capacity(revizor_proto::FEC_HEADER_LEN);
            FecHeader {
                epoch: self.frame.epoch,
                frame_id: self.frame.frame_id,
                group: g as u16,
                groups: self.groups,
                pkt_count: self.pkt_count,
                xor_len: *xor_len,
            }
            .encode(&mut w);
            (w.finish(), buf.as_slice())
        })
    }

    /// Approximate bytes on the wire (payload + media headers, excluding envelope).
    pub fn wire_bytes(&self) -> usize {
        self.frame.data.len()
            + self.pkt_count as usize * revizor_proto::MEDIA_HEADER_LEN
            + self.parity.iter().map(|(_, b)| b.len() + revizor_proto::FEC_HEADER_LEN).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize) -> EncodedFrame {
        EncodedFrame { epoch: 1, frame_id: 5, pts_us: 42, flags: 0, data: (0..len).map(|i| (i * 7 + 3) as u8).collect() }
    }

    #[test]
    fn splits_and_covers_whole_frame() {
        let pf = PacketizedFrame::with_payload(frame(1000), 0, 300);
        assert_eq!(pf.pkt_count, 4);
        let mut joined = vec![];
        for i in 0..4 {
            joined.extend_from_slice(pf.data_packet(i, false).unwrap().1);
        }
        assert_eq!(joined, pf.frame.data);
        assert!(pf.data_packet(4, false).is_none());
        assert_eq!(pf.fec_packets().count(), 0);
    }

    #[test]
    fn empty_frame_is_one_packet() {
        let pf = PacketizedFrame::with_payload(frame(0), 8, 300);
        assert_eq!(pf.pkt_count, 1);
        assert_eq!(pf.fec_k, 0);
    }

    #[test]
    fn fec_overhead_is_about_one_over_k() {
        let pf = PacketizedFrame::with_payload(frame(100 * 500), 10, 500);
        assert_eq!(pf.pkt_count, 100);
        assert_eq!(pf.groups, 10);
        assert_eq!(pf.fec_packets().count(), 10);
    }

    #[test]
    fn retransmit_flag_set() {
        let pf = PacketizedFrame::with_payload(frame(10), 0, 300);
        assert_ne!(pf.header(0, true).flags & revizor_proto::FLAG_RETRANSMIT, 0);
        assert_eq!(pf.header(0, false).flags & revizor_proto::FLAG_RETRANSMIT, 0);
    }
}
