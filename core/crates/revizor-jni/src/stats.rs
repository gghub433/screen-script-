//! Hand-rolled JSON for statistics. A field is `null` when it was not measured —
//! the UI shows "n/a" instead of a made-up number.

use revizor_session::{ReceiverStats, SenderState, SenderStats};

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("null".into(), |v| v.to_string())
}

fn esc(s: &str) -> String {
    s.chars().flat_map(|c| if c == '"' || c == '\\' { vec!['\\', c] } else if c.is_control() { vec![' '] } else { vec![c] }).collect()
}

pub fn sender_json(s: &SenderStats) -> String {
    let state = match &s.state {
        None => "null".to_string(),
        Some(SenderState::Connecting) => "\"connecting\"".into(),
        Some(SenderState::Streaming) => "\"streaming\"".into(),
        Some(SenderState::Reconnecting { .. }) => "\"reconnecting\"".into(),
        Some(SenderState::Stopped) => "\"stopped\"".into(),
        Some(SenderState::Failed(m)) => format!("\"failed\",\"error\":\"{}\"", esc(m)),
    };
    let r = s.last_report;
    format!(
        "{{\"state\":{state},\"peer\":\"{}\",\"codec\":{},\"width\":{},\"height\":{},\"fpsTarget\":{},\"bitrateTarget\":{},\"sentBps\":{},\
         \"fecK\":{},\"rttUs\":{},\"framesSent\":{},\"framesDroppedSender\":{},\"retransmitted\":{},\"keyframes\":{},\"sendErrors\":{},\
         \"encodeUs\":{},\"limitedBy\":{},\"hwCodecs\":{},\"lossPct\":{},\"jitterUs\":{},\"e2eUs\":{},\"decodeUs\":{},\"recvBps\":{}}}",
        esc(&s.peer_name),
        opt(s.codec.map(|c| c as u8)),
        s.width,
        s.height,
        s.fps_target,
        s.bitrate_target_bps,
        s.sent_bps,
        s.fec_k,
        opt(s.rtt_us),
        s.frames_sent,
        s.frames_dropped_sender,
        s.retransmitted_packets,
        s.keyframes_sent,
        s.send_errors,
        opt(s.encode_us_avg),
        opt(s.limited_by.map(|r| format!("{r:?}")).map(|r| format!("\"{r}\""))),
        s.hardware_codecs,
        opt(r.map(|r| if r.packets_expected > 0 { 100.0 * r.packets_lost as f32 / r.packets_expected as f32 } else { 0.0 })),
        opt(r.map(|r| r.jitter_us)),
        opt(r.and_then(|r| (r.e2e_latency_us > 0).then_some(r.e2e_latency_us))),
        opt(r.and_then(|r| (r.decode_us > 0).then_some(r.decode_us))),
        opt(r.map(|r| r.recv_bitrate_bps)),
    )
}

pub fn receiver_json(s: &ReceiverStats) -> String {
    let p = s.params;
    format!(
        "{{\"sender\":\"{}\",\"codec\":{},\"width\":{},\"height\":{},\"fpsTarget\":{},\"fps\":{:.1},\"recvBps\":{},\"lossPct\":{:.2},\
         \"jitterUs\":{},\"rttUs\":{},\"arrivalLatencyUs\":{},\"e2eUs\":{},\"decodeUs\":{},\"framesDelivered\":{},\"framesAbandoned\":{},\
         \"framesDiscarded\":{},\"framesDroppedApp\":{},\"recoveredFec\":{},\"recoveredRetx\":{},\"nacks\":{},\"keyframeRequests\":{},\"buffered\":{},\"clockOffsetUs\":{}}}",
        esc(&s.sender_name),
        opt(p.map(|p| p.video.codec as u8)),
        p.map_or(0, |p| p.video.width),
        p.map_or(0, |p| p.video.height),
        p.map_or(0, |p| p.video.fps),
        s.fps,
        s.recv_bitrate_bps,
        s.loss_pct,
        s.jitter_us,
        opt(s.rtt_us),
        opt(s.network_latency_us),
        opt(s.e2e_latency_us),
        opt(s.decode_us),
        s.frames_delivered,
        s.frames_abandoned,
        s.frames_discarded,
        s.frames_dropped_app,
        s.packets_recovered_fec,
        s.packets_recovered_retx,
        s.nacks_sent,
        s.keyframe_requests,
        s.buffered_frames,
        opt(s.clock_offset_us),
    )
}
