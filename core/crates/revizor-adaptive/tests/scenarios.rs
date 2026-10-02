//! Scenario tests: drive the engine with synthetic *inputs* (the engine itself
//! is real logic) and assert on its decisions over simulated minutes.

use revizor_adaptive::*;

const STEP_MS: u64 = 500;

fn ceiling() -> Ceiling {
    Ceiling { max_short_side: 1440, max_fps: 60, max_bitrate_bps: 80_000_000 }
}

fn good() -> Signals {
    Signals { loss_pct: 0.0, rtt_us: Some(3_000), jitter_us: 500, encode_us: Some(4_000), decode_us: Some(3_000), ..Default::default() }
}
fn lossy(p: f32) -> Signals {
    Signals { loss_pct: p, rtt_us: Some(40_000), ..good() }
}

#[derive(Debug, Clone, Copy)]
struct Ev {
    t: u64,
    tier: Option<(Tier, Reason)>,
    bitrate: Option<u32>,
}

struct Sim {
    e: Engine,
    now: u64,
    log: Vec<Ev>,
}

impl Sim {
    fn new(cfg: Config) -> Self {
        Self { e: Engine::new(cfg), now: 0, log: vec![] }
    }
    fn run(&mut self, secs: u64, mut f: impl FnMut(u64) -> Signals) {
        let end = self.now + secs * 1000;
        while self.now < end {
            self.now += STEP_MS;
            let s = f(self.now);
            let u = self.e.on_signals(self.now, &s);
            if !u.is_empty() {
                self.log.push(Ev { t: self.now, tier: u.tier, bitrate: u.bitrate_bps });
            }
        }
    }
    fn tier_changes(&self) -> Vec<Ev> {
        self.log.iter().copied().filter(|e| e.tier.is_some()).collect()
    }
}

fn balanced() -> Config {
    Config::for_profile(Profile::Balanced, ceiling())
}

#[test]
fn good_network_ramps_bitrate_and_never_changes_tier() {
    let mut s = Sim::new(balanced());
    let start = s.e.bitrate_bps();
    s.run(180, |_| good());
    assert!(s.tier_changes().is_empty());
    assert_eq!(s.e.status().ladder_index, 0);
    let max = s.e.config().tier_max_bps(s.e.tier());
    assert!(s.e.bitrate_bps() > start);
    assert_eq!(s.e.bitrate_bps(), max, "bitrate should settle at the tier ceiling");
    assert_eq!(s.e.fec_k(), 0);
}

#[test]
fn single_bad_sample_is_ignored() {
    let mut s = Sim::new(balanced());
    s.run(10, |_| good());
    let before = s.e.bitrate_bps();
    s.run(1, |t| if t % 1000 == 0 { lossy(6.0) } else { good() });
    s.run(10, |_| good());
    assert!(s.e.bitrate_bps() >= before);
    assert!(s.tier_changes().is_empty());
}

#[test]
fn congestion_cuts_bitrate_first_then_tier_after_dwell() {
    let mut s = Sim::new(balanced());
    s.run(5, |_| good());
    let t0 = s.now;
    s.run(60, |_| lossy(6.0));
    let first = s.log.iter().find(|e| e.t > t0 && (e.bitrate.is_some() || e.tier.is_some())).unwrap();
    assert!(first.bitrate.is_some() && first.tier.is_none(), "bitrate must react before the tier: {first:?}");
    let ch = s.tier_changes();
    assert!(!ch.is_empty(), "sustained congestion must eventually reduce the tier");
    assert!(ch[0].t - t0 >= s.e.config().down_dwell_ms, "tier dropped too eagerly");
    assert_eq!(ch[0].tier.unwrap().1, Reason::Network);
    assert!(s.e.status().limited_by.is_some());
}

#[test]
fn tier_changes_respect_minimum_dwell() {
    let mut s = Sim::new(balanced());
    s.run(300, |_| lossy(12.0));
    let ch = s.tier_changes();
    for w in ch.windows(2) {
        assert!(w[1].t - w[0].t >= s.e.config().min_tier_dwell_ms, "{w:?}");
    }
    assert_eq!(s.e.status().ladder_index, s.e.config().ladder.len() - 1, "ends at the floor under permanent loss");
}

