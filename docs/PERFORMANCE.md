# Performance and efficiency notes

Design rules that follow from the requirements (latency, CPU/GPU load, heat, battery, memory):

1. **No pixel ever touches the CPU** on the media path: Android `Surface → MediaCodec` and `MediaCodec → Surface`;
   Windows `WGC texture → GPU video processor → hardware MFT`. The CPU handles bitstreams (kilobytes), not pictures.
2. **One copy per direction** of the bitstream into/out of the core (direct `ByteBuffer`s through JNI), one allocation
   per packet (header + ciphertext + tag built in place).
3. **Bounded everything** (see ARCHITECTURE.md). No unbounded queue exists, so latency cannot accumulate and memory cannot
   grow during multi-hour sessions.
4. **No polling loops** on the hot path: blocking waits on sockets/condvars with short timeouts; timers are driven by the
   receive loop. The UI never touches media threads.
5. **Adaptive work reduction**: the engine lowers fps/resolution/bitrate when the device is hot (thermal status), low on
   battery or in power-save mode, or when encode/decode takes > 92 % of a frame interval, instead of letting the OS throttle abruptly.
6. **Crypto is cheap**: ChaCha20-Poly1305 (no AES-NI dependency), keys derived once per session, nonce = counter.
7. **FEC is a measured trade**: parity (≈ 1/k extra bytes) is enabled only when loss is observed, and removed slowly.
8. **Static screens cost almost nothing**: the encoder is asked to repeat the last frame every 100 ms (a few hundred bytes), and on Windows no frame is captured/encoded while nothing changes.

## What has been measured here

On the authoring machine (4 vCPU Linux, release build, in-memory link, synthetic 1080p60 frames at up to 25 Mbit/s, no real
encoder/decoder): capture-timestamp → frame delivered to the app ≈ **3.2–3.7 ms**, memory flat at ≈ 4–6 MB RSS across a
200 s run with injected loss, bursts, bandwidth squeezes, jitter/reordering and 4 s outages. See TESTING.md for the run.

## What must be measured on real devices (not yet done)

Encoder/decoder time per device, capture→screen latency over real Wi-Fi, CPU/battery/thermal behaviour over time.
Use *Settings → Test this device's video performance* (Android: encode/decode time, achieved fps, drops, bitrate,
CPU share, thermal state and charge-counter change for 720p30 … 1440p60) and the Diagnostics panels. GPU utilisation is not
exposed by Android APIs and is not shown.

## Tuning knobs

`Profile` (steers thresholds and ladders), `AssemblerConfig` (frame deadline 60 ms, NACK timing), `Pacer` factor (3×),
FEC thresholds, send queue (8 frames), decode queue (6 frames), playout delay (0 … 2 frames).
