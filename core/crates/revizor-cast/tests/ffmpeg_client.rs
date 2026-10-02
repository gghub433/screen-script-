//! A REAL media client (ffmpeg's HLS and MPEG-TS demuxers + H.264/AAC decoders) plays the live stream
//! served by our HTTP server, fed in real time from a real encoded fixture. This is the closest we can get to a
//! TV without owning one. Skipped when ffmpeg is not installed.

mod common;
use common as fx;

use revizor_cast::server::{Server, ServerConfig};
use revizor_cast::ts::TsMuxer;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn have_ffmpeg() -> bool {
    fx::have("ffmpeg")
}

fn start_live(secs: u64) -> (Server, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let server = Server::start("127.0.0.1:0".parse().unwrap(), ServerConfig::new("tok".into(), vec!["127.0.0.1".parse().unwrap()]), || {}).unwrap();
    let hub = server.hub();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let feeder = std::thread::spawn(move || {
        let mut m = TsMuxer::new(true);
        let (v, a) = (fx::video_frames(), fx::audio_frames());
        let start = Instant::now();
        let mut ai = 0usize;
        let clip_us = 120 * 33_333u64;
        let mut round = 0u64;
        'outer: while start.elapsed() < Duration::from_secs(secs) && !s2.load(Ordering::Relaxed) {
            ai = 0;
            for (pts, au, key) in &v {
                let t = round * clip_us + pts;
                // real-time pacing
                while start.elapsed() < Duration::from_micros(t) {
                    if s2.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                while ai < a.len() && a[ai].0 <= *pts {
                    hub.push_other(m.audio(round * clip_us + a[ai].0 + 5_000_000, &a[ai].1));
                    ai += 1;
                }
                hub.push_video(t, *key, m.video(t + 5_000_000, *key, au));
            }
            round += 1;
        }
        let _ = ai;
    });
    (server, stop, feeder)
}

fn decoded_frames(url: &str, seconds: u32) -> (bool, String, u32) {
    let out = Command::new("timeout")
        .args(["40", "ffmpeg", "-v", "error", "-xerror", "-i", url, "-t", &seconds.to_string(), "-progress", "pipe:1", "-f", "null", "-"])
        .output()
        .unwrap();
    let progress = String::from_utf8_lossy(&out.stdout).to_string();
    let frames = progress.lines().filter_map(|l| l.strip_prefix("frame=")).filter_map(|v| v.trim().parse::<u32>().ok()).last().unwrap_or(0);
    (out.status.success(), String::from_utf8_lossy(&out.stderr).to_string(), frames)
}

#[test]
fn ffmpeg_plays_the_live_hls_stream() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not installed: skipped");
        return;
    }
    let (server, stop, feeder) = start_live(14);
    let t0 = Instant::now();
    while !server.hub().hls_ready() && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(server.hub().hls_ready(), "playlist never became ready");
    let url = format!("http://{}/tok/live.m3u8", server.addr);
    let (ok, err, frames) = decoded_frames(&url, 4);
    stop.store(true, Ordering::SeqCst);
    feeder.join().unwrap();
    assert!(ok && err.is_empty(), "ffmpeg failed: {err}");
    assert!(frames >= 60, "decoded only {frames} frames from 4 s of HLS");
    assert!(server.hub().stats.segment_requests.load(Ordering::Relaxed) >= 2);
}

#[test]
fn ffmpeg_plays_the_progressive_dlna_stream_and_starts_quickly() {
    if !have_ffmpeg() {
        return;
    }
    let (server, stop, feeder) = start_live(12);
    std::thread::sleep(Duration::from_millis(1500));
    let url = format!("http://{}/tok/live.ts", server.addr);
    let t = Instant::now();
    let (ok, err, frames) = decoded_frames(&url, 4);
    let took = t.elapsed();
    stop.store(true, Ordering::SeqCst);
    feeder.join().unwrap();
    assert!(ok && err.is_empty(), "ffmpeg failed: {err}");
    assert!(frames >= 80, "decoded only {frames} frames from 4 s of progressive TS");
    assert!(took < Duration::from_secs(9), "start + 4 s playback took {took:?}");
}
