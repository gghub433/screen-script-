# Streaming pipeline

```
CAPTURE → ENCODE → PACKETIZE → (FEC, pace) → ENCRYPT → TRANSPORT → DECRYPT → REASSEMBLE/REPAIR → DECODE → RENDER
```

## Android sender (zero-copy path)

1. `MediaProjection` → `VirtualDisplay` renders the display **directly into the encoder's input `Surface`**.
   No `Bitmap`, no `ImageReader`, no CPU copy of pixels exists on this path (pixels go compositor → codec).
2. `MediaCodec` (hardware, selected by `MediaCodecList`; software only if no HW encoder exists, and the UI says so):
   CBR, `KEY_PRIORITY=0` (real-time), `KEY_LOW_LATENCY` (API 30+), no B-frames, `prepend-sps-pps-to-idr-frames`,
   `repeat-previous-frame-after=100 ms` (a static screen still yields keyframes on demand), keyframes on request
   (`PARAMETER_KEY_REQUEST_SYNC_FRAME`) with a 10 s safety interval, live bitrate via `PARAMETER_KEY_VIDEO_BITRATE`.
3. The encoder callback hands the output `ByteBuffer` to the core through JNI as a **direct buffer** (one copy into
   the send queue, needed because the buffer is released right after).
4. Core: split into ≤ 1347-byte packets, add interleaved XOR parity, pace (token bucket at 3 × bitrate), AEAD-seal in
   place (one allocation per packet), send.

Resolution change / rotation: the core bumps the epoch, the service builds a new encoder, retargets the
`VirtualDisplay` (`resize` + `setSurface`), and the new encoder's first frame is a keyframe. Aspect ratio is always
derived from the real display size (`fit_short_side`), never assumed 16:9.

## Windows sender

1. `Windows.Graphics.Capture` (monitor or window; the same API games use) → frames arrive as GPU textures. No GDI.
2. A single-slot mailbox keeps only the newest frame; frames faster than the stream fps (e.g. a 144/240 Hz game) are
   skipped, never queued, so the capture cannot slow the game down.
3. `ID3D11VideoProcessor` converts BGRA → NV12 (BT.709 limited) and scales to the stream size **on the GPU**.
4. Media Foundation hardware H.264 MFT (NVENC / AMD / Intel Quick Sync, whatever the driver registers) is fed the
   NV12 texture through a DXGI device manager: no CPU readback. Software MFT with one staging copy is used only if
   no hardware encoder is registered, and is reported in the UI ("Software encoder").
5. Same core path as Android from here on.

## TV mode (Chromecast / DLNA, nothing installed on the TV)

Capture and the hardware encoder are the same; what differs is everything after them (full description in
[CASTING.md](CASTING.md)):

```
CAPTURE → ENCODE (fixed ≤1080p30, IDR every 1 s) → MPEG-TS MUX → HLS segments / progressive TS → HTTP → the TV's own player
```

* Android: the encoder is built with a 1 s I-frame interval and a keyframe is also requested every second, so the HLS
  segmenter can always cut; `repeat-previous-frame-after` keeps a static screen alive.
* Windows: `Mode::Tv` letterboxes the source into one fixed output size on the GPU, forces a keyframe every second and
  re-encodes the last frame after 200 ms of silence (a static desktop produces no new frames).
* There is no feedback from the TV, so there is no adaptive engine, FEC, NACK or latency measurement on this path; the only
  numbers shown are the ones the sender itself can measure (bitrate, bytes served, requests, uptime).

## Receiver (Android)

`receive thread` decrypts/reassembles → bounded queue → `DecodeLoop` feeds `MediaCodec` (hardware, low-latency flags)
whose output goes **straight to the `SurfaceView` surface** (`releaseOutputBuffer(render=true)`), i.e.
decoder → GPU → display with no bitmap. A bounded playout delay (3 × measured jitter, clamped to 0.25–2 frame
intervals; 0 in "Lowest delay") smooths frame pacing by scheduling `releaseOutputBuffer` against the sender
timestamps. The view keeps the stream's aspect ratio (letter/pillar-boxing, never stretching).

## Timing and latency measurement

* Each side measures clock offset and RTT with `Ping/Pong` (NTP formula, minimum-RTT sample of the last 30 s).
* **Capture→screen latency** = `local_now + offset − pts`, taken when `MediaCodec` reports the frame rendered
  (`OnFrameRenderedListener`). It is shown only after clock sync and is `—` otherwise.
* Encoder time (Android) = codec output time − surface capture timestamp (same `CLOCK_MONOTONIC`).

## Latency budget (low-latency mode, LAN)

| Stage | Typical (to be measured per device) |
|---|---|
| Capture + compose | ≤ 1 frame (8–16 ms) |
| Encode (HW, CBR, no B-frames) | 2–8 ms |
| Packetize + pace + encrypt | **≈ 3 ms measured** in the core (see TESTING.md) |
| Network (Wi-Fi 5/6) | 2–10 ms |
| Reassembly / playout buffer | 0 (Lowest delay) … 1–2 frames |
| Decode (HW) | 2–8 ms |
| Render (vsync) | ≤ 1 frame |

Only the "packetize → reassemble" row is something this repository has measured (in-memory link, release build);
the others are the design targets and depend on the phone, GPU and Wi-Fi. The app displays the real values.

## Audio

Android: system sound via `AudioPlaybackCapture` (API 29+) and/or microphone via `AudioRecord`, mixed in 16-bit PCM,
encoded to **AAC-LC 128 kbit/s** with `MediaCodec` (hardware where the device has it), ADTS framing, one frame per
packet, no FEC/NACK (loss is concealed by dropping). Receiver decodes with `MediaCodec` → `AudioTrack`
(`PERFORMANCE_MODE_LOW_LATENCY`) and schedules each chunk at `pts + video_latency` on the synchronised clock so lip-sync
follows the measured video latency. Windows audio capture is **not implemented yet** (the Windows sender does not
advertise audio).
