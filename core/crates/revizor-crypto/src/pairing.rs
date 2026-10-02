//! First-time pairing with a short PIN shown on the receiver.
//!
//! Uses SPAKE2 (balanced PAKE): both sides derive a strong shared key from the
//! PIN without revealing it, so a passive or active attacker gets at most one
//! guess per run and cannot brute-force the PIN offline. After the key is
//! agreed each side proves possession of it (HMAC) while exchanging its
//! long-term identity key, which is then stored in the trust store.
//!
//! ```text
//! sender                                    receiver (shows PIN)
//!  P1: spake_a ───────────────────────────►
//!      ◄─────────── P2: spake_b, id_r, name_r, MAC_K("R"‖A‖B‖id_r‖name_r)
//!  (verify MAC → store receiver)
//!  P3: id_s, name_s, MAC_K("S"‖A‖B‖id_s‖name_s) ──►  (verify MAC → store sender)
//!      ◄─────────── P4: ok
//! ```

use crate::identity::{Identity, TrustStore, TrustedDevice};
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::Rng;
use revizor_proto::wire::{Reader, Writer};
use revizor_proto::ProtoError;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SId, Password, Spake2};
use std::sync::Arc;
use subtle::ConstantTimeEq;

const T_P1: u8 = 1;
const T_P2: u8 = 2;
const T_P3: u8 = 3;
const T_P4: u8 = 4;

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("wrong PIN or tampered pairing exchange")]
    BadPin,
    #[error("unexpected pairing message")]
    Unexpected,
    #[error("malformed pairing message")]
    Malformed(#[from] ProtoError),
}

/// Uniformly random 6-digit PIN.
pub fn generate_pin() -> String {
    format!("{:06}", OsRng.gen_range(0..1_000_000u32))
}

fn mac(key: &[u8], role: &[u8], a: &[u8], b: &[u8], id: &[u8; 32], name: &str) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).unwrap();
    m.update(b"revizor pair");
    m.update(role);
    m.update(a);
    m.update(b);
    m.update(id);
    m.update(name.as_bytes());
    m.finalize().into_bytes().into()
}

fn spake(pin: &str, initiator: bool) -> (Spake2<Ed25519Group>, Vec<u8>) {
    let pw = Password::new(pin.as_bytes());
    let (a, b) = (SId::new(b"revizor-sender"), SId::new(b"revizor-receiver"));
    if initiator {
        Spake2::<Ed25519Group>::start_a(&pw, &a, &b)
    } else {
        Spake2::<Ed25519Group>::start_b(&pw, &a, &b)
    }
}

// ─── sender side ───

pub struct PairInitiator {
    identity: Arc<Identity>,
    name: String,
    trust: Arc<dyn TrustStore>,
    state: Option<(Spake2<Ed25519Group>, Vec<u8>)>,
    key: Option<Vec<u8>>,
    a: Vec<u8>,
    b: Vec<u8>,
}

impl PairInitiator {
    pub fn start(identity: Arc<Identity>, name: &str, pin: &str, trust: Arc<dyn TrustStore>) -> (Self, Vec<u8>) {
        let (st, msg) = spake(pin, true);
        let mut w = Writer::new();
        w.u8(T_P1).lp(&msg);
        (
            Self { identity, name: name.into(), trust, state: Some((st, msg.clone())), key: None, a: msg, b: vec![] },
            w.finish(),
        )
    }

    /// Handles P2, returns P3. The receiver's identity is stored only after its MAC verified.
    pub fn on_p2(&mut self, msg: &[u8]) -> Result<Vec<u8>, PairingError> {
        let (st, a) = self.state.take().ok_or(PairingError::Unexpected)?;
        let mut r = Reader::new(msg);
        if r.u8()? != T_P2 {
            return Err(PairingError::Unexpected);
        }
        let b = r.lp()?.to_vec();
        let id_r: [u8; 32] = r.array()?;
        let name_r = r.str()?;
        let tag: [u8; 32] = r.array()?;
        let key = st.finish(&b).map_err(|_| PairingError::BadPin)?;
        if tag.ct_eq(&mac(&key, b"R", &a, &b, &id_r, &name_r)).unwrap_u8() != 1 {
            return Err(PairingError::BadPin);
        }
        self.trust.add(TrustedDevice { public: id_r, name: name_r });
        let id_s = self.identity.public();
        let mut w = Writer::new();
        w.u8(T_P3).raw(&id_s).str(&self.name).raw(&mac(&key, b"S", &a, &b, &id_s, &self.name));
        self.key = Some(key);
        self.a = a;
        self.b = b;
        Ok(w.finish())
    }

    pub fn on_p4(&self, msg: &[u8]) -> Result<(), PairingError> {
        if msg.first() == Some(&T_P4) && msg.get(1) == Some(&1) {
            Ok(())
        } else {
            Err(PairingError::BadPin)
        }
    }
}

