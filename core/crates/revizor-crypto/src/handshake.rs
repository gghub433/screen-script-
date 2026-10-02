//! Authenticated key exchange (SIGMA-like, 1.5 round trips + key confirmation).
//!
//! ```text
//! sender (Initiator)                              receiver (Responder)
//!   ClientHello{ver range, eph_c, nonce_c, id_c, caps_c}  ─────────►
//!                    ◄───── ServerHello{ver, session_id, eph_s, nonce_s, id_s, caps_s,
//!                                        Sig_s(H(CH‖SH))}
//!   (verify id_s ∈ trust store, verify signature)
//!   ClientFinish{Sig_c(H(CH‖SH‖"C"))}  ─────────────────────────────►
//!                                       (verify id_c ∈ trust store, verify signature)
//!                    ◄───── Accept{MAC_k(H)}   (key confirmation)
//! ```
//! Keys: HKDF-SHA256(salt = H(CH‖SH), ikm = X25519(eph_c, eph_s)) → two
//! directional ChaCha20-Poly1305 keys + nonce salts. Because ephemeral keys are
//! used, past sessions stay confidential if a long-term identity leaks.

use crate::cipher::{RxCipher, TxCipher};
use crate::identity::{verify, Identity, TrustStore};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use revizor_proto::wire::{Reader, Writer};
use revizor_proto::{Capabilities, ProtoError, PROTOCOL_MIN_VERSION, PROTOCOL_VERSION};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use x25519_dalek::{EphemeralSecret, PublicKey};

const T_CLIENT_HELLO: u8 = 1;
const T_SERVER_HELLO: u8 = 2;
const T_CLIENT_FINISH: u8 = 3;
const T_REJECT: u8 = 4;
const T_ACCEPT: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectReason {
    NotPaired = 1,
    Version = 2,
    Busy = 3,
    Malformed = 4,
}

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("peer is not paired / not trusted")]
    UnknownPeer,
    #[error("peer rejected the connection: {0:?}")]
    Rejected(u8),
    #[error("incompatible protocol version")]
    Version,
    #[error("bad signature or confirmation")]
    BadProof,
    #[error("malformed handshake message")]
    Malformed(#[from] ProtoError),
    #[error("unexpected message")]
    Unexpected,
}

pub struct Established {
    pub session_id: u32,
    pub version: u16,
    pub peer_identity: [u8; 32],
    pub peer_caps: Capabilities,
    pub tx: TxCipher,
    pub rx: RxCipher,
}

struct Keys {
    k_i2r: [u8; 32],
    k_r2i: [u8; 32],
    s_i2r: [u8; 4],
    s_r2i: [u8; 4],
    confirm: [u8; 32],
}

fn derive(shared: &[u8; 32], transcript: &[u8; 32]) -> Keys {
    let hk = Hkdf::<Sha256>::new(Some(transcript), shared);
    let mut okm = [0u8; 32 + 32 + 4 + 4 + 32];
    hk.expand(b"revizor v1 session keys", &mut okm).expect("hkdf length");
    Keys {
        k_i2r: okm[0..32].try_into().unwrap(),
        k_r2i: okm[32..64].try_into().unwrap(),
        s_i2r: okm[64..68].try_into().unwrap(),
        s_r2i: okm[68..72].try_into().unwrap(),
        confirm: okm[72..104].try_into().unwrap(),
    }
}

fn confirm_tag(key: &[u8; 32], transcript: &[u8; 32]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).unwrap();
    m.update(b"revizor accept");
    m.update(transcript);
    m.finalize().into_bytes().into()
}

