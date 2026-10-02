//! Shared helpers: a REAL H.264/AAC elementary stream (libx264 / ffmpeg AAC, in tests/fixtures).
#![allow(dead_code)]

use revizor_cast::ts::TsMuxer;
use std::path::PathBuf;
use std::process::Command;

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

/// (pts_us, bytes, keyframe) per access unit.
pub fn video_frames() -> Vec<(u64, Vec<u8>, bool)> {
    let data = fixture("video.h264");
    let idx = String::from_utf8(fixture("video.idx")).unwrap();
    let mut off = 0;
    idx.lines()
        .map(|l| {
            let mut it = l.split_whitespace();
            let pts: u64 = it.next().unwrap().parse().unwrap();
            let len: usize = it.next().unwrap().parse().unwrap();
            let key = it.next().unwrap() == "1";
            let f = (pts, data[off..off + len].to_vec(), key);
            off += len;
            f
        })
        .collect()
}

/// ADTS frames with their capture time (1024 samples @ 48 kHz each).
pub fn audio_frames() -> Vec<(u64, Vec<u8>)> {
    let d = fixture("audio.adts");
    let (mut off, mut n, mut out) = (0usize, 0u64, vec![]);
    while off + 7 <= d.len() {
        assert!(d[off] == 0xFF && d[off + 1] & 0xF0 == 0xF0, "ADTS sync");
        let len = (((d[off + 3] & 3) as usize) << 11) | ((d[off + 4] as usize) << 3) | (d[off + 5] as usize >> 5);
        out.push((n * 1024 * 1_000_000 / 48_000, d[off..off + len].to_vec()));
        off += len;
        n += 1;
    }
    out
}

/// Whether an external tool (ffmpeg / ffprobe) is installed. The decoder-validation tests are skipped without it
/// so that `cargo test` works everywhere, but CI sets `REVIZOR_REQUIRE_FFMPEG=1` so a missing tool fails the run
/// instead of silently dropping the independent-decoder checks.
pub fn have(cmd: &str) -> bool {
    let ok = Command::new(cmd).arg("-version").output().map(|o| o.status.success()).unwrap_or(false);
    if !ok && std::env::var_os("REVIZOR_REQUIRE_FFMPEG").is_some() {
        panic!("{cmd} is required (REVIZOR_REQUIRE_FFMPEG is set) but was not found");
    }
    ok
}

pub fn mux(with_audio: bool) -> Vec<u8> {
    let mut m = TsMuxer::new(with_audio);
    let (v, a) = (video_frames(), audio_frames());
    let mut ts = Vec::new();
    let mut ai = 0;
    for (pts, au, key) in &v {
        while with_audio && ai < a.len() && a[ai].0 <= *pts {
            ts.extend(m.audio(a[ai].0 + 1_000_000, &a[ai].1)); // audio shifted like a real capture clock offset
            ai += 1;
        }
        ts.extend(m.video(pts + 1_000_000, *key, au));
    }
    ts
}

