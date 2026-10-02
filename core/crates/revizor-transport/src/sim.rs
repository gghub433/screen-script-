//! In-memory network with controllable impairments, for tests and
//! benchmarks of the real session/media code. It is a *test harness*: nothing
//! in the production path uses it, and it never fabricates statistics — the
//! application measures the packets that actually come out of it.

use crate::Transport;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use revizor_proto::TransportKind;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct LinkConfig {
    /// Independent random loss, percent.
    pub loss_pct: f64,
    /// Gilbert–Elliott burst model: P(good→bad) and P(bad→good) per packet; all packets dropped in bad state.
    pub burst_enter: f64,
    pub burst_exit: f64,
    pub base_delay: Duration,
    pub jitter: Duration,
    /// Link rate; `None` = unlimited.
    pub bandwidth_bps: Option<u64>,
    /// Drop-tail queue limit (as time at link rate).
    pub max_queue: Duration,
    /// Probability a packet gets extra delay causing reordering.
    pub reorder_pct: f64,
    /// Drop everything (cable pulled / Wi-Fi off).
    pub blackout: bool,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            loss_pct: 0.0,
            burst_enter: 0.0,
            burst_exit: 0.5,
            base_delay: Duration::from_micros(500),
            jitter: Duration::ZERO,
            bandwidth_bps: None,
            max_queue: Duration::from_millis(60),
            reorder_pct: 0.0,
            blackout: false,
        }
    }
}

struct Dir {
    cfg: LinkConfig,
    rng: StdRng,
    bad: bool,
    free_at: Instant,
    q: BinaryHeap<Reverse<(Instant, u64, Vec<u8>)>>,
    n: u64,
    sent: u64,
    dropped: u64,
}

struct Shared {
    dir: [Mutex<Dir>; 2],
    cv: [Condvar; 2],
}

/// Handle to change impairments while a test runs.
#[derive(Clone)]
pub struct LinkControl(Arc<Shared>);

impl LinkControl {
    /// Mutate the config of both directions.
    pub fn update(&self, f: impl Fn(&mut LinkConfig)) {
        for d in &self.0.dir {
            f(&mut d.lock().unwrap().cfg);
        }
    }
    /// Mutate one direction only (0 = a→b, 1 = b→a).
    pub fn update_dir(&self, dir: usize, f: impl Fn(&mut LinkConfig)) {
        f(&mut self.0.dir[dir].lock().unwrap().cfg);
    }
    pub fn stats(&self, dir: usize) -> (u64, u64) {
        let d = self.0.dir[dir].lock().unwrap();
        (d.sent, d.dropped)
    }
}

pub struct SimEndpoint {
    shared: Arc<Shared>,
    tx_dir: usize,
    addr: SocketAddr,
    peer: SocketAddr,
}

/// Creates two connected endpoints (`a` at 10.0.0.1:1000, `b` at 10.0.0.2:2000).
pub fn pair(cfg: LinkConfig, seed: u64) -> (SimEndpoint, SimEndpoint, LinkControl) {
    let mk = |s: u64| {
        Mutex::new(Dir {
            cfg: cfg.clone(),
            rng: StdRng::seed_from_u64(s),
            bad: false,
            free_at: Instant::now(),
            q: BinaryHeap::new(),
            n: 0,
            sent: 0,
            dropped: 0,
        })
    };
    let shared = Arc::new(Shared { dir: [mk(seed), mk(seed ^ 0x9e37_79b9)], cv: [Condvar::new(), Condvar::new()] });
    let (aa, ba): (SocketAddr, SocketAddr) = ("10.0.0.1:1000".parse().unwrap(), "10.0.0.2:2000".parse().unwrap());
    (
        SimEndpoint { shared: shared.clone(), tx_dir: 0, addr: aa, peer: ba },
        SimEndpoint { shared: shared.clone(), tx_dir: 1, addr: ba, peer: aa },
        LinkControl(shared),
    )
}

impl Transport for SimEndpoint {
    fn kind(&self) -> TransportKind {
        TransportKind::Udp
    }

