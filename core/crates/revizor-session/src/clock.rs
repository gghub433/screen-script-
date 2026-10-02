/// Monotonic microsecond clock. On Unix it is `CLOCK_MONOTONIC`, the same
/// domain as Android `System.nanoTime()` and `MediaCodec` surface-input
/// timestamps, so codec PTS values can be used as capture timestamps without
/// conversion. Elsewhere it counts from the first call in the process.
pub trait Clock: Send + Sync {
    fn now_us(&self) -> u64;
}

#[derive(Default, Clone, Copy)]
pub struct MonotonicClock;

#[cfg(unix)]
impl Clock for MonotonicClock {
    fn now_us(&self) -> u64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: valid pointer to a timespec; CLOCK_MONOTONIC is always available.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
    }
}

#[cfg(not(unix))]
impl Clock for MonotonicClock {
    fn now_us(&self) -> u64 {
        static BASE: std::sync::OnceLock<std::time::Instant> = OnceLock::new();
        BASE.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64 + 1
    }
}

