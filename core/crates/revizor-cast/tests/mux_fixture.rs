//! Validates the TS muxer against a real H.264/AAC stream using ffprobe/ffmpeg as an independent
//! demuxer + decoder. Skipped (with a message) when ffmpeg is not installed.

mod common;
use common::*;
use std::path::PathBuf;
use std::process::Command;
use revizor_cast::ts::TsMuxer;

#[test]
fn ffprobe_and_decoder_accept_the_stream() {
    if !have("ffprobe") || !have("ffmpeg") {
        eprintln!("ffmpeg not installed: skipping decoder validation");
        return;
    }
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join("fixture.ts");
    std::fs::write(&path, mux(true)).unwrap();

    // structure: one H.264 160x90 video stream + one AAC stream, 120 decodable frames
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-show_entries", "stream=codec_name,codec_type,width,height,nb_read_frames", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&probe.stdout).to_string();
    assert!(probe.stderr.is_empty(), "ffprobe complained: {}", String::from_utf8_lossy(&probe.stderr));
    assert!(text.contains("h264,video,160,90,120"), "unexpected streams: {text}");
    assert!(text.contains("aac,audio"), "unexpected streams: {text}");

    // full software decode of both streams must be clean (no corrupt NALs, no timestamp errors)
    let dec = Command::new("ffmpeg").args(["-v", "error", "-xerror", "-i"]).arg(&path).args(["-f", "null", "-"]).output().unwrap();
    assert!(dec.status.success() && dec.stderr.is_empty(), "decoder errors: {}", String::from_utf8_lossy(&dec.stderr));
}

#[test]
fn video_only_stream_decodes_and_timestamps_are_monotonic() {
    if !have("ffprobe") {
        return;
    }
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("video_only.ts");
    std::fs::write(&path, mux(false)).unwrap();
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,dts_time,flags", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let pts: Vec<f64> = text.lines().filter_map(|l| l.split(',').next()?.parse().ok()).collect();
    assert_eq!(pts.len(), 120);
    assert!(pts.windows(2).all(|w| w[1] > w[0]), "pts not strictly increasing");
    let step = (pts[119] - pts[0]) / 119.0;
    assert!((step - 1.0 / 30.0).abs() < 0.001, "frame spacing {step}");
    // keyframes flagged K on exactly the 4 IDR frames
    assert_eq!(text.lines().filter(|l| l.contains(",K")).count(), 4);
}

#[test]
fn stream_can_start_at_any_keyframe() {
    if !have("ffmpeg") {
        return;
    }
    // What a TV that joins late sees: PSI + a later IDR + the following frames only.
    let mut m = TsMuxer::new(false);
    let v = video_frames();
    let start = v.iter().position(|f| f.2 && f.0 >= 2_000_000 / 1).unwrap_or(60);
    let mut ts = Vec::new();
    for (pts, au, key) in &v[start..] {
        ts.extend(m.video(pts + 1_000_000, *key, au));
    }
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("late_join.ts");
    std::fs::write(&path, ts).unwrap();
    let out = Command::new("ffmpeg").args(["-v", "error", "-xerror", "-i"]).arg(&path).args(["-f", "null", "-"]).output().unwrap();
    assert!(out.status.success() && out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
}
