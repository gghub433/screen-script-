//! Transport abstraction. The video pipeline never touches sockets: it talks to
//! a [`Transport`], so UDP can be replaced by QUIC or a USB bulk pipe without
//! touching capture, encoding, packetization or the session logic.
//!
//! Implemented: [`UdpTransport`] (primary, lowest latency), [`TcpTransport`]
//! (fallback for networks that block UDP and for `adb reverse` USB tunnels).
//! [`sim`] is an in-memory impaired link used by tests/benchmarks.

pub mod discovery;
pub mod sim;
mod tcp;
mod udp;

pub use tcp::TcpTransport;
pub use udp::UdpTransport;

use revizor_proto::TransportKind;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

pub trait Transport: Send + Sync {
    fn kind(&self) -> TransportKind;
    fn send_to(&self, data: &[u8], to: SocketAddr) -> io::Result<()>;
    /// Blocks up to `timeout`; `Ok(None)` on timeout.
    fn recv_from(&self, buf: &mut [u8], timeout: Duration) -> io::Result<Option<(usize, SocketAddr)>>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}