fn sig_input(label: &[u8], transcript: &[u8; 32]) -> Vec<u8> {
    let mut v = label.to_vec();
    v.extend_from_slice(transcript);
    v
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn reject_message(reason: RejectReason) -> Vec<u8> {
    vec![T_REJECT, reason as u8]
}

// ───────────────────────────── Initiator (sender) ─────────────────────────────

pub struct Initiator {
    identity: Arc<Identity>,
    trust: Arc<dyn TrustStore>,
    secret: Option<EphemeralSecret>,
    hello: Vec<u8>,
    state: IState,
}

enum IState {
    SentHello,
    SentFinish { keys: Keys, transcript: [u8; 32], session_id: u32, version: u16, peer_id: [u8; 32], peer_caps: Capabilities },
    Done,
}

impl Initiator {
    /// Returns the initiator and the `ClientHello` body to send.
    pub fn start(identity: Arc<Identity>, caps: &Capabilities, trust: Arc<dyn TrustStore>) -> (Self, Vec<u8>) {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let eph = PublicKey::from(&secret);
        let mut nonce = [0u8; 16];
        OsRng.fill_bytes(&mut nonce);
        let mut w = Writer::new();
        w.u8(T_CLIENT_HELLO).u16(PROTOCOL_MIN_VERSION).u16(PROTOCOL_VERSION).raw(eph.as_bytes()).raw(&nonce).raw(&identity.public());
        caps.encode(&mut w);
        let hello = w.finish();
        (Self { identity, trust, secret: Some(secret), hello: hello.clone(), state: IState::SentHello }, hello)
    }

    /// Consumes `ServerHello`, returns `ClientFinish`.
    pub fn on_server_hello(&mut self, msg: &[u8]) -> Result<Vec<u8>, HandshakeError> {
        if !matches!(self.state, IState::SentHello) {
            return Err(HandshakeError::Unexpected);
        }
        let mut r = Reader::new(msg);
        match r.u8()? {
            T_REJECT => {
                let why = r.u8()?;
                return Err(if why == RejectReason::Version as u8 {
                    HandshakeError::Version
                } else if why == RejectReason::NotPaired as u8 {
                    HandshakeError::UnknownPeer
                } else {
                    HandshakeError::Rejected(why)
                });
            }
            T_SERVER_HELLO => {}
            _ => return Err(HandshakeError::Unexpected),
        }
        let version = r.u16()?;
        if !(PROTOCOL_MIN_VERSION..=PROTOCOL_VERSION).contains(&version) {
            return Err(HandshakeError::Version);
        }
        let session_id = r.u32()?;
        let eph_s: [u8; 32] = r.array()?;
        let _nonce_s: [u8; 16] = r.array()?;
        let peer_id: [u8; 32] = r.array()?;
        let peer_caps = Capabilities::decode(&mut r)?;
        let sig_off = msg.len() - r.remaining();
        let sig: [u8; 64] = r.array()?;

        // Identity must already be pinned: discovery is never trusted.
        if !self.trust.is_trusted(&peer_id) {
            return Err(HandshakeError::UnknownPeer);
        }
        let transcript = hash(&[&self.hello, &msg[..sig_off]]);
        if !verify(&peer_id, &sig_input(b"RVZ1-S", &transcript), &sig) {
            return Err(HandshakeError::BadProof);
        }

        let secret = self.secret.take().ok_or(HandshakeError::Unexpected)?;
        let shared = secret.diffie_hellman(&PublicKey::from(eph_s));
        let keys = derive(shared.as_bytes(), &transcript);

        let my_sig = self.identity.sign(&sig_input(b"RVZ1-C", &transcript));
        let mut w = Writer::new();
        w.u8(T_CLIENT_FINISH).raw(&my_sig);
        self.state = IState::SentFinish { keys, transcript, session_id, version, peer_id, peer_caps };
        Ok(w.finish())
    }

    /// Consumes `Accept`; the session is established.
    pub fn on_accept(&mut self, msg: &[u8]) -> Result<Established, HandshakeError> {
        let IState::SentFinish { keys, transcript, session_id, version, peer_id, peer_caps } =
            std::mem::replace(&mut self.state, IState::Done)
        else {
            return Err(HandshakeError::Unexpected);
        };
        let mut r = Reader::new(msg);
        match r.u8()? {
            T_ACCEPT => {}
            T_REJECT => return Err(HandshakeError::Rejected(r.u8()?)),
            _ => return Err(HandshakeError::Unexpected),
        }
        let tag: [u8; 32] = r.array()?;
        if tag.ct_eq(&confirm_tag(&keys.confirm, &transcript)).unwrap_u8() != 1 {
            return Err(HandshakeError::BadProof);
        }
        Ok(Established {
            session_id,
            version,
            peer_identity: peer_id,
            peer_caps,
            tx: TxCipher::new(&keys.k_i2r, keys.s_i2r, session_id),
            rx: RxCipher::new(&keys.k_r2i, keys.s_r2i, session_id),
        })
    }
}

// ───────────────────────────── Responder (receiver) ─────────────────────────────

pub struct Responder {
    identity: Arc<Identity>,
    caps: Arc<Capabilities>,
    trust: Arc<dyn TrustStore>,
    state: RState,
}

enum RState {
    Idle,
    SentHello { keys: Keys, transcript: [u8; 32], session_id: u32, version: u16, peer_id: [u8; 32], peer_caps: Capabilities },
}

impl Responder {
    pub fn new(identity: Arc<Identity>, caps: Arc<Capabilities>, trust: Arc<dyn TrustStore>) -> Self {
        Self { identity, caps, trust, state: RState::Idle }
    }

    /// Returns the reply: `ServerHello` or `Reject`. On `Err` the caller should
    /// send the returned reject bytes (see [`HandshakeError`] mapping in `reject_for`).
    pub fn on_client_hello(&mut self, msg: &[u8]) -> Result<Vec<u8>, (HandshakeError, Vec<u8>)> {
        let bad = |e: HandshakeError, why| (e, reject_message(why));
        let mut r = Reader::new(msg);
        let parse = (|| -> Result<_, ProtoError> {
            if r.u8()? != T_CLIENT_HELLO {
                return Err(ProtoError::Invalid("not a client hello"));
            }
            let vmin = r.u16()?;
            let vmax = r.u16()?;
            let eph_c: [u8; 32] = r.array()?;
            let _nonce: [u8; 16] = r.array()?;
            let id_c: [u8; 32] = r.array()?;
            let caps = Capabilities::decode(&mut r)?;
            Ok((vmin, vmax, eph_c, id_c, caps))
        })();
        let (vmin, vmax, eph_c, id_c, peer_caps) =
            parse.map_err(|e| bad(HandshakeError::Malformed(e), RejectReason::Malformed))?;

        let version = vmax.min(PROTOCOL_VERSION);
        if version < vmin.max(PROTOCOL_MIN_VERSION) {
            return Err(bad(HandshakeError::Version, RejectReason::Version));
        }
        if !self.trust.is_trusted(&id_c) {
            return Err(bad(HandshakeError::UnknownPeer, RejectReason::NotPaired));
        }

        let secret = EphemeralSecret::random_from_rng(OsRng);
        let eph_s = PublicKey::from(&secret);
        let mut nonce_s = [0u8; 16];
        OsRng.fill_bytes(&mut nonce_s);
        let mut session_id = OsRng.next_u32();
        if session_id == 0 {
            session_id = 1;
        }

        let mut w = Writer::new();
        w.u8(T_SERVER_HELLO).u16(version).u32(session_id).raw(eph_s.as_bytes()).raw(&nonce_s).raw(&self.identity.public());
        self.caps.encode(&mut w);
        let body = w.finish();
        let transcript = hash(&[msg, &body]);
        let sig = self.identity.sign(&sig_input(b"RVZ1-S", &transcript));
        let mut out = body;
        out.extend_from_slice(&sig);

        // The signature is not part of the transcript hash input above (matches Initiator).
        let shared = secret.diffie_hellman(&PublicKey::from(eph_c));
        let keys = derive(shared.as_bytes(), &transcript);
        self.state = RState::SentHello { keys, transcript, session_id, version, peer_id: id_c, peer_caps };
        Ok(out)
    }

    /// Verifies `ClientFinish`; returns the `Accept` message and the session.
    pub fn on_client_finish(&mut self, msg: &[u8]) -> Result<(Vec<u8>, Established), HandshakeError> {
        let RState::SentHello { keys, transcript, session_id, version, peer_id, peer_caps } =
            std::mem::replace(&mut self.state, RState::Idle)
        else {
            return Err(HandshakeError::Unexpected);
        };
        let mut r = Reader::new(msg);
        if r.u8()? != T_CLIENT_FINISH {
            return Err(HandshakeError::Unexpected);
        }
        let sig: [u8; 64] = r.array()?;
        if !verify(&peer_id, &sig_input(b"RVZ1-C", &transcript), &sig) {
            return Err(HandshakeError::BadProof);
        }
        let mut w = Writer::new();
        w.u8(T_ACCEPT).raw(&confirm_tag(&keys.confirm, &transcript));
        let est = Established {
            session_id,
            version,
            peer_identity: peer_id,
            peer_caps,
            tx: TxCipher::new(&keys.k_r2i, keys.s_r2i, session_id),
            rx: RxCipher::new(&keys.k_i2r, keys.s_i2r, session_id),
        };
        Ok((w.finish(), est))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{MemoryTrustStore, TrustedDevice};
    use revizor_proto::{Channel, Codec, CodecCap, Platform, TransportKind};

    fn caps(name: &str) -> Capabilities {
        Capabilities {
            device_name: name.into(),
            platform: Platform::Linux,
            codecs: vec![CodecCap { codec: Codec::H264, max_width: 1920, max_height: 1080, max_fps: 60, hardware: true }],
            audio: vec![],
            transports: TransportKind::Udp.bit(),
            hdr: false,
            max_bitrate_bps: 50_000_000,
        }
    }

    struct World {
        a: Arc<Identity>,
        b: Arc<Identity>,
        ca: Arc<Capabilities>,
        cb: Arc<Capabilities>,
        ta: Arc<MemoryTrustStore>,
        tb: Arc<MemoryTrustStore>,
    }

    fn world(a_trusts_b: bool, b_trusts_a: bool) -> World {
        let w = World {
            a: Arc::new(Identity::generate()),
            b: Arc::new(Identity::generate()),
            ca: Arc::new(caps("sender")),
            cb: Arc::new(caps("receiver")),
            ta: Arc::new(MemoryTrustStore::new()),
            tb: Arc::new(MemoryTrustStore::new()),
        };
        if a_trusts_b {
            w.ta.add(TrustedDevice { public: w.b.public(), name: "b".into() });
        }
        if b_trusts_a {
            w.tb.add(TrustedDevice { public: w.a.public(), name: "a".into() });
        }
        w
    }

    fn run(w: &World) -> Result<(Established, Established), HandshakeError> {
        let (mut i, ch) = Initiator::start(w.a.clone(), &w.ca, w.ta.clone());
        let mut r = Responder::new(w.b.clone(), w.cb.clone(), w.tb.clone());
        let sh = r.on_client_hello(&ch).map_err(|(e, _)| e)?;
        let cf = i.on_server_hello(&sh)?;
        let (acc, est_r) = r.on_client_finish(&cf)?;
        let est_i = i.on_accept(&acc)?;
        Ok((est_i, est_r))
    }

    #[test]
    fn full_handshake_and_traffic_both_directions() {
        let w = world(true, true);
        let (mut i, mut r) = run(&w).unwrap();
        assert_eq!(i.session_id, r.session_id);
        assert_eq!(i.peer_identity, w.b.public());
        assert_eq!(r.peer_caps.device_name, "sender");
        let d = i.tx.seal(Channel::Video, &[b"frame"]);
        assert_eq!(r.rx.open(&d).unwrap().payload, b"frame");
        let d = r.tx.seal(Channel::Control, &[b"ack"]);
        assert_eq!(i.rx.open(&d).unwrap().payload, b"ack");
        // direction separation: a reflected packet must not decrypt
        let d = i.tx.seal(Channel::Control, &[b"x"]);
        assert!(i.rx.open(&d).is_err());
    }

    #[test]
    fn unpaired_receiver_rejects_sender() {
        let w = world(true, false);
        assert!(matches!(run(&w), Err(HandshakeError::UnknownPeer)));
    }

    #[test]
    fn sender_refuses_unpinned_receiver() {
        let w = world(false, true);
        assert!(matches!(run(&w), Err(HandshakeError::UnknownPeer)));
    }

    #[test]
    fn mitm_with_other_identity_is_detected() {
        // Sender pinned B, but an attacker M answers with its own identity.
        let w = world(true, true);
        let m = Arc::new(Identity::generate());
        let mt = Arc::new(MemoryTrustStore::new());
        mt.add(TrustedDevice { public: w.a.public(), name: "a".into() });
        let (mut i, ch) = Initiator::start(w.a.clone(), &w.ca, w.ta.clone());
        let mut attacker = Responder::new(m.clone(), w.cb.clone(), mt.clone());
        let sh = attacker.on_client_hello(&ch).map_err(|e| e.0).unwrap();
        assert!(matches!(i.on_server_hello(&sh), Err(HandshakeError::UnknownPeer)));
    }

    #[test]
    fn tampered_server_hello_fails_signature() {
        let w = world(true, true);
        let (mut i, ch) = Initiator::start(w.a.clone(), &w.ca, w.ta.clone());
        let mut r = Responder::new(w.b.clone(), w.cb.clone(), w.tb.clone());
        let mut sh = r.on_client_hello(&ch).map_err(|e| e.0).unwrap();
        sh[10] ^= 0x40; // flip a bit in the ephemeral key
        assert!(matches!(i.on_server_hello(&sh), Err(HandshakeError::BadProof)));
    }

    #[test]
    fn forged_client_finish_fails() {
        let w = world(true, true);
        let (mut i, ch) = Initiator::start(w.a.clone(), &w.ca, w.ta.clone());
        let mut r = Responder::new(w.b.clone(), w.cb.clone(), w.tb.clone());
        let sh = r.on_client_hello(&ch).map_err(|e| e.0).unwrap();
        let mut cf = i.on_server_hello(&sh).unwrap();
        cf[5] ^= 1;
        assert!(matches!(r.on_client_finish(&cf), Err(HandshakeError::BadProof)));
    }

    #[test]
    fn version_mismatch_reported() {
        let w = world(true, true);
        let (_, mut ch) = Initiator::start(w.a.clone(), &w.ca, w.ta.clone());
        // claim only versions 50..60
        ch[1..3].copy_from_slice(&50u16.to_le_bytes());
        ch[3..5].copy_from_slice(&60u16.to_le_bytes());
        let mut r = Responder::new(w.b.clone(), w.cb.clone(), w.tb.clone());
        let e = r.on_client_hello(&ch).unwrap_err();
        assert!(matches!(e.0, HandshakeError::Version));
        let (mut i2, _) = Initiator::start(w.a.clone(), &w.ca, w.ta.clone());
        assert!(matches!(i2.on_server_hello(&e.1), Err(HandshakeError::Version)));
    }

    #[test]
    fn sessions_have_distinct_keys() {
        let w = world(true, true);
        let (i1, _) = run(&w).unwrap();
        let (_, mut r2) = run(&w).unwrap();
        // packet from session 1 must not open in session 2
        assert!(r2.rx.open(&i1.tx.seal(Channel::Video, &[b"x"])).is_err());
    }
}
