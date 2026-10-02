//! Capabilities exchanged before a session starts, and the negotiation that
//! turns two capability sets into the initial stream parameters.

use crate::wire::{Reader, Writer};
use crate::ProtoError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Codec {
    H264 = 1,
    H265 = 2,
    Av1 = 3,
}

impl Codec {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            1 => Ok(Self::H264),
            2 => Ok(Self::H265),
            3 => Ok(Self::Av1),
            _ => Err(ProtoError::Invalid("codec")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioCodec {
    None = 0,
    /// AAC-LC (MediaCodec `audio/mp4a-latm`), universally available in hardware/OS codecs.
    AacLc = 1,
    /// Opus (MediaCodec `audio/opus`, Android 10+ / Windows via libopus).
    Opus = 2,
}

impl AudioCodec {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            0 => Ok(Self::None),
            1 => Ok(Self::AacLc),
            2 => Ok(Self::Opus),
            _ => Err(ProtoError::Invalid("audio codec")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransportKind {
    Udp = 1,
    Tcp = 2,
}

impl TransportKind {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            1 => Ok(Self::Udp),
            2 => Ok(Self::Tcp),
            _ => Err(ProtoError::Invalid("transport")),
        }
    }
    pub fn bit(self) -> u8 {
        1 << (self as u8 - 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Platform {
    Android = 1,
    Windows = 2,
    Linux = 3,
    Other = 255,
}

impl Platform {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Android,
            2 => Self::Windows,
            3 => Self::Linux,
            _ => Self::Other,
        }
    }
}

/// What one side can do for a given codec. `max_*` come from the real
/// `MediaCodecInfo.VideoCapabilities` / MFT attributes, never from a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecCap {
    pub codec: Codec,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    /// True when backed by dedicated hardware (not a software codec).
    pub hardware: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub device_name: String,
    pub platform: Platform,
    pub codecs: Vec<CodecCap>,
    pub audio: Vec<AudioCodec>,
    /// Bitmask of `TransportKind::bit()`.
    pub transports: u8,
    /// HDR (PQ/10-bit) end-to-end. Not implemented in v1: always false.
    pub hdr: bool,
    pub max_bitrate_bps: u32,
}

impl Capabilities {
    pub fn encode(&self, w: &mut Writer) {
        w.str(&self.device_name);
        w.u8(self.platform as u8);
        w.u8(self.codecs.len() as u8);
        for c in &self.codecs {
            w.u8(c.codec as u8).u16(c.max_width).u16(c.max_height).u16(c.max_fps).u8(c.hardware as u8);
        }
        w.u8(self.audio.len() as u8);
        for a in &self.audio {
            w.u8(*a as u8);
        }
        w.u8(self.transports).u8(self.hdr as u8).u32(self.max_bitrate_bps);
    }

    pub fn decode(r: &mut Reader) -> Result<Self, ProtoError> {
        let device_name = r.str()?;
        let platform = Platform::from_u8(r.u8()?);
        let n = r.u8()? as usize;
        let mut codecs = Vec::with_capacity(n);
        for _ in 0..n {
            codecs.push(CodecCap {
                codec: Codec::from_u8(r.u8()?)?,
                max_width: r.u16()?,
                max_height: r.u16()?,
                max_fps: r.u16()?,
                hardware: r.u8()? != 0,
            });
        }
        let n = r.u8()? as usize;
        let mut audio = Vec::with_capacity(n);
        for _ in 0..n {
            audio.push(AudioCodec::from_u8(r.u8()?)?);
        }
        Ok(Self {
            device_name,
            platform,
            codecs,
            audio,
            transports: r.u8()?,
            hdr: r.u8()? != 0,
            max_bitrate_bps: r.u32()?,
        })
    }

    pub fn codec(&self, c: Codec) -> Option<&CodecCap> {
        self.codecs.iter().find(|x| x.codec == c)
    }
}

/// Parameters of a running stream. Changing any field bumps `epoch`; media
/// packets carry the epoch so the receiver never mixes frames from two configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoParams {
    pub codec: Codec,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_bps: u32,
    /// Keyframe interval in milliseconds (0 = on demand only).
    pub keyframe_interval_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_bps: u32,
}

impl AudioParams {
    pub const NONE: AudioParams =
        AudioParams { codec: AudioCodec::None, sample_rate: 0, channels: 0, bitrate_bps: 0 };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamParams {
    pub epoch: u16,
    pub video: VideoParams,
    pub audio: AudioParams,
}

impl StreamParams {
    pub fn encode(&self, w: &mut Writer) {
        w.u16(self.epoch);
        let v = &self.video;
        w.u8(v.codec as u8).u16(v.width).u16(v.height).u16(v.fps).u32(v.bitrate_bps).u32(v.keyframe_interval_ms);
        let a = &self.audio;
        w.u8(a.codec as u8).u32(a.sample_rate).u8(a.channels).u32(a.bitrate_bps);
    }

