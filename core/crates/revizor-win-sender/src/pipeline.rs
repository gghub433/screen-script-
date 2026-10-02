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
