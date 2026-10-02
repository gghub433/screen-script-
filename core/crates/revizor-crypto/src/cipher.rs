use crate::CryptoError;
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use revizor_proto::{Channel, PacketHeader, PacketKind, AEAD_TAG_LEN, HEADER_LEN};
use std::sync::atomic::{AtomicU64, Ordering};

/// Sliding window of 2048 sequence numbers (RFC 4303 style).
pub struct ReplayWindow {
    highest: u64,
    seen_any: bool,
    bits: [u64; 32],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self { highest: 0, seen_any: false, bits: [0; 32] }
    }
}

const WINDOW: u64 = 2048;

impl ReplayWindow {
    /// Read-only check; call [`commit`](Self::commit) only after authentication succeeded.
    pub fn check(&self, seq: u64) -> bool {
        if !self.seen_any {
            return true;
        }
        if seq > self.highest {
            return true;
        }
        let d = self.highest - seq;
        if d >= WINDOW {
            return false;
        }
        !self.bit(d)
    }

    pub fn commit(&mut self, seq: u64) {
        if !self.seen_any {
            self.seen_any = true;
            self.highest = seq;
            self.bits = [0; 32];
            self.set(0);
            return;
        }
        if seq > self.highest {
            let shift = seq - self.highest;
            self.shift(shift);
            self.highest = seq;
            self.set(0);
        } else {
            self.set(self.highest - seq);
        }
    }

    fn bit(&self, d: u64) -> bool {
        self.bits[(d / 64) as usize] >> (d % 64) & 1 == 1
    }
    fn set(&mut self, d: u64) {
        self.bits[(d / 64) as usize] |= 1 << (d % 64);
    }
    fn shift(&mut self, n: u64) {
        if n >= WINDOW {
            self.bits = [0; 32];
            return;
        }
        let words = (n / 64) as usize;
        let bits = (n % 64) as u32;
        for i in (0..32).rev() {
            let src = i as isize - words as isize;
            let mut v = if src >= 0 { self.bits[src as usize] << bits } else { 0 };
            if bits > 0 && src - 1 >= 0 {
                v |= self.bits[(src - 1) as usize] >> (64 - bits);
            }
            self.bits[i] = v;
        }
    }
}

fn nonce(salt: &[u8; 4], seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(salt);
    n[4..].copy_from_slice(&seq.to_le_bytes());
    n
}

/// Encrypts outgoing `Data` packets. Cheap to share between threads.
pub struct TxCipher {
    aead: ChaCha20Poly1305,
    salt: [u8; 4],
    session_id: u32,
    seq: AtomicU64,
}

impl TxCipher {
    pub fn new(key: &[u8; 32], salt: [u8; 4], session_id: u32) -> Self {
        Self { aead: ChaCha20Poly1305::new(Key::from_slice(key)), salt, session_id, seq: AtomicU64::new(1) }
    }

    /// Builds a full datagram: header | AEAD(channel | parts...) | tag.
    /// A single allocation, encrypted in place.
    pub fn seal(&self, channel: Channel, parts: &[&[u8]]) -> Vec<u8> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let body: usize = 1 + parts.iter().map(|p| p.len()).sum::<usize>();
        let mut out = Vec::with_capacity(HEADER_LEN + body + AEAD_TAG_LEN);
        let header = PacketHeader::new(PacketKind::Data, self.session_id, seq).encode();
        out.extend_from_slice(&header);
        out.push(channel as u8);
        for p in parts {
            out.extend_from_slice(p);
        }
        let tag = self
            .aead
            .encrypt_in_place_detached(Nonce::from_slice(&nonce(&self.salt, seq)), &header, &mut out[HEADER_LEN..])
            .expect("chacha20poly1305 encrypt cannot fail for in-memory buffers");
        out.extend_from_slice(&tag);
        out
    }
}

/// Decrypts incoming `Data` packets and enforces replay protection.
pub struct RxCipher {
    aead: ChaCha20Poly1305,
    salt: [u8; 4],
    session_id: u32,
    window: ReplayWindow,
}

pub struct Opened {
    pub seq: u64,
    pub channel: Channel,
    /// Plaintext after the channel byte.
    pub payload: Vec<u8>,
}

