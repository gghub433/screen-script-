//! Revizor wire protocol, version 1.
//!
//! Everything that crosses the network or the JNI boundary is defined here:
//! packet envelope, media headers, control messages, capabilities, negotiation
//! and the discovery announcement. No I/O, no crypto, no platform code.

pub mod caps;
pub mod control;
pub mod discovery;
pub mod geometry;
pub mod media;
pub mod packet;
pub mod wire;

pub use caps::*;
pub use control::*;
pub use media::*;
pub use packet::*;

/// Protocol version spoken by this build.
pub const PROTOCOL_VERSION: u16 = 1;
/// Oldest protocol version this build can interoperate with.
pub const PROTOCOL_MIN_VERSION: u16 = 1;

#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum ProtoError {
    #[error("message truncated")]
    Truncated,
    #[error("invalid field: {0}")]
    Invalid(&'static str),
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
}
