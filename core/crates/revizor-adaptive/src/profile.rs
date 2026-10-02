/// Resolution class (shorter side in pixels) and frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tier {
    pub short_side: u16,
    pub fps: u16,
}

impl Tier {
    pub const fn new(short_side: u16, fps: u16) -> Self {
        Self { short_side, fps }
    }
    /// Approximate pixel rate for a 16:9 frame of this tier (used for bitrate scaling).
    pub fn pixel_rate(&self) -> u64 {
        let h = self.short_side as u64;
        h * (h * 16 / 9) * self.fps as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    BatterySaver,
    Balanced,
    Quality,
    LowLatency,
    /// User pinned something; see [`Config::lock_tier`] / [`Config::fixed_bitrate`].
    Custom,
}

/// Hard limits discovered by capability negotiation / the user.
#[derive(Debug, Clone, Copy)]
pub struct Ceiling {
    pub max_short_side: u16,
    pub max_fps: u16,
    pub max_bitrate_bps: u32,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub profile: Profile,
    pub ladder: Vec<Tier>,
    pub max_bitrate_bps: u32,
    /// bits per pixel: floor / start / ceiling for a tier.
    pub bpp_min: f64,
    pub bpp_start: f64,
    pub bpp_max: f64,
    pub loss_high_pct: f32,
    pub loss_healthy_pct: f32,
    /// Queueing delay (RTT above the baseline) considered congestion, µs.
    pub queue_delay_high_us: u32,
    pub decrease_factor: f64,
    pub decrease_cooldown_ms: u64,
    pub increase_hold_ms: u64,
    /// Pinned at the bitrate floor this long → drop a tier.
    pub down_dwell_ms: u64,
    pub up_dwell_ms: u64,
    pub min_tier_dwell_ms: u64,
    /// A downgrade within this long of an upgrade counts as a flap.
    pub probation_ms: u64,
    pub thermal_clear_hold_ms: u64,
    pub overload_hold_ms: u64,
    pub battery_low_pct: u8,
    /// Custom profile: never leave this ladder index.
    pub lock_tier: Option<Tier>,
    /// Custom profile: never change the bitrate.
    pub fixed_bitrate: Option<u32>,
    /// Start at this ladder index (0 = best allowed).
    pub start_index: usize,
}

impl Config {
    pub fn for_profile(profile: Profile, c: Ceiling) -> Self {
        // Preference order. Balanced/Quality sacrifice fps before resolution late;
        // LowLatency keeps 60 fps as long as possible; BatterySaver never goes above 1080p30.
        let all: &[Tier] = match profile {
            Profile::LowLatency => &[Tier::new(1440, 60), Tier::new(1080, 60), Tier::new(720, 60), Tier::new(720, 30), Tier::new(540, 30)],
            Profile::BatterySaver => &[Tier::new(1080, 30), Tier::new(720, 30), Tier::new(540, 30)],
            _ => &[Tier::new(1440, 60), Tier::new(1080, 60), Tier::new(1080, 30), Tier::new(720, 30), Tier::new(540, 30)],
        };
        let mut ladder: Vec<Tier> = all
            .iter()
            .map(|t| Tier::new(t.short_side.min(c.max_short_side), t.fps.min(c.max_fps)))
            .collect();
        ladder.dedup();
        if ladder.is_empty() {
            ladder.push(Tier::new(c.max_short_side.min(720), c.max_fps.min(30).max(1)));
        }
        let (bpp_start, bpp_max, loss_high, down_dwell, up_dwell) = match profile {
            Profile::Quality => (0.11, 0.22, 3.0, 4_000, 25_000),
            Profile::LowLatency => (0.08, 0.16, 1.5, 2_500, 20_000),
            Profile::BatterySaver => (0.07, 0.12, 3.0, 3_000, 45_000),
            _ => (0.09, 0.20, 2.0, 3_000, 20_000),
        };
        Self {
            profile,
            ladder,
            max_bitrate_bps: c.max_bitrate_bps,
            bpp_min: 0.035,
            bpp_start,
            bpp_max,
            loss_high_pct: loss_high,
            loss_healthy_pct: 0.5,
            queue_delay_high_us: if profile == Profile::LowLatency { 15_000 } else { 30_000 },
            decrease_factor: 0.85,
            decrease_cooldown_ms: 600,
            increase_hold_ms: 2_000,
            down_dwell_ms: down_dwell,
            up_dwell_ms: up_dwell,
            min_tier_dwell_ms: 5_000,
            probation_ms: 30_000,
            thermal_clear_hold_ms: 45_000,
            overload_hold_ms: 2_000,
            battery_low_pct: 15,
            lock_tier: None,
            fixed_bitrate: None,
            start_index: 0,
        }
    }

    /// Custom profile: pin tier and/or bitrate chosen in the advanced settings.
    pub fn custom(c: Ceiling, tier: Option<Tier>, bitrate: Option<u32>) -> Self {
        let mut cfg = Self::for_profile(Profile::Balanced, c);
        cfg.profile = Profile::Custom;
        cfg.lock_tier = tier;
        cfg.fixed_bitrate = bitrate;
        if let Some(t) = tier {
            cfg.ladder = vec![t];
        }
        cfg
    }

    pub(crate) fn bitrate_for(&self, t: Tier, bpp: f64) -> u32 {
        let bps = (t.pixel_rate() as f64 * bpp) as u64;
        bps.min(self.max_bitrate_bps as u64).max(500_000) as u32
    }
    pub fn tier_min_bps(&self, t: Tier) -> u32 {
        self.bitrate_for(t, self.bpp_min)
    }
    pub fn tier_start_bps(&self, t: Tier) -> u32 {
        self.bitrate_for(t, self.bpp_start)
    }
    pub fn tier_max_bps(&self, t: Tier) -> u32 {
        self.bitrate_for(t, self.bpp_max)
    }
}
