//! The capture → convert → encode loop for one stream, on a dedicated thread.

use super::capture::{create_device, create_item, Capture, D3d};
use super::convert::Converter;
use super::encoder::{EncodedOut, Encoder};
use crate::pipeline::{EncoderInfo, FrameSink, PipelineCmd};
use revizor_proto::StreamParams;
use revizor_session::Clock;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

pub struct WinPipeline {
    cmd: Sender<PipelineCmd>,
    thread: Option<JoinHandle<()>>,
    pub info: Arc<Mutex<EncoderInfo>>,
}

/// How the stream is consumed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Revizor protocol: the session adapts size/fps/bitrate and asks for keyframes.
    Revizor,
    /// Standard TV (Cast / DLNA): fixed output size, a keyframe every second, last frame repeated on a static screen.
    Tv,
}

impl WinPipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        source_id: String,
        refresh_hz: u16,
        sink: Arc<dyn FrameSink>,
        clock: Arc<dyn Clock>,
        params: StreamParams,
        mode: Mode,
        cmd: (Sender<PipelineCmd>, Receiver<PipelineCmd>),
        on_error: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Self {
        let (tx, rx) = cmd;
        let info = Arc::new(Mutex::new(EncoderInfo::default()));
        let info2 = info.clone();
        let thread = std::thread::Builder::new()
            .name("rvz-win-pipeline".into())
            .spawn(move || unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                // 1 ms timer resolution so pacing sleeps are accurate.
                let _ = timeBeginPeriod(1);
                if let Err(e) = run(&source_id, refresh_hz, &sink, clock, params, mode, rx, &info2) {
                    log::error!("pipeline stopped: {e}");
                    on_error(e);
                }
                CoUninitialize();
            })
            .expect("spawn pipeline thread");
        Self { cmd: tx, thread: Some(thread), info }
    }

    pub fn send(&self, c: PipelineCmd) {
        let _ = self.cmd.send(c);
    }

    pub fn stop(mut self) {
        let _ = self.cmd.send(PipelineCmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Stage {
    conv: Converter,
    enc: Encoder,
    params: StreamParams,
}

fn build_stage(d3d: &D3d, in_w: u32, in_h: u32, p: StreamParams, mode: Mode, info: &Mutex<EncoderInfo>) -> Result<Stage, String> {
    let (w, h, fps) = (p.video.width as u32, p.video.height as u32, p.video.fps as u32);
    let conv = Converter::new(d3d, in_w, in_h, w, h, fps, mode == Mode::Tv)?;
    let gop = if mode == Mode::Tv { 1 } else { 10 };
    let enc = Encoder::new(d3d, w, h, fps, p.video.bitrate_bps, gop)?;
    *info.lock().unwrap() = EncoderInfo { name: enc.name.clone(), hardware: enc.hardware };
    log::info!("encoder: {} (hardware: {}) {}x{}@{} {} kbit/s", enc.name, enc.hardware, w, h, fps, p.video.bitrate_bps / 1000);
    Ok(Stage { conv, enc, params: p })
}

#[allow(clippy::too_many_arguments)]
fn run(
    source_id: &str,
    refresh_hz: u16,
    sink: &Arc<dyn FrameSink>,
    clock: Arc<dyn Clock>,
    params: StreamParams,
    mode: Mode,
    rx: Receiver<PipelineCmd>,
    info: &Mutex<EncoderInfo>,
) -> Result<(), String> {
    let d3d = create_device()?;
    let item = create_item(source_id)?;
    let mut cap = Capture::start(&d3d, item, clock.clone())?;
    let (iw, ih) = (cap.size.Width as u32, cap.size.Height as u32);
    let mut st = build_stage(&d3d, iw, ih, params, mode, info)?;
    let mut outs: Vec<EncodedOut> = Vec::new();
    let mut next_due_us = 0u64;
    let mut last_nv12: Option<ID3D11Texture2D> = None;
    let mut resend_key_at: Option<u64> = None;
    let mut last_submit_us = clock.now_us();
    let mut last_force_key_us = clock.now_us();

    loop {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                PipelineCmd::Stop => return Ok(()),
                PipelineCmd::SetBitrate(b) => st.enc.set_bitrate(b),
                PipelineCmd::ForceKeyframe => {
                    st.enc.force_keyframe();
                    // A static screen produces no new frames; re-encode the last one so the IDR still appears.
                    resend_key_at = Some(clock.now_us() + 40_000);
                }
                PipelineCmd::Reconfigure(p) => {
                    let (iw, ih) = st.conv.input_size();
                    drop(std::mem::replace(&mut st, build_stage(&d3d, iw, ih, p, mode, info)?));
                    last_nv12 = None;
                    next_due_us = 0;
                }
            }
        }

        st.enc.poll(&mut outs)?;
        for o in outs.drain(..) {
            sink.encode_time_us(o.encode_us);
            sink.video(st.params.epoch, o.pts_us, o.keyframe, o.data);
        }

        let now = clock.now_us();
        let interval = 1_000_000 / st.params.video.fps.max(1) as u64;
        if mode == Mode::Tv && now.saturating_sub(last_force_key_us) >= 1_000_000 {
            // HLS segments can only start at keyframes: guarantee one per second.
            last_force_key_us = now;
            st.enc.force_keyframe();
            resend_key_at.get_or_insert(now + 40_000);
        }
        match cap.mailbox.take(Duration::from_millis(3)) {
            Some(f) => {
                // Source size changed (window resized, resolution switch): follow it.
                if f.width != 0 && (f.width, f.height) != st.conv.input_size() {
                    cap.resize_if_needed(f.width, f.height);
                    sink.source_changed(f.width as u16, f.height as u16, refresh_hz);
                    st.conv = Converter::new(&d3d, f.width, f.height, st.params.video.width as u32, st.params.video.height as u32, st.params.video.fps as u32, mode == Mode::Tv)?;
                    continue;
                }
                if f.captured_us + interval / 4 < next_due_us {
                    continue; // source runs faster than the stream: skip, never queue
                }
                next_due_us = (next_due_us + interval).max(f.captured_us);
                if !st.enc.can_accept() {
                    sink.capture_drop();
                    continue;
                }
                let tex = st.conv.convert(&f.texture)?;
                st.enc.submit(&tex, f.captured_us)?;
                last_nv12 = Some(tex);
                last_submit_us = now;
                resend_key_at = None;
            }
            None => {
                // Static screen. Keyframe requests (and, in TV mode, a steady trickle of frames so the TV's
                // player never starves) are served by re-encoding the last converted picture.
                let tv_idle = mode == Mode::Tv && now.saturating_sub(last_submit_us) >= 200_000;
                let wants = resend_key_at.is_some_and(|at| now >= at) || tv_idle;
                if let (true, Some(tex)) = (wants, last_nv12.as_ref()) {
                    if st.enc.can_accept() {
                        st.enc.submit(tex, now)?;
                        resend_key_at = None;
                        last_submit_us = now;
                    }
                }
            }
        }
    }
}