#[test]
fn oscillating_link_does_not_flap() {
    let mut s = Sim::new(balanced());
    // 10 s bad / 10 s good for 10 minutes: shorter than the upgrade dwell.
    s.run(600, |t| if (t / 10_000) % 2 == 0 { lossy(6.0) } else { good() });
    let ch = s.tier_changes();
    let ups = ch.iter().filter(|e| e.tier.unwrap().1 == Reason::Recovery).count();
    assert_eq!(ups, 0, "upgrades must not fire inside 10 s windows: {ch:?}");
    assert!(ch.len() <= s.e.config().ladder.len() - 1);
}

#[test]
fn recovers_gradually_when_network_heals() {
    let mut s = Sim::new(balanced());
    s.run(60, |_| lossy(8.0));
    let low = s.e.status().ladder_index;
    assert!(low >= 1);
    s.run(600, |_| good());
    let ups: Vec<_> = s.tier_changes().into_iter().filter(|e| e.tier.unwrap().1 == Reason::Recovery).collect();
    assert!(!ups.is_empty());
    assert!(s.e.status().ladder_index < low, "quality should come back");
    for w in ups.windows(2) {
        assert!(w[1].t - w[0].t >= 15_000, "upgrades too fast: {w:?}");
    }
}

#[test]
fn premature_upgrade_is_penalised() {
    let mut s = Sim::new(balanced());
    s.run(40, |_| lossy(8.0)); // go down
    let down_idx = s.e.status().ladder_index;
    assert!(down_idx >= 1);
    // Heal until exactly one upgrade happens, then break the link again immediately.
    let mut upgraded_at = None;
    while upgraded_at.is_none() && s.now < 600_000 {
        s.run(1, |_| good());
        upgraded_at = s.log.iter().rev().find(|e| e.tier.is_some_and(|t| t.1 == Reason::Recovery)).map(|e| e.t);
    }
    let up_t = upgraded_at.expect("should upgrade eventually");
    s.run(40, |_| lossy(8.0));
    let flap_down_t = s.log.iter().rev().find(|e| e.tier.is_some_and(|t| t.1 == Reason::Network) && e.t > up_t).map(|e| e.t);
    assert!(flap_down_t.is_some(), "link broke again, tier must drop");
    // Now heal: because of the flap the next upgrade must wait longer than the base dwell.
    let heal_start = s.now;
    s.run(600, |_| good());
    let next_up = s.log.iter().find(|e| e.t > heal_start && e.tier.is_some_and(|t| t.1 == Reason::Recovery)).map(|e| e.t).expect("eventually upgrades");
    let flap_t = flap_down_t.unwrap();
    assert!(next_up - flap_t >= 2 * s.e.config().up_dwell_ms, "flap penalty missing: waited only {} ms after the flap", next_up - flap_t);
}

#[test]
fn thermal_steps_and_slow_release() {
    let mut s = Sim::new(balanced());
    s.run(10, |_| good());
    s.run(5, |_| Signals { thermal: ThermalLevel::Moderate, ..good() });
    assert!(s.e.status().ladder_index >= 1);
    let first = s.tier_changes()[0];
    assert_eq!(first.tier.unwrap().1, Reason::Thermal);
    assert_eq!(s.e.status().limited_by, Some(Reason::Thermal));

    s.run(5, |_| Signals { thermal: ThermalLevel::Severe, ..good() });
    assert!(s.e.status().ladder_index >= 2);

    // cools down: must NOT bounce back for at least the hold time
    let cool = s.now;
    s.run(30, |_| Signals { thermal: ThermalLevel::None, ..good() });
    assert!(s.e.status().ladder_index >= 2, "restored before thermal hold elapsed");
    s.run(600, |_| Signals { thermal: ThermalLevel::None, ..good() });
    assert_eq!(s.e.status().ladder_index, 0, "should fully recover after cooling");
    let ups: Vec<_> = s.tier_changes().into_iter().filter(|e| e.t > cool).collect();
    assert!(ups.iter().all(|e| e.tier.unwrap().1 == Reason::Recovery));
}

#[test]
fn thermal_critical_goes_to_floor_immediately() {
    let mut s = Sim::new(balanced());
    s.run(2, |_| good());
    s.run(1, |_| Signals { thermal: ThermalLevel::Critical, ..good() });
    assert_eq!(s.e.status().ladder_index, s.e.config().ladder.len() - 1);
}

#[test]
fn unknown_thermal_is_not_treated_as_hot() {
    let mut s = Sim::new(balanced());
    s.run(120, |_| Signals { thermal: ThermalLevel::Unknown, ..good() });
    assert!(s.tier_changes().is_empty());
}

