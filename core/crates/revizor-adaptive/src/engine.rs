use crate::profile::{Config, Profile, Tier};
use crate::signals::{Reason, Signals};
use std::collections::VecDeque;

/// What the caller must apply after an evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Update {
    /// New tier → reconfigure encoder (new epoch) and force a keyframe.
    pub tier: Option<(Tier, Reason)>,
    /// New *total* bitrate (video + FEC). Applied live, no keyframe.
    pub bitrate_bps: Option<u32>,
    /// New FEC group size (0 = off). Applied to subsequent frames.
    pub fec_k: Option<u8>,
}

impl Update {
    pub fn is_empty(&self) -> bool {
        self.tier.is_none() && self.bitrate_bps.is_none() && self.fec_k.is_none()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EngineStatus {
    pub tier: Tier,
    pub bitrate_bps: u32,
    pub fec_k: u8,
    /// Set while quality is held below the best available tier.
    pub limited_by: Option<Reason>,
    pub ladder_index: usize,
    pub loss_pct: f32,
    pub queue_delay_us: u32,
}

struct Ewma {
    v: Option<f64>,
    alpha: f64,
}
impl Ewma {
    fn new(alpha: f64) -> Self {
        Self { v: None, alpha }
    }
    fn push(&mut self, x: f64) -> f64 {
        let n = match self.v {
            None => x,
            Some(v) => v + self.alpha * (x - v),
        };
        self.v = Some(n);
        n
    }
    fn get(&self) -> f64 {
        self.v.unwrap_or(0.0)
    }
}

pub struct Engine {
    cfg: Config,
    idx: usize,
    bitrate: u32,
    fec_k: u8,

    loss: Ewma,
    queue_us: Ewma,
    enc_load: Ewma,
    dec_load: Ewma,
    drops: Ewma,
    rtt_window: VecDeque<(u64, u32)>,

    congested_streak: u8,
    healthy_since: Option<u64>,
    pinned_since: Option<u64>,
    overload_since: Option<u64>,
    last_decrease_ms: u64,
    last_increase_ms: u64,
    last_tier_change_ms: u64,
    last_up_ms: Option<u64>,
    flaps: u32,
    last_flap_ms: u64,
    up_block_until_ms: u64,

    /// Best ladder index currently allowed by thermal/battery constraints.
    cap_idx: usize,
    cap_reason: Option<Reason>,
    cap_pressure_ms: u64,
    fec_pending: Option<(u8, u8)>,
    limited_by: Option<Reason>,
    started_ms: Option<u64>,
}

impl Engine {
    pub fn new(cfg: Config) -> Self {
        let idx = cfg.start_index.min(cfg.ladder.len() - 1);
        let tier = cfg.ladder[idx];
        let bitrate = cfg.fixed_bitrate.unwrap_or_else(|| cfg.tier_start_bps(tier));
        Self {
            idx,
            bitrate,
            fec_k: 0,
            loss: Ewma::new(0.35),
            queue_us: Ewma::new(0.3),
            enc_load: Ewma::new(0.3),
            dec_load: Ewma::new(0.3),
            drops: Ewma::new(0.3),
            rtt_window: VecDeque::new(),
            congested_streak: 0,
            healthy_since: None,
            pinned_since: None,
            overload_since: None,
            last_decrease_ms: 0,
            last_increase_ms: 0,
            last_tier_change_ms: 0,
            last_up_ms: None,
            flaps: 0,
            last_flap_ms: 0,
            up_block_until_ms: 0,
            cap_idx: 0,
            cap_reason: None,
            cap_pressure_ms: 0,
            fec_pending: None,
            limited_by: None,
            started_ms: None,
            cfg,
        }
    }

    pub fn tier(&self) -> Tier {
        self.cfg.ladder[self.idx]
    }
    pub fn config(&self) -> &Config {
        &self.cfg
    }
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate
    }
    pub fn fec_k(&self) -> u8 {
        self.fec_k
    }

    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            tier: self.tier(),
            bitrate_bps: self.bitrate,
            fec_k: self.fec_k,
            limited_by: self.limited_by,
            ladder_index: self.idx,
            loss_pct: self.loss.get() as f32,
            queue_delay_us: self.queue_us.get() as u32,
        }
    }

    /// Video bitrate to give the encoder: total budget minus FEC overhead.
    pub fn video_bitrate_bps(&self) -> u32 {
        if self.fec_k == 0 {
            self.bitrate
        } else {
            (self.bitrate as u64 * self.fec_k as u64 / (self.fec_k as u64 + 1)) as u32
        }
    }

    pub fn on_signals(&mut self, now_ms: u64, s: &Signals) -> Update {
        self.started_ms.get_or_insert(now_ms);
        let mut up = Update::default();
        let tier0 = self.tier();
        let br0 = self.bitrate;
        let fec0 = self.fec_k;

        self.ingest(now_ms, s);
        self.decay_flaps(now_ms);

        self.apply_caps(now_ms, s, &mut up);
        if self.cfg.profile != Profile::Custom || self.cfg.lock_tier.is_none() {
            self.check_overload(now_ms, s, &mut up);
        }
        self.network(now_ms, s, &mut up);
        self.update_fec();

        if self.tier() != tier0 {
            up.tier = up.tier.or(Some((self.tier(), Reason::Network)));
        } else {
            up.tier = None;
        }
        if self.bitrate != br0 && self.cfg.fixed_bitrate.is_none() {
            up.bitrate_bps = Some(self.bitrate);
        }
        if self.fec_k != fec0 {
            up.fec_k = Some(self.fec_k);
        }
        up
    }

    // ───────────────────────────── measurement intake ─────────────────────────────

    fn ingest(&mut self, now: u64, s: &Signals) {
        self.loss.push(s.loss_pct as f64);
        self.drops.push(s.dropped_ratio as f64);
        if let Some(rtt) = s.rtt_us {
            self.rtt_window.push_back((now, rtt));
            while self.rtt_window.front().is_some_and(|(t, _)| now - t > 30_000) {
                self.rtt_window.pop_front();
            }
            let base = self.rtt_window.iter().map(|(_, r)| *r).min().unwrap_or(rtt);
            self.queue_us.push(rtt.saturating_sub(base) as f64);
        }
        let frame_us = 1_000_000.0 / self.tier().fps as f64;
        if let Some(e) = s.encode_us {
            self.enc_load.push(e as f64 / frame_us);
        }
        if let Some(d) = s.decode_us {
            self.dec_load.push(d as f64 / frame_us);
        }
    }

    fn decay_flaps(&mut self, now: u64) {
        if self.flaps > 0 && now.saturating_sub(self.last_flap_ms) > 300_000 {
            self.flaps = 0;
        }
    }

    // ───────────────────────────── hard constraints ─────────────────────────────

    fn apply_caps(&mut self, now: u64, s: &Signals, up: &mut Update) {
        let n = self.cfg.ladder.len();
        let worst = s.thermal.max(s.receiver_thermal);
        let thermal_steps = worst.steps().min(n - 1);
        let mut want = thermal_steps;
        let mut reason = if thermal_steps > 0 { Some(Reason::Thermal) } else { None };

        if let Some(b) = s.battery {
            let low = !b.charging && b.percent <= self.cfg.battery_low_pct;
            if (low || b.power_save) && self.cfg.profile != Profile::Custom {
                // Battery constraint = stay at or below the second ladder step.
                let bat = 1.min(n - 1);
                if bat > want {
                    want = bat;
                    reason = Some(Reason::Battery);
                }
            }
        }

        if want > self.cap_idx {
            self.cap_idx = want;
            self.cap_reason = reason;
            self.cap_pressure_ms = now;
        } else if want < self.cap_idx {
            // Constraint is easing: release one step per hold period.
            if now.saturating_sub(self.cap_pressure_ms) >= self.cfg.thermal_clear_hold_ms {
                self.cap_idx -= 1;
                self.cap_pressure_ms = now;
                if self.cap_idx == 0 {
                    self.cap_reason = None;
                }
            }
        } else {
            // Still under the same constraint: keep the release timer fresh.
            if want > 0 {
                self.cap_pressure_ms = now;
            }
        }

        if self.idx < self.cap_idx {
            let r = self.cap_reason.unwrap_or(Reason::Thermal);
            self.move_to(now, self.cap_idx, r, up, true);
        }
        self.limited_by = if self.idx > 0 {
            if self.cap_idx > 0 && self.idx <= self.cap_idx {
                self.cap_reason
            } else {
                self.limited_by.or(Some(Reason::Network))
            }
        } else {
            None
        };
    }

    // ───────────────────────────── encoder / decoder overload ─────────────────────────────

    fn check_overload(&mut self, now: u64, _s: &Signals, up: &mut Update) {
        let enc = self.enc_load.get();
        let dec = self.dec_load.get();
        let drops = self.drops.get();
        let overloaded = enc > 0.92 || dec > 0.92 || drops > 0.05;
        if !overloaded {
            self.overload_since = None;
            return;
        }
        let since = *self.overload_since.get_or_insert(now);
        if now - since >= self.cfg.overload_hold_ms && self.can_step_down(now) {
            let reason = if dec > 0.92 { Reason::DecoderOverload } else { Reason::EncoderOverload };
            self.overload_since = None;
            self.step_down(now, reason, up);
        }
    }

    // ───────────────────────────── network-driven control ─────────────────────────────

    fn network(&mut self, now: u64, s: &Signals, up: &mut Update) {
        let loss = self.loss.get() as f32;
        let q = self.queue_us.get() as u32;
        let hi_loss = loss > self.cfg.loss_high_pct;
        let hi_q = q > self.cfg.queue_delay_high_us;
        let severe = s.loss_pct > 10.0;
        let congested_now = hi_loss || hi_q;
        self.congested_streak = if congested_now { self.congested_streak.saturating_add(1) } else { 0 };
        // A single bad sample is ignored unless it is a catastrophe.
        let congested = self.congested_streak >= 2 || severe;
        let healthy = loss < self.cfg.loss_healthy_pct && q < self.cfg.queue_delay_high_us / 3 && self.drops.get() < 0.01;

        let tier = self.tier();
        let (bmin, bmax) = (self.cfg.tier_min_bps(tier), self.cfg.tier_max_bps(tier));
        let locked_rate = self.cfg.fixed_bitrate.is_some();

        if congested {
            self.healthy_since = None;
            if !locked_rate && now.saturating_sub(self.last_decrease_ms) >= self.cfg.decrease_cooldown_ms {
                // Don't pretend the link carries more than the receiver actually saw.
                let mut base = self.bitrate as f64;
                if let (Some(r), true) = (s.recv_bps, s.loss_pct > 1.0) {
                    if r > 0 && (r as f64) < base {
                        base = r as f64;
                    }
                }
                let factor = if severe { 0.7 } else { self.cfg.decrease_factor };
                self.bitrate = ((base * factor) as u32).clamp(bmin, bmax);
                self.last_decrease_ms = now;
            }
            if self.bitrate <= bmin + bmin / 20 {
                self.pinned_since.get_or_insert(now);
            } else {
                self.pinned_since = None;
            }
            if let Some(p) = self.pinned_since {
                if now - p >= self.cfg.down_dwell_ms && self.can_step_down(now) {
                    self.pinned_since = None;
                    self.step_down(now, Reason::Network, up);
                }
            }
            return;
        }
        self.pinned_since = None;

        if !healthy {
            self.healthy_since = None;
            return;
        }
        let since = *self.healthy_since.get_or_insert(now);
        let held = now - since;

        // Additive increase once the link has been clean for a while.
        let tier = self.tier();
        let bmax = self.cfg.tier_max_bps(tier);
        if !locked_rate && held >= self.cfg.increase_hold_ms && now.saturating_sub(self.last_increase_ms) >= 500 && self.bitrate < bmax {
            let step = (bmax / 40).max(250_000);
            self.bitrate = (self.bitrate + step).min(bmax);
            self.last_increase_ms = now;
        }

        // Tier upgrade: long clean dwell, proven bitrate, headroom, no constraint.
        if self.idx > self.cap_idx && self.cfg.lock_tier.is_none() && held >= self.cfg.up_dwell_ms && now >= self.up_block_until_ms {
            let next = self.cfg.ladder[self.idx - 1];
            let proven = self.bitrate as f64 >= 0.9 * self.cfg.tier_start_bps(next) as f64;
            let headroom = self.enc_load.get() < 0.6 && self.dec_load.get() < 0.6 && self.drops.get() < 0.01;
            let dwell_ok = now.saturating_sub(self.last_tier_change_ms) >= self.cfg.min_tier_dwell_ms * 3;
            if proven && headroom && dwell_ok {
                let to = self.idx - 1;
                self.move_to(now, to, Reason::Recovery, up, false);
                self.last_up_ms = Some(now);
                self.healthy_since = Some(now); // require a fresh clean period at the new tier
            }
        }
    }

    // ───────────────────────────── tier movement ─────────────────────────────

    fn can_step_down(&self, now: u64) -> bool {
        self.cfg.lock_tier.is_none()
            && self.idx + 1 < self.cfg.ladder.len()
            && now.saturating_sub(self.last_tier_change_ms) >= self.cfg.min_tier_dwell_ms
    }

    fn step_down(&mut self, now: u64, reason: Reason, up: &mut Update) {
        // A downgrade shortly after an upgrade means the upgrade was premature.
        if self.last_up_ms.is_some_and(|t| now.saturating_sub(t) < self.cfg.probation_ms) {
            self.flaps += 1;
            self.last_flap_ms = now;
            let backoff = self.cfg.up_dwell_ms.saturating_mul(1 << self.flaps.min(4));
            self.up_block_until_ms = now + backoff.min(300_000);
            self.last_up_ms = None;
        }
        let to = self.idx + 1;
        self.move_to(now, to, reason, up, false);
        if reason == Reason::Network {
            // Land on the new tier at a rate the link was just shown to carry.
            let t = self.tier();
            self.bitrate = self.cfg.tier_start_bps(t).min(self.bitrate.max(self.cfg.tier_min_bps(t)));
        }
    }

    fn move_to(&mut self, now: u64, idx: usize, reason: Reason, up: &mut Update, emergency: bool) {
        let idx = idx.min(self.cfg.ladder.len() - 1);
        if idx == self.idx {
            return;
        }
        let from = self.tier();
        if emergency && idx > self.idx && self.last_up_ms.is_some() {
            self.last_up_ms = None;
        }
        self.idx = idx;
        let t = self.tier();
        self.last_tier_change_ms = now;
        let (bmin, bmax) = (self.cfg.tier_min_bps(t), self.cfg.tier_max_bps(t));
        self.bitrate = match self.cfg.fixed_bitrate {
            Some(b) => b,
            None if idx > 0 && from.pixel_rate() > t.pixel_rate() => self.bitrate.clamp(bmin, bmax).min(self.cfg.tier_start_bps(t).max(bmin)),
            None => self.bitrate.max(self.cfg.tier_start_bps(t)).clamp(bmin, bmax),
        };
        self.healthy_since = None;
        self.pinned_since = None;
        up.tier = Some((t, reason));
        log::info!("tier {}p{} -> {}p{} ({:?})", from.short_side, from.fps, t.short_side, t.fps, reason);
    }

    // ───────────────────────────── FEC ─────────────────────────────

    fn update_fec(&mut self) {
        let loss = self.loss.get();
        let want: u8 = match loss {
            l if l < 0.3 => 0,
            l if l < 1.0 => 16,
            l if l < 3.0 => 10,
            l if l < 8.0 => 6,
            _ => 4,
        };
        if want == self.fec_k {
            self.fec_pending = None;
            return;
        }
        // Require two consecutive evaluations agreeing; raise protection faster than lowering it.
        match self.fec_pending {
            Some((w, n)) if w == want => {
                let need = if want != 0 && (self.fec_k == 0 || want < self.fec_k) { 2 } else { 6 };
                if n + 1 >= need {
                    self.fec_k = want;
                    self.fec_pending = None;
                } else {
                    self.fec_pending = Some((w, n + 1));
                }
            }
            _ => self.fec_pending = Some((want, 1)),
        }
    }
}
