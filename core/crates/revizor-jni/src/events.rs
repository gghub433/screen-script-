use jni::objects::{GlobalRef, JObject, JValue};
use jni::{JNIEnv, JavaVM};
use revizor_adaptive::Reason;
use revizor_session::{ReceiverEvent, ReceiverState, ReconfigReason, SenderEvent, SenderState};

/// Kotlin `Native.Callback.onEvent(kind: Int, nums: LongArray, text: String)`.
pub struct Callback {
    vm: JavaVM,
    obj: GlobalRef,
}

impl Callback {
    pub fn new(env: &mut JNIEnv, cb: &JObject) -> Option<Self> {
        Some(Self { vm: env.get_java_vm().ok()?, obj: env.new_global_ref(cb).ok()? })
    }

    pub fn call(&self, kind: i32, nums: &[i64], text: &str) {
        let Ok(mut env) = self.vm.attach_current_thread() else { return };
        let Ok(arr) = env.new_long_array(nums.len() as i32) else { return };
        let _ = env.set_long_array_region(&arr, 0, nums);
        let Ok(s) = env.new_string(text) else { return };
        let _ = env.call_method(
            &self.obj,
            "onEvent",
            "(I[JLjava/lang/String;)V",
            &[JValue::Int(kind), JValue::Object(&arr), JValue::Object(&s)],
        );
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
    }
}

fn reason_idx(r: Reason) -> i64 {
    match r {
        Reason::Network => 0,
        Reason::EncoderOverload => 1,
        Reason::DecoderOverload => 2,
        Reason::Thermal => 3,
        Reason::Battery => 4,
        Reason::Recovery => 5,
        Reason::Profile => 6,
    }
}

fn params_nums(p: &revizor_proto::StreamParams) -> Vec<i64> {
    vec![
        p.epoch as i64,
        p.video.codec as i64,
        p.video.width as i64,
        p.video.height as i64,
        p.video.fps as i64,
        p.video.bitrate_bps as i64,
        p.audio.codec as i64,
        p.audio.sample_rate as i64,
        p.audio.channels as i64,
        p.audio.bitrate_bps as i64,
    ]
}

pub fn sender_event(cb: &Callback, e: SenderEvent) {
    match e {
        SenderEvent::State(s) => match s {
            SenderState::Connecting => cb.call(1, &[0], ""),
            SenderState::Streaming => cb.call(1, &[1], ""),
            SenderState::Reconnecting { attempt } => cb.call(1, &[2, attempt as i64], ""),
            SenderState::Stopped => cb.call(1, &[3], ""),
            SenderState::Failed(m) => cb.call(1, &[4], &m),
        },
        SenderEvent::PeerInfo { name, device_id, hardware_codecs } => cb.call(2, &[hardware_codecs as i64], &format!("{name}|{device_id}")),
        SenderEvent::Reconfigure { params, reason, limited_by } => {
            let mut n = params_nums(&params);
            n.push(match reason {
                ReconfigReason::Initial => 0,
                ReconfigReason::SourceChanged => 1,
                ReconfigReason::Adaptive(r) => 2 + reason_idx(r),
            });
            n.push(limited_by.map_or(-1, reason_idx));
            cb.call(3, &n, "");
        }
        SenderEvent::SetBitrate { video_bps } => cb.call(4, &[video_bps as i64], ""),
        SenderEvent::RequestKeyframe(r) => cb.call(5, &[r as i64], ""),
    }
}

pub fn receiver_event(cb: &Callback, e: ReceiverEvent) {
    match e {
        ReceiverEvent::State(ReceiverState::Listening) => cb.call(1, &[0], ""),
        ReceiverEvent::State(ReceiverState::Streaming { sender }) => cb.call(1, &[1], &sender),
        ReceiverEvent::State(ReceiverState::Stopped) => cb.call(1, &[2], ""),
        ReceiverEvent::PairingOpened { pin } => cb.call(2, &[], &pin),
        ReceiverEvent::PairingClosed => cb.call(3, &[], ""),
        ReceiverEvent::Paired { name, device_id } => cb.call(4, &[], &format!("{name}|{device_id}")),
        ReceiverEvent::PairingLocked => cb.call(5, &[], ""),
        ReceiverEvent::SenderConnected { name, device_id } => cb.call(6, &[], &format!("{name}|{device_id}")),
        ReceiverEvent::Params(p) => cb.call(7, &params_nums(&p), ""),
        ReceiverEvent::SenderDisconnected => cb.call(8, &[], ""),
    }
}
