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
| LAN discovery (§11) | ✅ | UDP broadcast; manual IP supported by the Windows UI; both UIs also list standard TVs (see CASTING section below) |
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
| JNI bridge (35 symbols) | ✅ builds for host and passes `cargo check` for Android (without TLS, see below); Kotlin↔Rust symbol parity checked by script; never loaded in a real app process |

## TV casting — nothing installed on the TV — `core/crates/revizor-cast` (see [CASTING.md](CASTING.md))

| Item | Status | Notes |
|---|---|---|
| MPEG-TS muxer (PSI, PCR, AUD, SPS/PPS repeat, AAC), HLS segmenter, bounded HTTP server with token + TV-IP allow-list | ✅ | a real H.264/AAC fixture is demuxed **and decoded by ffprobe/ffmpeg** (monotonic timestamps, keyframe flags, joining at a later keyframe) |
| A real media client plays our live streams | ✅ | ffmpeg plays the live HLS playlist and the progressive TS in real time (frames counted) — a stand-in for a TV, **not** a TV |
| Discovery: mDNS `_googlecast._tcp` + SSDP/UPnP `MediaRenderer`, merged by IP | ✅ simulated devices over real sockets · ⛔ never run on a real LAN with real TVs | |
| Google Cast control (CASTV2, TLS :8009, Default Media Receiver, HLS `LIVE`) | ✅ against a simulated Chromecast over real TLS · ⛔ not verified against a real Chromecast | no Cast device-authentication check (accepts any certificate) |
| DLNA control (`SetAVTransportURI` + DIDL-Lite, `Play`, `GetTransportInfo`) | ✅ against a simulated renderer · ⛔ not verified against a real TV | vendor quirks are expected |
| Method fallback (Cast → DLNA) and honest failure messages | ✅ | 11 session-level scenarios |
| Android: TV mode in the capture service, JNI bridge, unified list, "No app needed" tag, button to Android's built-in cast screen | 🔷 | JNI symbols match; Kotlin type-checks; `ring`/TLS for Android is built only by CI (`android-native`), Termux and the release job |
| Windows: unified list, TV start/stop, fixed 1080p30 pipeline (`Mode::Tv`), background search, live panel | ✅ controller logic (7 tests) and UI (rendered in headless Chromium) · 🔷 capture/encode path | |
| Audio to the TV | 🔷 Android (AAC) · ⛔ Windows | |
| Delay | **1–6 s, set by the TV** — by design, see CASTING.md | cannot be measured by us: a TV reports nothing |
| AirPlay, Miracast (the app itself), Roku, 4K, HDR, several TVs at once | ⛔ | Android's own cast screen covers Miracast |

## Not started

Revizor receivers for Windows/macOS/Linux/iOS (the Android receiver also covers Android TV in principle, untested), QUIC, USB
accessory transport, HDR, AV1, Windows audio, Keystore/DPAPI key wrapping, a store listing/installer, a UI localisation layer.

## How to read "type-checked"

For Windows: `cargo check --target x86_64-pc-windows-gnu --no-default-features` against `windows 0.58`. For Android:
`cargo check -p revizor-jni --target aarch64-linux-android --no-default-features`. (`--no-default-features` drops only the
TLS needed for Google Cast, because `ring` cannot be compiled for those targets without a C toolchain for them; the real
builds — MSVC on Windows, the NDK or Termux on Android — include it, and CI builds both.) For Android: the platform-API Kotlin
(everything except the Compose UI and WorkManager glue) compiles against the Android 14 SDK classes (`tools/android-typecheck.sh`).
That proves the API usage compiles; it does **not** prove runtime behaviour on any GPU, codec or phone. Expect device-specific
fixes during first bring-up (codec quirks, alignment, timestamps, permissions).