impl RxCipher {
    pub fn new(key: &[u8; 32], salt: [u8; 4], session_id: u32) -> Self {
        Self { aead: ChaCha20Poly1305::new(Key::from_slice(key)), salt, session_id, window: ReplayWindow::default() }
    }

    pub fn open(&mut self, datagram: &[u8]) -> Result<Opened, CryptoError> {
        let (h, body) = PacketHeader::decode(datagram).map_err(|_| CryptoError::Malformed)?;
        if h.kind != PacketKind::Data || h.session_id != self.session_id || body.len() < AEAD_TAG_LEN + 1 {
            return Err(CryptoError::Malformed);
        }
        if !self.window.check(h.seq) {
            return Err(CryptoError::Replay);
        }
        let (ct, tag) = body.split_at(body.len() - AEAD_TAG_LEN);
        let mut buf = ct.to_vec();
        self.aead
            .decrypt_in_place_detached(
                Nonce::from_slice(&nonce(&self.salt, h.seq)),
                &datagram[..HEADER_LEN],
                &mut buf,
                Tag::from_slice(tag),
            )
            .map_err(|_| CryptoError::Auth)?;
        self.window.commit(h.seq);
        let channel = Channel::from_u8(buf[0]).map_err(|_| CryptoError::Malformed)?;
        buf.remove(0); // channel byte; payloads are ≤ MTU so the shift is negligible
        Ok(Opened { seq: h.seq, channel, payload: buf })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (TxCipher, RxCipher) {
        (TxCipher::new(&[9; 32], [1, 2, 3, 4], 77), RxCipher::new(&[9; 32], [1, 2, 3, 4], 77))
    }

    #[test]
    fn roundtrip_multi_part() {
        let (tx, mut rx) = pair();
        let d = tx.seal(Channel::Video, &[b"head", b"tail"]);
        let o = rx.open(&d).unwrap();
        assert_eq!(o.channel, Channel::Video);
        assert_eq!(o.payload, b"headtail");
        assert_eq!(o.seq, 1);
    }

    #[test]
    fn rejects_replay_tamper_and_wrong_key() {
        let (tx, mut rx) = pair();
        let d = tx.seal(Channel::Control, &[b"x"]);
        rx.open(&d).unwrap();
        assert!(matches!(rx.open(&d), Err(CryptoError::Replay)));

        let mut t = tx.seal(Channel::Control, &[b"y"]);
        let n = t.len();
        t[n - 20] ^= 1;
        assert!(matches!(rx.open(&t), Err(CryptoError::Auth)));
        // header tamper (AAD)
        let mut t = tx.seal(Channel::Control, &[b"z"]);
        t[3] ^= 1;
        assert!(matches!(rx.open(&t), Err(CryptoError::Auth)));

        let mut other = RxCipher::new(&[8; 32], [1, 2, 3, 4], 77);
        assert!(other.open(&tx.seal(Channel::Control, &[b"w"])).is_err());
    }

    #[test]
    fn tampered_packet_does_not_poison_window() {
        let (tx, mut rx) = pair();
        let good = tx.seal(Channel::Video, &[b"a"]);
        let mut bad = good.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(rx.open(&bad).is_err());
        assert!(rx.open(&good).is_ok());
    }

    #[test]
    fn out_of_order_within_window_ok_but_old_rejected() {
        let (tx, mut rx) = pair();
        let pkts: Vec<_> = (0..3000).map(|_| tx.seal(Channel::Video, &[b"p"])).collect();
        rx.open(&pkts[2999]).unwrap();
        rx.open(&pkts[2998]).unwrap(); // reordered, in window
        assert!(matches!(rx.open(&pkts[2998]), Err(CryptoError::Replay)));
        assert!(matches!(rx.open(&pkts[10]), Err(CryptoError::Replay))); // beyond window
        rx.open(&pkts[2500]).unwrap();
    }

    #[test]
    fn window_shift_across_words() {
        let mut w = ReplayWindow::default();
        for s in [1u64, 5, 70, 200] {
            assert!(w.check(s));
            w.commit(s);
        }
        assert!(!w.check(70) && !w.check(5) && !w.check(200));
        assert!(w.check(100));
    }
}
