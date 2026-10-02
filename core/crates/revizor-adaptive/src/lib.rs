//! Adaptive Streaming Engine.
//!
//! A pure state machine: feed it measured [`Signals`] (about every 500 ms) and
//! it returns what to change. It owns no threads, sockets or clocks, so its
//! behaviour is fully deterministic and unit-tested against simulated
//! scenarios (congestion, flapping links, thermal throttling, overload).
//!
//! Strategy, in order of cost to the viewer:
//! 1. **Bitrate** is adjusted continuously (cheap, live, no keyframe).
//! 2. **FEC** overhead follows measured loss.
//! 3. **Tier** (resolution × fps) changes only after bitrate has been pinned at
//!    the floor for a dwell time, or for hard reasons (thermal, battery,
//!    encoder/decoder overload). Each change needs a reconfigure + keyframe.
//! 4. Quality is restored slowly: long healthy dwell, headroom checks, and an
//!    exponentially growing penalty if an upgrade was followed by a downgrade
//!    (anti-flapping).

mod engine;
mod profile;
mod signals;

pub use engine::{Engine, EngineStatus, Update};
pub use profile::{Ceiling, Config, Profile, Tier};
pub use signals::{Battery, Reason, Signals, ThermalLevel};

/// Receiver playout delay suggestion from measured jitter: enough to absorb
/// real jitter, never more than the profile allows. Returns µs.
pub fn suggest_playout_delay_us(jitter_us: u32, frame_interval_us: u32, profile: Profile) -> u32 {
    let (min, max_frames) = match profile {
        Profile::LowLatency => (0, 1u32),
        Profile::Quality | Profile::BatterySaver => (frame_interval_us / 2, 4),
        _ => (frame_interval_us / 4, 2),
    };
    let want = jitter_us.saturating_mul(3);
    want.clamp(min, frame_interval_us.saturating_mul(max_frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playout_delay_follows_jitter_within_bounds() {
        let fi = 16_667;
        assert_eq!(suggest_playout_delay_us(0, fi, Profile::LowLatency), 0);
        assert_eq!(suggest_playout_delay_us(2_000, fi, Profile::LowLatency), 6_000);
        assert_eq!(suggest_playout_delay_us(50_000, fi, Profile::LowLatency), fi);
        assert_eq!(suggest_playout_delay_us(0, fi, Profile::Balanced), fi / 4);
        assert_eq!(suggest_playout_delay_us(1_000_000, fi, Profile::Quality), fi * 4);
    }
}
