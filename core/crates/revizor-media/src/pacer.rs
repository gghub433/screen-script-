/// Burst-tolerant pacer (GCRA / token bucket): spreads a frame's packets over
/// time instead of dumping them in one burst, which overflows Wi-Fi/AP queues
/// and causes the very loss and jitter we are trying to avoid.
///
/// Credit accumulates while idle (or when the OS sleeps longer than asked) up
/// to `burst_bytes`, so coarse timer granularity (1–15 ms on Windows) does not
/// reduce the achieved rate below the configured one.
pub struct Pacer {
    rate_bps: u64,
    burst_bytes: u64,
    /// Theoretical arrival time of the next packet, µs.
    tat_us: u64,
}

impl Pacer {
    /// `rate_bps` is the *pacing* rate, typically 2–3× the video bitrate so a
    /// frame still leaves well within its frame interval.
    pub fn new(rate_bps: u64, burst_bytes: u64) -> Self {
        Self { rate_bps: rate_bps.max(100_000), burst_bytes, tat_us: 0 }
    }

    pub fn set_rate(&mut self, rate_bps: u64) {
        self.rate_bps = rate_bps.max(100_000);
    }

    /// Returns how long (µs) the caller must wait before sending `bytes`, and
    /// accounts for the send.
    pub fn delay_us(&mut self, now_us: u64, bytes: usize) -> u64 {
        let cost = bytes as u64 * 8 * 1_000_000 / self.rate_bps;
        let tau = self.burst_bytes * 8 * 1_000_000 / self.rate_bps;
        let tat = self.tat_us.max(now_us.saturating_sub(tau));
        let wait = tat.saturating_sub(tau).saturating_sub(now_us);
        self.tat_us = tat + cost;
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_burst_then_paces() {
        // 8 Mbit/s → 1000 bytes = 1 ms. Burst allowance 3000 bytes = 3 ms.
        let mut p = Pacer::new(8_000_000, 3000);
        let waits: Vec<_> = (0..8).map(|_| p.delay_us(0, 1000)).collect();
        assert_eq!(&waits[..4], &[0, 0, 0, 0]);
        assert_eq!(waits[4], 1000);
        assert_eq!(waits[7], 4000);
    }

    #[test]
    fn idle_resets_debt() {
        let mut p = Pacer::new(8_000_000, 1000);
        for _ in 0..10 {
            p.delay_us(0, 1000);
        }
        assert_eq!(p.delay_us(1_000_000, 1000), 0);
    }

    #[test]
    fn long_run_rate_matches() {
        let mut p = Pacer::new(10_000_000, 1500);
        let mut t = 0u64;
        let mut sent = 0u64;
        for _ in 0..1000 {
            t += p.delay_us(t, 1250);
            sent += 1250;
        }
        // 1.25 MB at 10 Mbit/s = 1.0 s
        let secs = t as f64 / 1e6;
        assert!((secs - 1.0).abs() < 0.05, "{secs}");
        assert_eq!(sent, 1_250_000);
    }
}
