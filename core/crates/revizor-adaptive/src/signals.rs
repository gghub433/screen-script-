/// Android `PowerManager.THERMAL_STATUS_*` collapsed to what matters. `Unknown`
/// means the platform cannot report it — never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalLevel {
    Unknown,
    None,
    Light,
    Moderate,
    Severe,
    Critical,
}

impl ThermalLevel {
    /// From Android `getCurrentThermalStatus()` (0..=6).
    pub fn from_android(status: i32) -> Self {
        match status {
            0 => Self::None,
            1 => Self::Light,
            2 => Self::Moderate,
            3 => Self::Severe,
            s if s >= 4 => Self::Critical,
            _ => Self::Unknown,
        }
    }
    /// Maps back to the 0..=6 style byte used in receiver reports (255 = unknown).
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Unknown => 255,
            Self::None => 0,
            Self::Light => 1,
            Self::Moderate => 2,
            Self::Severe => 3,
            Self::Critical => 4,
        }
    }
    pub fn from_wire(b: u8) -> Self {
        if b == 255 {
            Self::Unknown
        } else {
            Self::from_android(b as i32)
        }
    }
    /// Number of ladder steps quality must be reduced by.
    pub(crate) fn steps(self) -> usize {
        match self {
            Self::Unknown | Self::None | Self::Light => 0,
            Self::Moderate => 1,
            Self::Severe => 2,
            Self::Critical => usize::MAX,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
    /// OS battery-saver mode is on.
    pub power_save: bool,
}

/// One measurement interval. Fields the platform cannot measure are `None` /
/// `Unknown` and are simply ignored by the engine.
#[derive(Debug, Clone, Copy)]
pub struct Signals {
    /// Packets lost on the network before FEC/retransmit recovery, percent.
    pub loss_pct: f32,
    pub rtt_us: Option<u32>,
    pub jitter_us: u32,
    /// Bitrate the receiver actually received (bps).
    pub recv_bps: Option<u32>,
    /// Bitrate the sender actually emitted in the same interval (bps).
    pub sent_bps: u32,
    /// Fraction (0..1) of frames dropped by sender queue, receiver timeout or decoder queue.
    pub dropped_ratio: f32,
    /// Mean encode time per frame, µs.
    pub encode_us: Option<u32>,
    /// Mean decode time per frame, µs.
    pub decode_us: Option<u32>,
    pub thermal: ThermalLevel,
    pub receiver_thermal: ThermalLevel,
    pub battery: Option<Battery>,
}

impl Default for Signals {
    fn default() -> Self {
        Self {
            loss_pct: 0.0,
            rtt_us: None,
            jitter_us: 0,
            recv_bps: None,
            sent_bps: 0,
            dropped_ratio: 0.0,
            encode_us: None,
            decode_us: None,
            thermal: ThermalLevel::Unknown,
            receiver_thermal: ThermalLevel::Unknown,
            battery: None,
        }
    }
}

/// Why quality is (or was) reduced; shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Network,
    EncoderOverload,
    DecoderOverload,
    Thermal,
    Battery,
    Recovery,
    Profile,
}