    pub fn decode(r: &mut Reader) -> Result<Self, ProtoError> {
        Ok(Self {
            epoch: r.u16()?,
            video: VideoParams {
                codec: Codec::from_u8(r.u8()?)?,
                width: r.u16()?,
                height: r.u16()?,
                fps: r.u16()?,
                bitrate_bps: r.u32()?,
                keyframe_interval_ms: r.u32()?,
            },
            audio: AudioParams {
                codec: AudioCodec::from_u8(r.u8()?)?,
                sample_rate: r.u32()?,
                channels: r.u8()?,
                bitrate_bps: r.u32()?,
            },
        })
    }
}

/// Codecs the *product* currently enables (feature gating by release, §53–55).
/// H.264 is the mandatory fallback; the others are listed in preference order
/// only when the build enables them.
#[derive(Debug, Clone)]
pub struct Preference {
    /// Codecs in descending preference. Must contain `H264` last as the fallback.
    pub codec_order: Vec<Codec>,
    pub want_audio: bool,
    /// Requested short-side in pixels (720/1080/1440). `None` = auto → highest common.
    pub max_short_side: Option<u16>,
    pub target_fps: u16,
    pub target_bitrate_bps: Option<u32>,
}

impl Default for Preference {
    fn default() -> Self {
        Self { codec_order: vec![Codec::H264], want_audio: true, max_short_side: None, target_fps: 60, target_bitrate_bps: None }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NegotiationError {
    #[error("no video codec supported by both devices")]
    NoCommonCodec,
    #[error("no common transport")]
    NoCommonTransport,
}

/// Result of negotiation: the initial parameters plus the ceiling the adaptive
/// engine must never exceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    pub params: StreamParams,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    pub hardware_both: bool,
    pub transport: TransportKind,
}

/// Default bitrate for a given pixel rate; ~0.1 bits/pixel for H.264 screen
/// content with low-latency CBR, scaled down for better codecs.
pub fn default_bitrate(codec: Codec, w: u32, h: u32, fps: u32) -> u32 {
    let bpp_milli = match codec {
        Codec::H264 => 100u64,
        Codec::H265 => 70,
        Codec::Av1 => 55,
    };
    let bps = (w as u64 * h as u64 * fps as u64 * bpp_milli) / 1000;
    bps.clamp(1_000_000, 80_000_000) as u32
}

/// Pick stream parameters both sides can really handle.
///
/// `src_w`/`src_h` are the captured source dimensions (orientation-aware).
/// `sender` is who encodes, `receiver` who decodes: a codec is usable only if
/// the sender can encode it *and* the receiver can decode it. Hardware on both
/// sides wins over software; among equals the order in `pref.codec_order` wins.
pub fn negotiate(
    sender: &Capabilities,
    receiver: &Capabilities,
    pref: &Preference,
    src_w: u16,
    src_h: u16,
    epoch: u16,
) -> Result<Negotiated, NegotiationError> {
    let mut best: Option<(bool, usize, CodecCap, CodecCap)> = None;
    for (rank, codec) in pref.codec_order.iter().enumerate() {
        let (Some(s), Some(r)) = (sender.codec(*codec), receiver.codec(*codec)) else { continue };
        let hw = s.hardware && r.hardware;
        let better = match &best {
            None => true,
            Some((bhw, brank, _, _)) => (hw && !bhw) || (hw == *bhw && rank < *brank),
        };
        if better {
            best = Some((hw, rank, *s, *r));
        }
    }
    let (hardware_both, _, s, r) = best.ok_or(NegotiationError::NoCommonCodec)?;

    let common = sender.transports & receiver.transports;
    let transport = if common & TransportKind::Udp.bit() != 0 {
        TransportKind::Udp
    } else if common & TransportKind::Tcp.bit() != 0 {
        TransportKind::Tcp
    } else {
        return Err(NegotiationError::NoCommonTransport);
    };

    let max_fps = s.max_fps.min(r.max_fps).max(1);
    let fps = pref.target_fps.min(max_fps);
    let cap_w = s.max_width.min(r.max_width);
    let cap_h = s.max_height.min(r.max_height);

    // Largest standard short-side (<= preference, <= source) that fits both codecs.
    let src_short = src_w.min(src_h);
    let limit = pref.max_short_side.unwrap_or(1440).min(src_short.max(1));
    let mut chosen = None;
    for &short in &[1440u16, 1080, 720, 540, 480] {
        if short > limit && short != 480 {
            continue;
        }
        let (w, h) = crate::geometry::fit_short_side(src_w, src_h, short.min(src_short), 2);
        if fits(w, h, cap_w, cap_h) {
            chosen = Some((w, h));
            break;
        }
    }
    let (w, h) = chosen.unwrap_or_else(|| crate::geometry::fit_short_side(src_w, src_h, 480, 2));

    let bitrate = pref
        .target_bitrate_bps
        .unwrap_or_else(|| default_bitrate(s.codec, w as u32, h as u32, fps as u32))
        .min(sender.max_bitrate_bps.min(receiver.max_bitrate_bps).max(1_000_000));

    let audio = if pref.want_audio {
        // Prefer Opus (lower latency) if both ends have it, else AAC-LC.
        let both = |c| sender.audio.contains(&c) && receiver.audio.contains(&c);
        if both(AudioCodec::Opus) {
            AudioParams { codec: AudioCodec::Opus, sample_rate: 48_000, channels: 2, bitrate_bps: 128_000 }
        } else if both(AudioCodec::AacLc) {
            AudioParams { codec: AudioCodec::AacLc, sample_rate: 48_000, channels: 2, bitrate_bps: 128_000 }
        } else {
            AudioParams::NONE
        }
    } else {
        AudioParams::NONE
    };

    Ok(Negotiated {
        params: StreamParams {
            epoch,
            video: VideoParams { codec: s.codec, width: w, height: h, fps, bitrate_bps: bitrate, keyframe_interval_ms: 0 },
            audio,
        },
        max_width: cap_w,
        max_height: cap_h,
        max_fps,
        hardware_both,
        transport,
    })
}

/// Encoders report limits as a bounding box that may be given in either
/// orientation (e.g. 4096x2304); a stream fits if it fits one of them.
fn fits(w: u16, h: u16, cap_w: u16, cap_h: u16) -> bool {
    (w <= cap_w && h <= cap_h) || (w <= cap_h && h <= cap_w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(codecs: &[(Codec, u16, u16, u16, bool)], audio: &[AudioCodec]) -> Capabilities {
        Capabilities {
            device_name: "dev".into(),
            platform: Platform::Android,
            codecs: codecs
                .iter()
                .map(|&(codec, w, h, f, hw)| CodecCap { codec, max_width: w, max_height: h, max_fps: f, hardware: hw })
                .collect(),
            audio: audio.to_vec(),
            transports: TransportKind::Udp.bit() | TransportKind::Tcp.bit(),
            hdr: false,
            max_bitrate_bps: 100_000_000,
        }
    }

    #[test]
    fn caps_roundtrip() {
        let c = caps(&[(Codec::H264, 4096, 2304, 120, true), (Codec::H265, 3840, 2160, 60, false)], &[AudioCodec::Opus]);
        let mut w = Writer::new();
        c.encode(&mut w);
        let v = w.finish();
        assert_eq!(Capabilities::decode(&mut Reader::new(&v)).unwrap(), c);
    }

    #[test]
    fn params_roundtrip() {
        let p = StreamParams {
            epoch: 7,
            video: VideoParams { codec: Codec::H265, width: 1080, height: 2400, fps: 60, bitrate_bps: 12_000_000, keyframe_interval_ms: 2000 },
            audio: AudioParams { codec: AudioCodec::Opus, sample_rate: 48000, channels: 2, bitrate_bps: 96000 },
        };
        let mut w = Writer::new();
        p.encode(&mut w);
        let v = w.finish();
        assert_eq!(StreamParams::decode(&mut Reader::new(&v)).unwrap(), p);
    }

    #[test]
    fn negotiates_1080p60_when_capped_by_receiver() {
        let s = caps(&[(Codec::H264, 4096, 2304, 120, true)], &[AudioCodec::AacLc]);
        let r = caps(&[(Codec::H264, 1920, 1088, 60, true)], &[AudioCodec::AacLc]);
        let n = negotiate(&s, &r, &Preference::default(), 2560, 1440, 1).unwrap();
        assert_eq!((n.params.video.width, n.params.video.height, n.params.video.fps), (1920, 1080, 60));
        assert!(n.hardware_both);
        assert_eq!(n.params.audio.codec, AudioCodec::AacLc);
    }

    #[test]
    fn negotiates_1440p_when_both_can() {
        let s = caps(&[(Codec::H264, 4096, 2304, 60, true)], &[]);
        let r = caps(&[(Codec::H264, 4096, 2304, 60, true)], &[]);
        let n = negotiate(&s, &r, &Preference::default(), 2560, 1440, 1).unwrap();
        assert_eq!((n.params.video.width, n.params.video.height), (2560, 1440));
        assert_eq!(n.params.audio.codec, AudioCodec::None);
    }

    #[test]
    fn portrait_phone_keeps_aspect() {
        let s = caps(&[(Codec::H264, 4096, 2304, 60, true)], &[]);
        let r = caps(&[(Codec::H264, 4096, 2304, 60, true)], &[]);
        let pref = Preference { max_short_side: Some(1080), ..Default::default() };
        let n = negotiate(&s, &r, &pref, 1440, 3200, 1).unwrap();
        assert_eq!((n.params.video.width, n.params.video.height), (1080, 2400));
    }

    #[test]
    fn hardware_beats_software_even_if_less_preferred() {
        let s = caps(&[(Codec::H265, 4096, 2304, 60, false), (Codec::H264, 4096, 2304, 60, true)], &[]);
        let r = caps(&[(Codec::H265, 4096, 2304, 60, true), (Codec::H264, 4096, 2304, 60, true)], &[]);
        let pref = Preference { codec_order: vec![Codec::H265, Codec::H264], ..Default::default() };
        let n = negotiate(&s, &r, &pref, 1920, 1080, 1).unwrap();
        assert_eq!(n.params.video.codec, Codec::H264);
        assert!(n.hardware_both);
    }

    #[test]
    fn no_common_codec_is_an_error() {
        let s = caps(&[(Codec::H265, 4096, 2304, 60, true)], &[]);
        let r = caps(&[(Codec::H264, 4096, 2304, 60, true)], &[]);
        let pref = Preference { codec_order: vec![Codec::H265, Codec::H264], ..Default::default() };
        assert_eq!(negotiate(&s, &r, &pref, 1920, 1080, 1).unwrap_err(), NegotiationError::NoCommonCodec);
    }
}
