# Encoder / decoder / resolution selection

## What is read from the device (never assumed)

**Android** — `MediaCodecList`: for each of H.264 (`video/avc`), HEVC (`video/hevc`), AV1 (`video/av01`) the best
codec is chosen as *hardware first* (`isHardwareAccelerated && !isSoftwareOnly`, API 29+), then by name for
determinism. From `VideoCapabilities` we read max width/height, the supported frame rate for 1080p, and the size
alignment (used as `size_align` so the stream size is always encodable). These numbers are sent to the peer in the
handshake as `CodecCap{codec, max_w, max_h, max_fps, hardware}`.

**Windows** — `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, HARDWARE)` for NV12→H.264. If a hardware MFT exists, H.264 is
advertised as `hardware=true`, otherwise as software (and the UI says "Software encoder").

## Negotiation (`revizor_proto::negotiate`)

Inputs: sender caps, receiver caps, user preference (codec order allowed by the build/settings, audio on/off, optional
max short side, fps, bitrate), the source size.

1. **Codec**: usable only if the sender can *encode* it **and** the receiver can *decode* it. Among usable codecs,
   hardware-on-both-ends wins; ties go to the preference order. H.264 is always appended as the mandatory fallback.
   Default order is `[H264]` (MVP); "Allow HEVC" in Settings makes it `[HEVC, H264]`. AV1 exists in the protocol but the
   apps do not offer it yet (needs encoder/decoder validation on devices).
2. **Transport**: UDP if both support it, otherwise TCP.
3. **Resolution**: the largest of 1440 / 1080 / 720 / 540 / 480 *short side* that is ≤ the preference, ≤ the source
   (never upscales), and fits both devices' codec limits (checked in either orientation). Width/height keep the
   source aspect ratio and are rounded to the encoder alignment (`fit_short_side`), so portrait phones, ultrawide
   monitors and foldables are never distorted.
4. **FPS**: min(preference, both codec limits, display refresh rate).
5. **Bitrate**: bits-per-pixel heuristic (0.09 bpp balanced for H.264; HEVC/AV1 scale down), clamped by both devices'
   maximum, then owned by the adaptive engine.
6. **Audio**: Opus if both have it, else AAC-LC, else none.

Failures are explicit: no common codec → "The receiver cannot decode this stream" (`Failed`), no fallback to a made-up
mode.

## Adaptive ladder (what the engine may move between)

| Profile | Ladder (short side × fps), best → worst |
|---|---|
| Balanced / Best quality | 1440p60 → 1080p60 → 1080p30 → 720p30 → 540p30 |
| Lowest delay | 1440p60 → 1080p60 → 720p60 → 720p30 → 540p30 (keeps 60 fps as long as possible) |
| Save power | 1080p30 → 720p30 → 540p30 |

The ladder is cut at the negotiated ceiling (device limits, display refresh rate). The default start is the top rung
(1080p60 on most phones; 1440p60 only when both ends really support it and the user's profile allows).

Per-tier bitrate range: floor 0.035 bpp, start 0.07–0.11 bpp, ceiling 0.12–0.22 bpp (by profile) × pixel rate,
capped by both devices' max bitrate.

## Decoder side

`DecodeLoop` selects the hardware decoder the same way (`CodecCaps.best(codec, encoder=false)`), configures it with
`KEY_LOW_LATENCY` (API 30+), `KEY_PRIORITY=0`, `KEY_OPERATING_RATE=fps` and the Qualcomm vendor low-latency key, and
renders to the `Surface`. A software decoder is used only if no hardware decoder exists; the stats overlay shows
"(software)" in that case. A decoder error or stall requests a keyframe and recreates the decoder.

## HDR / colour

v1 streams SDR BT.709 limited range end-to-end (Windows converts on the GPU with the video processor; Android passes
the surface through the hardware encoder). `Capabilities.hdr` is always `false` and no HDR path is advertised, so
there is no fake HDR and no CPU tone-mapping. HDR (10-bit HEVC/AV1, PQ) is a v1.2 item.
