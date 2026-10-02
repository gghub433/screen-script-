//! Sender and receiver sessions: the glue between the protocol, crypto, media
//! plane, adaptive engine and a [`Transport`](revizor_transport::Transport).
//!
//! Both sessions own their I/O threads. Platform code (MediaCodec, Media
//! Foundation, …) only exchanges *encoded frames* and *device signals* with
//! them, which is what keeps the encoder/decoder/renderer layers swappable.

mod clock;
mod events;
mod pairing_io;
mod receiver;
mod sender;
mod sync;

pub use clock::{Clock, MonotonicClock};
pub use events::{ReceiverEvent, ReconfigReason, SenderEvent, SenderState};
pub use pairing_io::{pair_as_sender, PairError};
pub use receiver::{AudioFrame, ReceiverConfig, ReceiverSession, ReceiverState, ReceiverStats};
pub use sender::{DeviceSignals, SenderConfig, SenderSession, SenderStats, SourceInfo};
pub use sync::ClockSync;
