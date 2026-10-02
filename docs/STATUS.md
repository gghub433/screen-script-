# Implementation status

Legend: ✅ implemented **and exercised by automated tests** · 🔷 implemented and **type-checked** against the real
platform API, but never run on real hardware by the authors · 📄 designed/documented only · ⛔ not implemented.

## Core (Rust) — `core/`

| Area (spec §) | Status | Notes |
|---|---|---|
| Wire protocol v1, versioning, capability exchange (§46, §4) | ✅ | version range negotiation, explicit incompatible-version error |
| Packet IDs, frame IDs, timestamps, loss detection (§15, §16) | ✅ | |
| Loss recovery: interleaved XOR FEC, NACK, keyframe request, bounded waits (§15, §17) | ✅ | single lost packet never stalls the stream; no keyframe storms (tested) |
| Pacing, jitter/loss/rate meters (RFC 3550 jitter) (§9, §32) | ✅ | |
| Transport abstraction; UDP; TCP (§14) | ✅ | QUIC ⛔, dedicated USB transport ⛔ (see NETWORK.md for USB via tethering / adb reverse) |
| LAN discovery (§11) | ✅ | UDP broadcast; manual IP supported by the Windows UI |
| Pairing with PIN, pinned identities, forget device (§12) | ✅ | SPAKE2; wrong-PIN/lock-out tests |
| Encryption, auth, session keys, replay protection (§13) | ✅ | MITM/impostor/forgery/replay/tamper tests |
| Auto-reconnect with re-handshake + keyframe (§31) | ✅ | tested with a 4 s blackout and in the soak run |
| Adaptive Streaming Engine: bitrate, FEC, tiers, hysteresis, cooldown, dwell, anti-flap (§7, §8) | ✅ | 19 scenario tests |
| Thermal / battery / overload adaptation logic (§21, §22) | ✅ logic · 🔷 inputs | inputs come from Android APIs (type-checked, not run) |
| Orientation / resolution change without distortion (§19, §20) | ✅ | epoch change + `fit_short_side`, tested end-to-end |
| Real statistics only (§32) | ✅ | unmeasured values are `None`/`—` |
| Long-run stability (§48–49) | ✅ short · ⛔ hours | 200 s fault-injection soak passed (see TESTING.md); multi-hour runs on real devices not done |
| HDR (§38), AV1 (§55) | ⛔ | `hdr` is always advertised as false; AV1 id reserved in protocol, apps do not offer it |

## Windows sender — `core/crates/revizor-win-sender`

| Item | Status |
|---|---|
| Monitor/window enumeration, multi-monitor selection (§24, §26) | 🔷 |
| Windows.Graphics.Capture (GPU frames, no GDI) (§24) | 🔷 |
| GPU BGRA→NV12 + scaling (D3D11 video processor) (§23, §39) | 🔷 |
| Hardware H.264 via Media Foundation (NVENC/AMF/QuickSync through the driver MFT), software fallback reported (§25) | 🔷 |
| High-refresh game sources (144/240 Hz) streamed at ≤ 60 fps without queueing (§27) | 🔷 (frame skipping logic) |
| Local web UI, pairing, discovery, live stats (§50) | ✅ API/UI exercised against the real receiver CLI over UDP; capture path 🔷 |
| Audio capture (WASAPI loopback) | ⛔ (not advertised) |
| Game-specific/exclusive-fullscreen capture hooks (§27) | ⛔ (WGC only) |

## Android app — `android/`

| Item | Status |
|---|---|
| MediaProjection → VirtualDisplay → hardware MediaCodec Surface path, zero CPU pixel copies (§3, §23) | 🔷 |
| Capability probing from `MediaCodecList`, H.264 mandatory, HEVC optional (§4) | 🔷 |
| Live bitrate / keyframe control, rotation handling (§8, §19) | 🔷 |
| System audio + microphone → AAC, A/V alignment (§18) | 🔷 |
| Thermal + battery signals into the engine (§21, §22) | 🔷 |
| Receiver: hardware decode to SurfaceView, aspect-correct, fullscreen, stats overlay (§28–30) | 🔷 |
| Benchmark of encode/decode for 720p30…1440p60 with real measurements (§34) | 🔷 |
| Diagnostics, log export without content (§33) | 🔷 |
| In-app update check/download/verify/install (see UPDATES.md) | 🔷 |
| Compose UI, Gradle build, resources | 📄 written, **never compiled** (Google Maven unreachable in the authoring environment) |
| JNI bridge | ✅ builds for host and passes `cargo check` for `aarch64`/`armv7` Android; Kotlin↔Rust symbol parity checked by script; never loaded in a real app process |

## Not started

Windows/macOS/Linux/iOS/TV receivers (Android receiver also covers Android TV in principle, untested), QUIC, USB
accessory transport, HDR, AV1, Windows audio, Keystore/DPAPI key wrapping, a store listing/installer, a UI localisation layer.

## How to read "type-checked"

For Windows: `cargo check --target x86_64-pc-windows-gnu` against `windows 0.58`. For Android: the platform-API Kotlin
(everything except the Compose UI and WorkManager glue) compiles against the Android 14 SDK classes (`tools/android-typecheck.sh`).
That proves the API usage compiles; it does **not** prove runtime behaviour on any GPU, codec or phone. Expect device-specific
fixes during first bring-up (codec quirks, alignment, timestamps, permissions).
