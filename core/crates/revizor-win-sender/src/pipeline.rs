//! Platform-neutral contract between the controller (UI, sessions) and the
//! capture+encode pipeline. The Windows implementation lives in `win/`.

use revizor_proto::StreamParams;

#[derive(Debug, Clone)]
pub enum PipelineCmd {
    /// New epoch: recreate colour conversion + encoder with these parameters.
    Reconfigure(StreamParams),
    /// Live bitrate change (video bits per second).
    SetBitrate(u32),
    ForceKeyframe,
    Stop,
}

/// What the encoder layer reports about itself — shown in diagnostics.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EncoderInfo {
    pub name: String,
    pub hardware: bool,
}

use revizor_cast::CastSession;
use revizor_session::{SenderSession, SourceInfo};
use std::sync::Arc;

/// Where the capture/encode pipeline delivers frames: the Revizor protocol engine, or a standard TV (Cast / DLNA).
pub trait FrameSink: Send + Sync {
    /// Returns false if the frame was dropped.
    fn video(&self, epoch: u16, pts_us: u64, keyframe: bool, data: Vec<u8>) -> bool;
    fn encode_time_us(&self, us: u32);
    fn capture_drop(&self);
    /// The captured window/monitor changed size.
    fn source_changed(&self, w: u16, h: u16, refresh_hz: u16);
}

pub struct RevizorSink(pub Arc<SenderSession>);

impl FrameSink for RevizorSink {
    fn video(&self, epoch: u16, pts_us: u64, keyframe: bool, data: Vec<u8>) -> bool {
        self.0.submit_video(epoch, pts_us, keyframe, data)
    }
    fn encode_time_us(&self, us: u32) {
        self.0.report_encode_time_us(us)
    }
    fn capture_drop(&self) {
        self.0.report_capture_drop()
    }
    fn source_changed(&self, w: u16, h: u16, refresh_hz: u16) {
        self.0.set_source(SourceInfo { width: w, height: h, refresh_hz })
    }
}

/// TV mode has a fixed output size; a changed source is letterboxed into it by the GPU converter.
pub struct TvSink(pub Arc<CastSession>);

impl FrameSink for TvSink {
    fn video(&self, _epoch: u16, pts_us: u64, keyframe: bool, data: Vec<u8>) -> bool {
        self.0.submit_video(pts_us, keyframe, &data);
        true
    }
    fn encode_time_us(&self, _us: u32) {}
    fn capture_drop(&self) {}
    fn source_changed(&self, _w: u16, _h: u16, _hz: u16) {}
}