// ─── receiver side ───

pub struct PairResponder {
    identity: Arc<Identity>,
    name: String,
    trust: Arc<dyn TrustStore>,
    pin: String,
    state: Option<(Spake2<Ed25519Group>, Vec<u8>, Vec<u8>)>,
    key: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
}

impl PairResponder {
    pub fn new(identity: Arc<Identity>, name: &str, pin: &str, trust: Arc<dyn TrustStore>) -> Self {
        Self { identity, name: name.into(), trust, pin: pin.into(), state: None, key: None }
    }

    /// Handles P1, returns P2.
    pub fn on_p1(&mut self, msg: &[u8]) -> Result<Vec<u8>, PairingError> {
        let mut r = Reader::new(msg);
        if r.u8()? != T_P1 {
            return Err(PairingError::Unexpected);
        }
        let a = r.lp()?.to_vec();
        let (st, b) = spake(&self.pin, false);
        let key = st.finish(&a).map_err(|_| PairingError::BadPin)?;
        let id_r = self.identity.public();
        let mut w = Writer::new();
        w.u8(T_P2).lp(&b).raw(&id_r).str(&self.name).raw(&mac(&key, b"R", &a, &b, &id_r, &self.name));
        self.key = Some((key, a, b));
        self.state = None;
        Ok(w.finish())
    }

    /// Handles P3; on success the sender is stored in the trust store and P4 returned.
    pub fn on_p3(&mut self, msg: &[u8]) -> Result<(Vec<u8>, TrustedDevice), PairingError> {
        let (key, a, b) = self.key.take().ok_or(PairingError::Unexpected)?;
        let mut r = Reader::new(msg);
        if r.u8()? != T_P3 {
            return Err(PairingError::Unexpected);
        }
        let id_s: [u8; 32] = r.array()?;
        let name_s = r.str()?;
        let tag: [u8; 32] = r.array()?;
        if tag.ct_eq(&mac(&key, b"S", &a, &b, &id_s, &name_s)).unwrap_u8() != 1 {
            return Err(PairingError::BadPin);
        }
        let dev = TrustedDevice { public: id_s, name: name_s };
        self.trust.add(dev.clone());
        Ok((vec![T_P4, 1], dev))
    }
}

/// Failure counter for the receiver: after `max` bad attempts the PIN is
/// invalidated and a new one must be generated, bounding online guessing to
/// `max / 10^6` per displayed PIN.
pub struct PairingGuard {
    failures: u8,
    max: u8,
}

impl PairingGuard {
    pub fn new(max: u8) -> Self {
        Self { failures: 0, max }
    }
    pub fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }
    pub fn exhausted(&self) -> bool {
        self.failures >= self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::MemoryTrustStore;

    #[test]
    fn pin_format() {
        for _ in 0..50 {
            let p = generate_pin();
            assert_eq!(p.len(), 6);
            assert!(p.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    fn run(pin_s: &str, pin_r: &str) -> (Result<(), PairingError>, Arc<MemoryTrustStore>, Arc<MemoryTrustStore>, Arc<Identity>, Arc<Identity>) {
        let (ids, idr) = (Arc::new(Identity::generate()), Arc::new(Identity::generate()));
        let (ts, tr) = (Arc::new(MemoryTrustStore::new()), Arc::new(MemoryTrustStore::new()));
        let res = (|| {
            let (mut i, p1) = PairInitiator::start(ids.clone(), "Phone", pin_s, ts.clone());
            let mut r = PairResponder::new(idr.clone(), "TV", pin_r, tr.clone());
            let p2 = r.on_p1(&p1)?;
            let p3 = i.on_p2(&p2)?;
            let (p4, _) = r.on_p3(&p3)?;
            i.on_p4(&p4)
        })();
        (res, ts, tr, ids, idr)
    }

    #[test]
    fn correct_pin_pairs_both_ways() {
        let (res, ts, tr, ids, idr) = run("123456", "123456");
        res.unwrap();
        assert!(ts.is_trusted(&idr.public()));
        assert!(tr.is_trusted(&ids.public()));
    }

    #[test]
    fn wrong_pin_pairs_nobody() {
        let (res, ts, tr, ids, idr) = run("123456", "654321");
        assert!(matches!(res, Err(PairingError::BadPin)));
        assert!(!ts.is_trusted(&idr.public()));
        assert!(!tr.is_trusted(&ids.public()));
    }

    #[test]
    fn guard_limits_attempts() {
        let mut g = PairingGuard::new(3);
        for _ in 0..2 {
            g.record_failure();
        }
        assert!(!g.exhausted());
        g.record_failure();
        assert!(g.exhausted());
    }
}
