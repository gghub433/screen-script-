//! Media plane: packetization, FEC, retransmission history, pacing, frame
//! reassembly with loss recovery, and real network statistics.
//!
//! Pure logic: no sockets, no clocks (callers pass `now_us`), so every
//! behaviour is deterministic and unit-testable.

pub mod assembler;
pub mod history;
pub mod pacer;
pub mod packetizer;
pub mod stats;

pub use assembler::{Assembler, AssemblerConfig, AssemblerOutput, ReceivedFrame};
pub use history::SendHistory;
pub use pacer::Pacer;
pub use packetizer::{EncodedFrame, PacketizedFrame};
pub use stats::{JitterEstimator, LossTracker, RateMeter};

use revizor_proto::{AEAD_TAG_LEN, HEADER_LEN, MAX_DATAGRAM, MEDIA_HEADER_LEN};

/// Media bytes that fit one datagram after the envelope, AEAD tag and media header.
pub const MAX_MEDIA_PAYLOAD: usize = MAX_DATAGRAM - HEADER_LEN - 1 - AEAD_TAG_LEN - MEDIA_HEADER_LEN;

/// Wrap-aware comparison of u16 epochs / u32 frame ids: `a` is newer than `b`.
pub fn newer_u32(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}
pub fn newer_u16(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}
