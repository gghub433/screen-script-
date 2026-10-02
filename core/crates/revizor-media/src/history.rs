use crate::packetizer::PacketizedFrame;
use std::collections::VecDeque;
use std::sync::Arc;

/// Recently sent frames, kept for NACK retransmission. Bounded by age and by
/// bytes so memory use is constant over multi-hour sessions.
pub struct SendHistory {
    frames: VecDeque<(u64, Arc<PacketizedFrame>)>,
    bytes: usize,
    max_age_us: u64,
    max_bytes: usize,
}

impl SendHistory {
    pub fn new(max_age_us: u64, max_bytes: usize) -> Self {
        Self { frames: VecDeque::new(), bytes: 0, max_age_us, max_bytes }
    }

    pub fn push(&mut self, now_us: u64, f: Arc<PacketizedFrame>) {
        self.bytes += f.frame.data.len();
        self.frames.push_back((now_us, f));
        self.evict(now_us);
    }

    fn evict(&mut self, now_us: u64) {
        while let Some((t, f)) = self.frames.front() {
            if now_us.saturating_sub(*t) > self.max_age_us || self.bytes > self.max_bytes {
                self.bytes -= f.frame.data.len();
                self.frames.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn get(&self, epoch: u16, frame_id: u32) -> Option<Arc<PacketizedFrame>> {
        self.frames.iter().rev().find(|(_, f)| f.frame.frame_id == frame_id && f.frame.epoch == epoch).map(|(_, f)| f.clone())
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packetizer::EncodedFrame;

    fn pf(id: u32, n: usize) -> Arc<PacketizedFrame> {
        Arc::new(PacketizedFrame::new(EncodedFrame { epoch: 0, frame_id: id, pts_us: 0, flags: 0, data: vec![0; n] }, 0))
    }

    #[test]
    fn evicts_by_age_and_bytes() {
        let mut h = SendHistory::new(100, 1000);
        h.push(0, pf(1, 400));
        h.push(10, pf(2, 400));
        assert!(h.get(0, 1).is_some());
        h.push(20, pf(3, 400)); // 1200 bytes > 1000: oldest goes
        assert!(h.get(0, 1).is_none());
        assert!(h.get(0, 3).is_some());
        h.push(500, pf(4, 10)); // everything older than 100 µs goes
        assert_eq!(h.len(), 1);
        assert_eq!(h.bytes(), 10);
    }

    #[test]
    fn epoch_must_match() {
        let mut h = SendHistory::new(100, 1000);
        h.push(0, pf(1, 4));
        assert!(h.get(1, 1).is_none());
    }
}
