//! Revizor security layer.
//!
//! * [`identity`] — long-term Ed25519 device identity and trust store.
//! * [`pairing`]  — first-time pairing with a short PIN using SPAKE2 (a PAKE:
//!   an attacker gets one PIN guess per protocol run, offline brute force is
//!   impossible even for a 6-digit PIN).
//! * [`handshake`] — per-session authenticated key exchange (ephemeral X25519,
//!   Ed25519 signatures over the transcript, HKDF key schedule).
//! * [`cipher`]   — ChaCha20-Poly1305 packet protection with replay window.
//!
//! ChaCha20-Poly1305 is chosen over AES-GCM because it is fast on every phone
//! SoC without relying on AES instructions, keeping CPU load/heat low.

pub mod cipher;
pub mod handshake;
pub mod identity;
pub mod pairing;

pub use cipher::{RxCipher, TxCipher};
pub use handshake::{Established, HandshakeError, Initiator, Responder};
pub use identity::{device_id, FileTrustStore, Identity, MemoryTrustStore, TrustStore, TrustedDevice};

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("authentication failed")]
    Auth,
    #[error("replayed or too-old packet")]
    Replay,
    #[error("malformed packet")]
    Malformed,
}
