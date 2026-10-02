use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Mutex;

/// Hex of the first 16 bytes of SHA-256(public key). Stable, display-friendly.
pub fn device_id(public: &[u8; 32]) -> String {
    let h = Sha256::digest(public);
    hex(&h[..16])
}

pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Long-term device identity. The 32-byte secret must be stored in the
/// platform keystore (Android Keystore-wrapped, Windows DPAPI) by the app.
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        Self { signing: SigningKey::generate(&mut OsRng) }
    }
    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self { signing: SigningKey::from_bytes(&secret) }
    }
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }
    pub fn public(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }
    pub fn device_id(&self) -> String {
        device_id(&self.public())
    }
    pub(crate) fn sign(&self, msg: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        self.signing.sign(msg).to_bytes()
    }
}

pub(crate) fn verify(public: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(public) else { return false };
    let sig = ed25519_dalek::Signature::from_bytes(sig);
    vk.verify_strict(msg, &sig).is_ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedDevice {
    pub public: [u8; 32],
    pub name: String,
}

impl TrustedDevice {
    pub fn id(&self) -> String {
        device_id(&self.public)
    }
}

/// Persistent set of paired devices. Implementations must be thread-safe.
pub trait TrustStore: Send + Sync {
    fn is_trusted(&self, public: &[u8; 32]) -> bool;
    fn add(&self, device: TrustedDevice);
    /// Removes by device id; returns whether something was removed.
    fn remove(&self, id: &str) -> bool;
    fn list(&self) -> Vec<TrustedDevice>;
}

#[derive(Default)]
pub struct MemoryTrustStore {
    inner: Mutex<Vec<TrustedDevice>>,
}

impl MemoryTrustStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TrustStore for MemoryTrustStore {
    fn is_trusted(&self, public: &[u8; 32]) -> bool {
        self.inner.lock().unwrap().iter().any(|d| &d.public == public)
    }
    fn add(&self, device: TrustedDevice) {
        let mut g = self.inner.lock().unwrap();
        g.retain(|d| d.public != device.public);
        g.push(device);
    }
    fn remove(&self, id: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        let n = g.len();
        g.retain(|d| d.id() != id);
        g.len() != n
    }
    fn list(&self) -> Vec<TrustedDevice> {
        self.inner.lock().unwrap().clone()
    }
}

/// Trust store persisted as `hexpubkey<TAB>name` lines.
pub struct FileTrustStore {
    path: PathBuf,
    mem: MemoryTrustStore,
}

impl FileTrustStore {
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mem = MemoryTrustStore::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let Some((k, name)) = line.split_once('\t') else { continue };
                if let Some(b) = unhex(k).and_then(|v| <[u8; 32]>::try_from(v).ok()) {
                    mem.add(TrustedDevice { public: b, name: name.to_string() });
                }
            }
        }
        Ok(Self { path, mem })
    }

    fn persist(&self) {
        let mut out = String::new();
        for d in self.mem.list() {
            out.push_str(&hex(&d.public));
            out.push('\t');
            out.push_str(&d.name.replace(['\t', '\n'], " "));
            out.push('\n');
        }
        // Write-then-rename so a crash never leaves a truncated trust list.
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, out).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

impl TrustStore for FileTrustStore {
    fn is_trusted(&self, p: &[u8; 32]) -> bool {
        self.mem.is_trusted(p)
    }
    fn add(&self, d: TrustedDevice) {
        self.mem.add(d);
        self.persist();
    }
    fn remove(&self, id: &str) -> bool {
        let r = self.mem.remove(id);
        if r {
            self.persist();
        }
        r
    }
    fn list(&self) -> Vec<TrustedDevice> {
        self.mem.list()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable() {
        let i = Identity::from_secret([7u8; 32]);
        assert_eq!(i.device_id(), Identity::from_secret([7u8; 32]).device_id());
        assert_eq!(i.device_id().len(), 32);
    }

    #[test]
    fn sign_verify() {
        let i = Identity::generate();
        let s = i.sign(b"hello");
        assert!(verify(&i.public(), b"hello", &s));
        assert!(!verify(&i.public(), b"hellp", &s));
    }

    #[test]
    fn file_store_persists_and_removes() {
        let dir = std::env::temp_dir().join(format!("rvz-trust-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let a = Identity::generate().public();
        {
            let s = FileTrustStore::open(&dir).unwrap();
            s.add(TrustedDevice { public: a, name: "Pixel".into() });
        }
        let s = FileTrustStore::open(&dir).unwrap();
        assert!(s.is_trusted(&a));
        assert!(s.remove(&device_id(&a)));
        let s = FileTrustStore::open(&dir).unwrap();
        assert!(!s.is_trusted(&a));
        let _ = std::fs::remove_file(&dir);
    }
}
