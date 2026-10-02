use revizor_crypto::pairing::{PairInitiator, PairingError};
use revizor_crypto::{Identity, TrustStore};
use revizor_proto::{PacketHeader, PacketKind};
use revizor_transport::Transport;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("receiver did not answer")]
    NoAnswer,
    #[error("wrong PIN or pairing refused")]
    WrongPin,
    #[error("I/O error: {0}")]
    Io(String),
}

fn datagram(body: &[u8]) -> Vec<u8> {
    let mut d = PacketHeader::new(PacketKind::Pairing, 0, 0).encode().to_vec();
    d.extend_from_slice(body);
    d
}

/// Blocking sender-side pairing: runs the SPAKE2 exchange against the receiver
/// that displays `pin`. On success both identities are in the respective trust stores.
pub fn pair_as_sender(
    transport: &dyn Transport,
    peer: SocketAddr,
    identity: Arc<Identity>,
    device_name: &str,
    pin: &str,
    trust: Arc<dyn TrustStore>,
) -> Result<(), PairError> {
    let (mut init, p1) = PairInitiator::start(identity, device_name, pin, trust);
    let send = |b: &[u8]| transport.send_to(&datagram(b), peer).map_err(|e| PairError::Io(e.to_string()));
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(8);

    let recv_body = |buf: &mut [u8; 2048]| -> Result<Vec<u8>, PairError> {
        while Instant::now() < deadline {
            if let Some((n, _)) = transport.recv_from(buf, Duration::from_millis(100)).map_err(|e| PairError::Io(e.to_string()))? {
                if let Ok((h, body)) = PacketHeader::decode(&buf[..n]) {
                    if h.kind == PacketKind::Pairing {
                        return Ok(body.to_vec());
                    }
                }
            }
        }
        Err(PairError::NoAnswer)
    };

    send(&p1)?;
    let p2 = recv_body(&mut buf)?;
    let p3 = init.on_p2(&p2).map_err(|e| match e {
        PairingError::BadPin => PairError::WrongPin,
        other => PairError::Io(other.to_string()),
    })?;
    send(&p3)?;
    let p4 = recv_body(&mut buf)?;
    init.on_p4(&p4).map_err(|_| PairError::WrongPin)
}
