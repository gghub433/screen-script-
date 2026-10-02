//! Real network/timing measurements (RFC 3550 style). Nothing here is synthetic.

/// Interarrival jitter, RFC 3550 §6.4.1: `J += (|D| − J) / 16`.
#[derive(Default)]
pub struct JitterEstimator {
    last_transit: Option<i64>,
    jitter_us: f64,
}

impl JitterEstimator {
    /// `arrival_us` on the local clock, `send_ts_us` on the remote clock. The
    /// constant clock offset cancels out in the difference of transits.
    pub fn update(&mut self, arrival_us: u64, send_ts_us: u64) {
        let transit = arrival_us as i64 - send_ts_us as i64;
        if let Some(prev) = self.last_transit {
            let d = (transit - prev).abs() as f64;
            self.jitter_us += (d - self.jitter_us) / 16.0;
        }
        self.last_transit = Some(transit);
    }
    pub fn jitter_us(&self) -> u32 {
        self.jitter_us as u32
    }
}

/// Packet loss from the gaps in the sender's outer packet sequence numbers.
/// Counts per reporting interval and resets on [`take`](Self::take).
#[derive(Default)]
pub struct LossTracker {
    base: Option<u64>,
    highest: u64,
    received: u64,
    /// Packets numbered below this belong to an already-reported interval.
    floor: u64,
}

impl LossTracker {
    pub fn on_packet(&mut self, seq: u64) {
        if seq < self.floor {
            return; // late arrival from an interval that was already reported
        }
        match self.base {
            None => {
                self.base = Some(seq);
                self.highest = seq;
            }
            Some(b) => {
                self.base = Some(b.min(seq));
                self.highest = self.highest.max(seq);
            }
        }
        self.received += 1;
    }

    /// Returns `(expected, lost)` for the interval and starts a new one.
    pub fn take(&mut self) -> (u32, u32) {
        let Some(base) = self.base else { return (0, 0) };
        let expected = self.highest - base + 1;
        let lost = expected.saturating_sub(self.received);
        self.floor = self.highest + 1;
        self.base = None;
        self.received = 0;
        (expected.min(u32::MAX as u64) as u32, lost.min(u32::MAX as u64) as u32)
    }
}

/// Bytes-per-second meter over a sliding interval.
#[derive(Default)]
pub struct RateMeter {
    bytes: u64,
    since_us: u64,
}

impl RateMeter {
    pub fn add(&mut self, now_us: u64, bytes: usize) {
        if self.since_us == 0 {
            self.since_us = now_us;
        }
        self.bytes += bytes as u64;
    }
    /// Bits per second since the last call; resets the window.
    pub fn take_bps(&mut self, now_us: u64) -> u32 {
        let dt = now_us.saturating_sub(self.since_us);
        let bps = if dt == 0 { 0 } else { self.bytes * 8 * 1_000_000 / dt };
        self.bytes = 0;
        self.since_us = now_us;
        bps.min(u32::MAX as u64) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_jitter_for_perfectly_paced_stream() {
        let mut j = JitterEstimator::default();
        for i in 0..100u64 {
            j.update(1_000_000 + i * 16_667, i * 16_667);
        }
        assert_eq!(j.jitter_us(), 0);
    }

    #[test]
    fn jitter_converges_to_alternating_delay() {
        let mut j = JitterEstimator::default();
        for i in 0..2000u64 {
            let extra = if i % 2 == 0 { 0 } else { 2000 };
            j.update(i * 16_667 + extra, i * 16_667);
        }
        // |D| is always 2000 µs, estimator converges to 2000
        assert!((j.jitter_us() as i64 - 2000).abs() < 50, "{}", j.jitter_us());
    }

    #[test]
    fn loss_counts_gaps() {
        let mut l = LossTracker::default();
        for s in [10u64, 11, 13, 14, 17] {
            l.on_packet(s);
        }
        assert_eq!(l.take(), (8, 3));
        assert_eq!(l.take(), (0, 0));
    }

    #[test]
    fn loss_handles_reorder() {
        let mut l = LossTracker::default();
        for s in [5u64, 7, 6, 8] {
            l.on_packet(s);
        }
        assert_eq!(l.take(), (4, 0));
    }

    #[test]
    fn rate_meter() {
        let mut m = RateMeter::default();
        m.add(1_000_000, 0);
        m.add(1_500_000, 125_000);
        assert_eq!(m.take_bps(2_000_000), 1_000_000);
    }
}
