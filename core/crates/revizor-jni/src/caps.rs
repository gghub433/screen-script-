use revizor_adaptive::Profile;
use revizor_proto::{AudioCodec, Capabilities, Codec, CodecCap, Platform, Preference, TransportKind};

/// `codec_caps` = flat `[codec, maxW, maxH, maxFps, hardware]` groups, queried from
/// `MediaCodecInfo.VideoCapabilities` on the Kotlin side.
pub fn build_caps(name: &str, codec_caps: &[i32], audio: &[i32], max_bitrate: u32, tcp: bool) -> Capabilities {
    let codecs = codec_caps
        .chunks_exact(5)
        .filter_map(|c| {
            Some(CodecCap {
                codec: Codec::from_u8(c[0] as u8).ok()?,
                max_width: c[1].clamp(0, 65535) as u16,
                max_height: c[2].clamp(0, 65535) as u16,
                max_fps: c[3].clamp(1, 1000) as u16,
                hardware: c[4] != 0,
            })
        })
        .collect();
    Capabilities {
        device_name: name.to_string(),
        platform: Platform::Android,
        codecs,
        audio: audio.iter().filter_map(|a| AudioCodec::from_u8(*a as u8).ok()).filter(|a| *a != AudioCodec::None).collect(),
        transports: TransportKind::Udp.bit() | if tcp { TransportKind::Tcp.bit() } else { 0 },
        hdr: false,
        max_bitrate_bps: max_bitrate,
    }
}

/// `custom` = `[shortSide, fps, bitrateBps]`, zeros mean "auto".
pub fn build_pref(order: &[i32], want_audio: bool, custom: &[i32]) -> (Preference, Option<(u16, u16)>) {
    let mut codec_order: Vec<Codec> = order.iter().filter_map(|c| Codec::from_u8(*c as u8).ok()).collect();
    if !codec_order.contains(&Codec::H264) {
        codec_order.push(Codec::H264); // mandatory fallback
    }
    let short = custom.first().copied().unwrap_or(0);
    let fps = custom.get(1).copied().unwrap_or(0);
    let pref = Preference {
        codec_order,
        want_audio,
        max_short_side: (short > 0).then_some(short as u16),
        target_fps: if fps > 0 { fps as u16 } else { 60 },
        target_bitrate_bps: custom.get(2).copied().filter(|b| *b > 0).map(|b| b as u32),
    };
    (pref, (short > 0 && fps > 0).then_some((short as u16, fps as u16)))
}

pub fn profile_from(p: i32) -> Profile {
    match p {
        0 => Profile::BatterySaver,
        2 => Profile::Quality,
        3 => Profile::LowLatency,
        4 => Profile::Custom,
        _ => Profile::Balanced,
    }
}
