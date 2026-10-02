use revizor_adaptive::Reason;
use revizor_proto::{KeyframeReason, StreamParams};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderState {
    Connecting,
    Streaming,
    /// Link lost; retrying automatically. `attempt` counts handshake rounds.
    Reconnecting { attempt: u32 },
    Stopped,
    /// Unrecoverable (not paired, incompatible version, gave up). Text is user-presentable.
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconfigReason {
    Initial,
    /// Adaptive engine changed resolution/fps; the reason is shown to the user.
    Adaptive(Reason),
    /// Source size changed (rotation, window resize, new monitor).
    SourceChanged,
}

/// Instructions from the session to the platform encoder/capture layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderEvent {
    State(SenderState),
    PeerInfo { name: String, device_id: String, hardware_codecs: bool },
    /// (Re)configure capture size + encoder. Frames passed to `submit_video` must
    /// carry `params.epoch`; anything else is discarded.
    Reconfigure { params: StreamParams, reason: ReconfigReason, limited_by: Option<Reason> },
    /// Change encoder bitrate live (no keyframe).
    SetBitrate { video_bps: u32 },
    /// Force the next frame to be an IDR (with in-band parameter sets).
    RequestKeyframe(KeyframeReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverEvent {
    State(crate::ReceiverState),
    /// A pairing window is open; show this PIN to the user.
    PairingOpened { pin: String },
    PairingClosed,
    /// New device paired.
    Paired { name: String, device_id: String },
    /// Too many wrong PINs; a new window must be opened.
    PairingLocked,
    /// A sender connected and authenticated.
    SenderConnected { name: String, device_id: String },
    /// (Re)configure the decoder: frames of `params.epoch` follow.
    Params(StreamParams),
    SenderDisconnected,
}