    fn send_to(&self, data: &[u8], _to: SocketAddr) -> io::Result<()> {
        let now = Instant::now();
        let mut d = self.shared.dir[self.tx_dir].lock().unwrap();
        d.sent += 1;
        if d.cfg.blackout {
            d.dropped += 1;
            return Ok(());
        }
        // Loss: burst model then random.
        let (enter, exit) = (d.cfg.burst_enter, d.cfg.burst_exit);
        let r: f64 = d.rng.gen();
        if d.bad {
            if r < exit {
                d.bad = false;
            }
        } else if r < enter {
            d.bad = true;
        }
        let rnd_loss = d.cfg.loss_pct / 100.0;
        if d.bad || d.rng.gen::<f64>() < rnd_loss {
            d.dropped += 1;
            return Ok(());
        }
        // Serialization + drop-tail queue.
        let mut at = now;
        if let Some(bw) = d.cfg.bandwidth_bps {
            let ser = Duration::from_secs_f64(data.len() as f64 * 8.0 / bw as f64);
            let start = d.free_at.max(now);
            if start - now > d.cfg.max_queue {
                d.dropped += 1;
                return Ok(());
            }
            d.free_at = start + ser;
            at = d.free_at;
        }
        let jitter = d.cfg.jitter;
        let mut delay = d.cfg.base_delay;
        if !jitter.is_zero() {
            delay += jitter.mul_f64(d.rng.gen::<f64>());
        }
        if d.cfg.reorder_pct > 0.0 && d.rng.gen::<f64>() < d.cfg.reorder_pct / 100.0 {
            delay += Duration::from_millis(3);
        }
        d.n += 1;
        let n = d.n;
        d.q.push(Reverse((at + delay, n, data.to_vec())));
        drop(d);
        self.shared.cv[self.tx_dir].notify_all();
        Ok(())
    }

    fn recv_from(&self, buf: &mut [u8], timeout: Duration) -> io::Result<Option<(usize, SocketAddr)>> {
        let dir = 1 - self.tx_dir;
        let deadline = Instant::now() + timeout;
        let mut d = self.shared.dir[dir].lock().unwrap();
        loop {
            let now = Instant::now();
            if let Some(Reverse((at, _, _))) = d.q.peek() {
                if *at <= now {
                    let Reverse((_, _, data)) = d.q.pop().unwrap();
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    return Ok(Some((n, self.peer)));
                }
            }
            if now >= deadline {
                return Ok(None);
            }
            let wake = d.q.peek().map_or(deadline, |Reverse((at, _, _))| (*at).min(deadline));
            let (g, _) = self.shared.cv[dir].wait_timeout(d, wake.saturating_duration_since(now)).unwrap();
            d = g;
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &SimEndpoint, wait: Duration) -> usize {
        let mut buf = [0u8; 16];
        let mut n = 0;
        while rx.recv_from(&mut buf, wait).unwrap().is_some() {
            n += 1;
        }
        n
    }

    #[test]
    fn delivers_with_delay() {
        let (a, b, _c) = pair(LinkConfig { base_delay: Duration::from_millis(20), ..Default::default() }, 1);
        let t = Instant::now();
        a.send_to(b"x", b.local_addr().unwrap()).unwrap();
        let mut buf = [0u8; 4];
        assert!(b.recv_from(&mut buf, Duration::from_millis(500)).unwrap().is_some());
        assert!(t.elapsed() >= Duration::from_millis(19));
    }

    #[test]
    fn random_loss_close_to_configured() {
        let (a, b, c) = pair(LinkConfig { loss_pct: 10.0, ..Default::default() }, 7);
        for _ in 0..5000 {
            a.send_to(&[0; 8], b.local_addr().unwrap()).unwrap();
        }
        let got = drain(&b, Duration::from_millis(30));
        let rate = 1.0 - got as f64 / 5000.0;
        assert!((rate - 0.10).abs() < 0.02, "{rate}");
        assert_eq!(c.stats(0).0, 5000);
    }

    #[test]
    fn bandwidth_limit_drops_excess() {
        // 1 Mbit/s with 20 ms queue: 100 x 1250 B (= 1 Mbit) sent at once mostly dropped
        let (a, b, c) = pair(LinkConfig { bandwidth_bps: Some(1_000_000), max_queue: Duration::from_millis(20), ..Default::default() }, 3);
        for _ in 0..100 {
            a.send_to(&[0; 1250], b.local_addr().unwrap()).unwrap();
        }
        let (_, dropped) = c.stats(0);
        assert!(dropped > 70, "{dropped}");
    }

    #[test]
    fn blackout_drops_everything_then_recovers() {
        let (a, b, c) = pair(LinkConfig::default(), 1);
        c.update(|x| x.blackout = true);
        a.send_to(b"x", b.local_addr().unwrap()).unwrap();
        assert_eq!(drain(&b, Duration::from_millis(10)), 0);
        c.update(|x| x.blackout = false);
        a.send_to(b"x", b.local_addr().unwrap()).unwrap();
        assert_eq!(drain(&b, Duration::from_millis(10)), 1);
    }
}
