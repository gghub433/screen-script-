use std::collections::VecDeque;

/// NTP-style clock offset and RTT estimation from Ping/Pong exchanges.
///
/// For a ping sent at local `t0`, received by the peer at `t1`, answered at
/// `t2` and received locally at `t3` (all microseconds, each on its own clock):
/// `rtt = (t3 − t0) − (t2 − t1)` and `offset = ((t1 − t0) + (t2 − t3)) / 2`
/// where `offset = peer_clock − local_clock`. The sample with the smallest RTT
/// in the recent window is the least affected by queueing, so it is used.
#[derive(Default)]
pub struct ClockSync {
    samples: VecDeque<(u64, i64, u32)>, // (local time, offset, rtt)
    srtt_us: Option<f64>,
}

const WINDOW_US: u64 = 30_000_000;

impl ClockSync {
    pub fn on_pong(&mut self, t0: u64, t1: u64, t2: u64, t3: u64) {
        let rtt = (t3 as i64 - t0 as i64) - (t2 as i64 - t1 as i64);
        if rtt < 0 {
            return; // clocks misbehaved; ignore
        }
        let offset = ((t1 as i64 - t0 as i64) + (t2 as i64 - t3 as i64)) / 2;
        self.samples.push_back((t3, offset, rtt as u32));
        while self.samples.front().is_some_and(|s| t3.saturating_sub(s.0) > WINDOW_US) {
            self.samples.pop_front();
        }
        self.srtt_us = Some(match self.srtt_us {
            None => rtt as f64,
            Some(s) => s + (rtt as f64 - s) / 8.0,
        });
    }

    /// `peer_clock − local_clock`, µs. `None` until the first pong.
    pub fn offset_us(&self) -> Option<i64> {
        self.samples.iter().min_by_key(|s| s.2).map(|s| s.1)
    }
    pub fn srtt_us(&self) -> Option<u32> {
        self.srtt_us.map(|v| v as u32)
    }
    /// Smallest RTT among samples from the last `window_us`. Queueing only ever *adds* delay, so the
    /// minimum over a short window ignores scheduling spikes yet still reveals a standing queue.
    pub fn recent_min_rtt_us(&self, now_us: u64, window_us: u64) -> Option<u32> {
        self.samples.iter().filter(|s| now_us.saturating_sub(s.0) <= window_us).map(|s| s.2).min()
    }
    pub fn min_rtt_us(&self) -> Option<u32> {
        self.samples.iter().map(|s| s.2).min()
    }
    pub fn has_sync(&self) -> bool {
        !self.samples.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_offset_and_rtt_with_symmetric_path() {
        // peer clock = local + 5_000_000; one-way 2 ms each way; peer turnaround 100 µs
        let off = 5_000_000i64;
        let mut s = ClockSync::default();
        let t0 = 1_000_000u64;
        let t1 = (t0 as i64 + 2_000 + off) as u64;
        let t2 = t1 + 100;
        let t3 = t0 + 2_000 + 100 + 2_000;
        s.on_pong(t0, t1, t2, t3);
        assert_eq!(s.offset_us(), Some(off));
        assert_eq!(s.srtt_us(), Some(4_000));
    }

    #[test]
    fn prefers_min_rtt_sample() {
        let mut s = ClockSync::default();
        // congested sample (asymmetric queueing → skewed offset)
        s.on_pong(0, 20_000, 20_000, 30_000);
        // clean sample
        s.on_pong(100_000, 101_000, 101_000, 102_000);
        assert_eq!(s.min_rtt_us(), Some(2_000));
        assert_eq!(s.offset_us(), Some(0));
    }

    #[test]
    fn recent_min_ignores_old_and_spiky_samples() {
        let mut s = ClockSync::default();
        s.on_pong(0, 0, 0, 1_000); // old, 1 ms
        s.on_pong(10_000_000, 10_000_000, 10_000_000, 10_030_000); // spike 30 ms
        s.on_pong(10_250_000, 10_250_000, 10_250_000, 10_254_000); // 4 ms
        assert_eq!(s.recent_min_rtt_us(10_300_000, 1_500_000), Some(4_000));
        assert_eq!(s.recent_min_rtt_us(100_000_000, 1_500_000), None);
    }

    #[test]
    fn negative_rtt_ignored() {
        let mut s = ClockSync::default();
        s.on_pong(100, 0, 1000, 200);
        assert!(!s.has_sync());
    }
}
