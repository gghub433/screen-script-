//! Control channel messages (encrypted, `Channel::Control`).

use crate::caps::StreamParams;
use crate::wire::{Reader, Writer};
use crate::ProtoError;

pub const UNKNOWN_U8: u8 = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyframeReason {
    StreamStart = 1,
    PacketLoss = 2,
    DecoderError = 3,
    Reconnect = 4,
    ConfigChange = 5,
}

impl KeyframeReason {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::StreamStart,
            3 => Self::DecoderError,
            4 => Self::Reconnect,
            5 => Self::ConfigChange,
            _ => Self::PacketLoss,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ByeReason {
    UserStopped = 1,
    Error = 2,
    Unauthorized = 3,
    IncompatibleVersion = 4,
}

impl ByeReason {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::UserStopped,
            3 => Self::Unauthorized,
            4 => Self::IncompatibleVersion,
            _ => Self::Error,
        }
    }
}

/// Periodic receiver → sender feedback. Every field is a real measurement;
/// fields the platform cannot measure are `UNKNOWN_*` and must be shown as "n/a".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReceiverReport {
    /// Length of the measurement interval.
    pub interval_ms: u32,
    /// Distinct data+parity packets the sequence numbers say were sent.
    pub packets_expected: u32,
    /// Packets that never arrived (before FEC/retransmit recovery).
    pub packets_lost: u32,
    /// Packets rebuilt by FEC or retransmission.
    pub packets_recovered: u32,
    /// RFC 3550 interarrival jitter estimate, µs.
    pub jitter_us: u32,
    /// Application bytes received per second over the interval.
    pub recv_bitrate_bps: u32,
    pub frames_complete: u32,
    /// Frames abandoned (incomplete past deadline) or dropped by the decoder queue.
    pub frames_dropped: u32,
    /// Mean decode time in microseconds (0 = no frames decoded).
    pub decode_us: u32,
    /// Capture→presentation latency in µs measured with clock sync (0 = unknown).
    pub e2e_latency_us: u32,
    pub highest_frame_id: u32,
    /// Receiver CPU load percent or 255.
    pub cpu_pct: u8,
    /// Android thermal status 0..6 or 255.
    pub thermal: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Sender → receiver: new stream parameters, effective from the next keyframe of `epoch`.
    Params(StreamParams),
    /// Receiver → sender.
    KeyframeRequest { epoch: u16, reason: KeyframeReason },
    /// Receiver → sender: retransmit these packet indexes of a frame
    /// (`idx < pkt_count` data, otherwise not valid; parity is never NACKed).
    Nack { epoch: u16, frame_id: u32, missing: Vec<u16> },
    Ping { id: u32, t0_us: u64 },
    /// `t1` = receiver clock when Ping arrived, `t2` = receiver clock when Pong left.
    Pong { id: u32, t0_us: u64, t1_us: u64, t2_us: u64 },
    Report(ReceiverReport),
    Bye(ByeReason),
    /// Receiver → sender: media with an unknown epoch arrived, resend `Params`.
    ParamsRequest,
}

impl Control {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Control::Params(p) => {
                w.u8(1);
                p.encode(&mut w);
            }
            Control::KeyframeRequest { epoch, reason } => {
                w.u8(2).u16(*epoch).u8(*reason as u8);
            }
            Control::Nack { epoch, frame_id, missing } => {
                w.u8(3).u16(*epoch).u32(*frame_id);
                let n = missing.len().min(255);
                w.u8(n as u8);
                for m in &missing[..n] {
                    w.u16(*m);
                }
            }
            Control::Ping { id, t0_us } => {
                w.u8(4).u32(*id).u64(*t0_us);
            }
            Control::Pong { id, t0_us, t1_us, t2_us } => {
                w.u8(5).u32(*id).u64(*t0_us).u64(*t1_us).u64(*t2_us);
            }
            Control::Report(r) => {
                w.u8(6)
                    .u32(r.interval_ms)
                    .u32(r.packets_expected)
                    .u32(r.packets_lost)
                    .u32(r.packets_recovered)
                    .u32(r.jitter_us)
                    .u32(r.recv_bitrate_bps)
                    .u32(r.frames_complete)
                    .u32(r.frames_dropped)
                    .u32(r.decode_us)
                    .u32(r.e2e_latency_us)
                    .u32(r.highest_frame_id)
                    .u8(r.cpu_pct)
                    .u8(r.thermal);
            }
            Control::Bye(b) => {
                w.u8(7).u8(*b as u8);
            }
            Control::ParamsRequest => {
                w.u8(8);
            }
        }
        w.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ProtoError> {
        let mut r = Reader::new(b);
        Ok(match r.u8()? {
            1 => Control::Params(StreamParams::decode(&mut r)?),
            2 => Control::KeyframeRequest { epoch: r.u16()?, reason: KeyframeReason::from_u8(r.u8()?) },
            3 => {
                let epoch = r.u16()?;
                let frame_id = r.u32()?;
                let n = r.u8()? as usize;
                let mut missing = Vec::with_capacity(n);
                for _ in 0..n {
                    missing.push(r.u16()?);
                }
                Control::Nack { epoch, frame_id, missing }
            }
            4 => Control::Ping { id: r.u32()?, t0_us: r.u64()? },
            5 => Control::Pong { id: r.u32()?, t0_us: r.u64()?, t1_us: r.u64()?, t2_us: r.u64()? },
            6 => Control::Report(ReceiverReport {
                interval_ms: r.u32()?,
                packets_expected: r.u32()?,
                packets_lost: r.u32()?,
                packets_recovered: r.u32()?,
                jitter_us: r.u32()?,
                recv_bitrate_bps: r.u32()?,
                frames_complete: r.u32()?,
                frames_dropped: r.u32()?,
                decode_us: r.u32()?,
                e2e_latency_us: r.u32()?,
                highest_frame_id: r.u32()?,
                cpu_pct: r.u8()?,
                thermal: r.u8()?,
            }),
            7 => Control::Bye(ByeReason::from_u8(r.u8()?)),
            8 => Control::ParamsRequest,
            _ => return Err(ProtoError::Invalid("control type")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::*;

    fn rt(c: Control) {
        assert_eq!(Control::decode(&c.encode()).unwrap(), c);
    }

    #[test]
    fn all_roundtrip() {
        rt(Control::Params(StreamParams {
            epoch: 2,
            video: VideoParams { codec: Codec::H264, width: 1920, height: 1080, fps: 60, bitrate_bps: 15_000_000, keyframe_interval_ms: 0 },
            audio: AudioParams::NONE,
        }));
        rt(Control::KeyframeRequest { epoch: 4, reason: KeyframeReason::DecoderError });
        rt(Control::Nack { epoch: 1, frame_id: 10, missing: vec![1, 5, 9] });
        rt(Control::Ping { id: 1, t0_us: 5 });
        rt(Control::Pong { id: 1, t0_us: 5, t1_us: 6, t2_us: 7 });
        rt(Control::Report(ReceiverReport { interval_ms: 500, packets_expected: 100, jitter_us: 33, thermal: 255, cpu_pct: 255, ..Default::default() }));
        rt(Control::Bye(ByeReason::UserStopped));
        rt(Control::ParamsRequest);
    }

    #[test]
    fn truncated_is_error() {
        let b = Control::Ping { id: 1, t0_us: 5 }.encode();
        assert!(Control::decode(&b[..b.len() - 1]).is_err());
        assert!(Control::decode(&[99]).is_err());
    }
}
