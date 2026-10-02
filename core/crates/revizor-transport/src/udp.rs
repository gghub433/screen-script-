use crate::Transport;
use revizor_proto::TransportKind;
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

pub struct UdpTransport {
    sock: UdpSocket,
}

impl UdpTransport {
    /// Binds a UDP socket tuned for real-time video: large kernel buffers so
    /// keyframe bursts are not dropped by the OS, and DSCP AF41 so Wi-Fi
    /// (WMM) places video in the high-priority queue.
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        s.set_reuse_address(true)?;
        let _ = s.set_recv_buffer_size(4 << 20);
        let _ = s.set_send_buffer_size(2 << 20);
        if addr.is_ipv4() {
            let _ = s.set_tos(0x88); // AF41; best effort, ignored where unsupported
        }
        s.bind(&addr.into())?;
        Ok(Self { sock: s.into() })
    }
}

impl Transport for UdpTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Udp
    }
    fn send_to(&self, data: &[u8], to: SocketAddr) -> io::Result<()> {
        self.sock.send_to(data, to).map(|_| ())
    }
    fn recv_from(&self, buf: &mut [u8], timeout: Duration) -> io::Result<Option<(usize, SocketAddr)>> {
        self.sock.set_read_timeout(Some(timeout.max(Duration::from_micros(500))))?;
        match self.sock.recv_from(buf) {
            Ok(v) => Ok(Some(v)),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(None),
            // Windows reports ICMP "port unreachable" from an earlier send as a recv error.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_roundtrip_and_timeout() {
        let a = UdpTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let b = UdpTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        a.send_to(b"hello", b.local_addr().unwrap()).unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = b.recv_from(&mut buf, Duration::from_millis(500)).unwrap().unwrap();
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(from, a.local_addr().unwrap());
        assert!(b.recv_from(&mut buf, Duration::from_millis(10)).unwrap().is_none());
    }
}