#[test]
fn receiver_overheating_also_limits() {
    let mut s = Sim::new(balanced());
    s.run(3, |_| Signals { receiver_thermal: ThermalLevel::Severe, ..good() });
    assert!(s.e.status().ladder_index >= 2);
}

#[test]
fn low_battery_caps_quality_unless_charging() {
    let mut s = Sim::new(balanced());
    s.run(3, |_| Signals { battery: Some(Battery { percent: 9, charging: false, power_save: false }), ..good() });
    assert!(s.e.status().ladder_index >= 1);
    assert_eq!(s.e.status().limited_by, Some(Reason::Battery));

    let mut c = Sim::new(balanced());
    c.run(60, |_| Signals { battery: Some(Battery { percent: 9, charging: true, power_save: false }), ..good() });
    assert!(c.tier_changes().is_empty());
}

#[test]
fn encoder_overload_steps_down_with_reason() {
    let mut s = Sim::new(balanced());
    s.run(10, |_| good());
    // 60 fps → 16.7 ms budget; encoder needs 20 ms
    s.run(20, |_| Signals { encode_us: Some(20_000), ..good() });
    let ch = s.tier_changes();
    assert!(!ch.is_empty());
    assert_eq!(ch[0].tier.unwrap().1, Reason::EncoderOverload);
}

#[test]
fn decoder_overload_steps_down_with_reason() {
    let mut s = Sim::new(balanced());
    s.run(10, |_| good());
    s.run(20, |_| Signals { decode_us: Some(30_000), ..good() });
    assert_eq!(s.tier_changes()[0].tier.unwrap().1, Reason::DecoderOverload);
}

#[test]
fn custom_fixed_bitrate_and_tier_are_respected() {
    let cfg = Config::custom(ceiling(), Some(Tier::new(1080, 60)), Some(25_000_000));
    let mut s = Sim::new(cfg);
    s.run(120, |_| lossy(10.0));
    assert!(s.tier_changes().is_empty());
    assert_eq!(s.e.bitrate_bps(), 25_000_000);
    assert!(s.log.iter().all(|e| e.bitrate.is_none()));
}

#[test]
fn fec_follows_loss_with_hysteresis() {
    let mut s = Sim::new(balanced());
    s.run(20, |_| Signals { loss_pct: 2.0, ..good() });
    assert_eq!(s.e.fec_k(), 10);
    // loss disappears: FEC is released slowly, not on the first clean sample
    s.run(1, |_| good());
    assert_ne!(s.e.fec_k(), 0);
    s.run(30, |_| good());
    assert_eq!(s.e.fec_k(), 0);
}

#[test]
fn video_bitrate_accounts_for_fec_overhead() {
    let mut s = Sim::new(balanced());
    s.run(20, |_| Signals { loss_pct: 2.0, ..good() });
    let k = s.e.fec_k() as u64;
    assert!(k > 0);
    let total = s.e.bitrate_bps() as u64;
    assert_eq!(s.e.video_bitrate_bps() as u64, total * k / (k + 1));
}

#[test]
fn ladder_respects_ceiling_and_profiles() {
    let c = Ceiling { max_short_side: 1080, max_fps: 60, max_bitrate_bps: 30_000_000 };
    let b = Config::for_profile(Profile::Balanced, c);
    assert_eq!(b.ladder[0], Tier::new(1080, 60));
    assert!(b.ladder.iter().all(|t| t.short_side <= 1080));
    let ll = Config::for_profile(Profile::LowLatency, c);
    assert!(ll.ladder.iter().take(2).all(|t| t.fps == 60), "low latency keeps 60 fps as long as possible");
    let bs = Config::for_profile(Profile::BatterySaver, c);
    assert_eq!(bs.ladder[0], Tier::new(1080, 30));
    let c30 = Ceiling { max_short_side: 1440, max_fps: 30, max_bitrate_bps: 30_000_000 };
    let b30 = Config::for_profile(Profile::Balanced, c30);
    assert!(b30.ladder.iter().all(|t| t.fps <= 30));
}

#[test]
fn bitrate_never_exceeds_global_cap() {
    let c = Ceiling { max_short_side: 1440, max_fps: 60, max_bitrate_bps: 8_000_000 };
    let mut s = Sim::new(Config::for_profile(Profile::Quality, c));
    s.run(300, |_| good());
    assert!(s.e.bitrate_bps() <= 8_000_000);
}
