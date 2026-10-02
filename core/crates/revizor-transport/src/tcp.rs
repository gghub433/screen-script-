use crate::Transport;
use revizor_proto::TransportKind;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Length-prefixed (u16) datagrams over one TCP connection. Used when UDP is
/// blocked and for USB debugging tunnels (`adb reverse tcp:PORT tcp:PORT`).
/// Head-of-line blocking makes it worse than UDP for live video, so the
/// negotiation only chooses it when UDP is not common to both ends.
pub struct TcpTransport {
    listener: Option<TcpListener>,
    remote: Option<SocketAddr>,
    stream: Mutex<Option<TcpStream>>,
    rbuf: Mutex<Vec<u8>>,
}

impl TcpTransport {
    pub fn listen(addr: SocketAddr) -> io::Result<Self> {
        let l = TcpListener::bind(addr)?;
        l.set_nonblocking(true)?;
        Ok(Self { listener: Some(l), remote: None, stream: Mutex::new(None), rbuf: Mutex::new(Vec::new()) })
    }

    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let t = Self { listener: None, remote: Some(addr), stream: Mutex::new(None), rbuf: Mutex::new(Vec::new()) };
        t.ensure_stream()?;
        Ok(t)
    }

    fn adopt(&self, s: TcpStream) {
        let _ = s.set_nodelay(true);
        *self.stream.lock().unwrap() = Some(s);
        self.rbuf.lock().unwrap().clear();
    }

    fn ensure_stream(&self) -> io::Result<()> {
        if let Some(l) = &self.listener {
            // A new connection replaces the old one (sender restarted).
            match l.accept() {
                Ok((s, _)) => {
                    s.set_nonblocking(false)?;
                    self.adopt(s);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        } else if self.stream.lock().unwrap().is_none() {
            if let Some(r) = self.remote {
                self.adopt(TcpStream::connect_timeout(&r, Duration::from_secs(2))?);
            }
        }
        Ok(())
    }

    fn peer(&self) -> Option<SocketAddr> {
        self.stream.lock().unwrap().as_ref().and_then(|s| s.peer_addr().ok())
    }
}

impl Transport for TcpTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn send_to(&self, data: &[u8], _to: SocketAddr) -> io::Result<()> {
        if data.len() > u16::MAX as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "datagram too large"));
        }
        self.ensure_stream()?;
        let mut g = self.stream.lock().unwrap();
        let Some(s) = g.as_mut() else { return Err(io::Error::new(io::ErrorKind::NotConnected, "no tcp peer")) };
        let mut framed = Vec::with_capacity(data.len() + 2);
        framed.extend_from_slice(&(data.len() as u16).to_le_bytes());
        framed.extend_from_slice(data);
        if let Err(e) = s.write_all(&framed) {
            *g = None;
            return Err(e);
        }
        Ok(())
    }

    fn recv_from(&self, buf: &mut [u8], timeout: Duration) -> io::Result<Option<(usize, SocketAddr)>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Err(e) = self.ensure_stream() {
                if self.remote.is_none() {
                    return Err(e);
                }
            }
            // Deliver a complete frame if already buffered.
            {
                let mut rb = self.rbuf.lock().unwrap();
                if rb.len() >= 2 {
                    let n = u16::from_le_bytes([rb[0], rb[1]]) as usize;
                    if rb.len() >= 2 + n {
                        if n > buf.len() {
                            rb.drain(..2 + n);
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame larger than buffer"));
                        }
                        buf[..n].copy_from_slice(&rb[2..2 + n]);
                        rb.drain(..2 + n);
                        let from = self.peer().unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());
                        return Ok(Some((n, from)));
                    }
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let mut tmp = [0u8; 4096];
            let r = {
                let g = self.stream.lock().unwrap();
                match g.as_ref() {
                    None => None,
                    Some(s) => {
                        // Short slices so a newly arrived connection is noticed promptly.
                        s.set_read_timeout(Some(left.min(Duration::from_millis(20))))?;
                        Some((&*s).read(&mut tmp))
                    }
                }
            };
            match r {
                None => std::thread::sleep(left.min(Duration::from_millis(5))),
                Some(Ok(0)) => {
                    *self.stream.lock().unwrap() = None;
                    if self.remote.is_none() {
                        std::thread::sleep(left.min(Duration::from_millis(5)));
                    }
                }
                Some(Ok(n)) => self.rbuf.lock().unwrap().extend_from_slice(&tmp[..n]),
                Some(Err(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Some(Err(_)) => *self.stream.lock().unwrap() = None,
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match (&self.listener, self.stream.lock().unwrap().as_ref()) {
            (Some(l), _) => l.local_addr(),
            (None, Some(s)) => s.local_addr(),
            _ => Err(io::Error::new(io::ErrorKind::NotConnected, "not connected")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_roundtrip_with_coalescing() {
        let server = TcpTransport::listen("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let client = TcpTransport::connect(addr).unwrap();
        for i in 0..10u8 {
            client.send_to(&vec![i; 100 + i as usize], addr).unwrap();
        }
        let mut buf = [0u8; 2048];
        for i in 0..10u8 {
            let (n, _) = server.recv_from(&mut buf, Duration::from_secs(2)).unwrap().unwrap();
            assert_eq!(n, 100 + i as usize);
            assert!(buf[..n].iter().all(|b| *b == i));
        }
        server.send_to(b"pong", addr).unwrap();
        let (n, _) = client.recv_from(&mut buf, Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(&buf[..n], b"pong");
        assert!(client.recv_from(&mut buf, Duration::from_millis(20)).unwrap().is_none());
    }
}
