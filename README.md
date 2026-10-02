# REVIZOR

Low-latency, encrypted screen streaming between **Android**, **Windows** and a **receiver** (Android phone/tablet/TV),
over the local network. Hardware encode → secure UDP → hardware decode → straight to the display, with an adaptive
engine that keeps the picture smooth and the device cool.

* **Simple for users:** open the app → pick the device → Share. Resolution, frame rate, bitrate, error protection
  and buffering are chosen automatically and shown honestly when quality is reduced ("because of temperature protection").
* **Serious underneath:** one Rust core (protocol, SPAKE2 pairing, authenticated encryption, FEC + NACK loss recovery,
  adaptive streaming engine, sessions) shared by every platform, with 110+ automated tests.
* **Private:** device-to-device only, end-to-end encrypted, no account, no relay, no analytics.

> **Read [docs/STATUS.md](docs/STATUS.md) first.** It says exactly what is tested, what is only type-checked and what is
> not implemented. In short: the core is built and tested here; the Windows sender and the Android app are written against
> the real platform APIs and type-checked, but have **not been run on real devices** and the Android Gradle/Compose build
> has **not been executed** (no Android SDK access where this was authored). Expect a bring-up round on hardware.

## Repository

| Path | What |
|---|---|
| `core/` | Rust workspace: `revizor-proto`, `-crypto`, `-media`, `-adaptive`, `-transport`, `-session`, `-jni`, `-recv-cli` |
| `core/crates/revizor-win-sender/` | Windows sender (Windows.Graphics.Capture → D3D11 → Media Foundation HW H.264) + local web UI |
| `android/` | Android app (Kotlin, Compose): sender, receiver, benchmark, diagnostics, updater |
| `tools/` | `termux-build.sh` (build/install the APK on a phone), `android-typecheck.sh` |
| `docs/` | Architecture, protocol, pipeline, encoder/decoder selection, network, security, build, testing, performance, troubleshooting, status, updates |

## Quick start

```bash
cd core && cargo test --workspace                       # run the test-suite
cargo run -p revizor-recv-cli -- --pair --out - | ffplay -f h264 -fflags nobuffer -i -   # development receiver
```

* **Windows sender:** `cd core/crates/revizor-win-sender && cargo build --release` (on Windows) → `revizor-sender.exe`.
* **Android:** build in Android Studio / CI (see [docs/BUILD.md](docs/BUILD.md)), or **on the phone with Termux**:
  `tools/termux-build.sh --install` (build) or `tools/termux-build.sh --download` (fetch the CI-built APK).
* **Automatic updates (Android):** the app checks this repository's latest GitHub Release, verifies the SHA-256 and hands the
  APK to Android's installer — see [docs/UPDATES.md](docs/UPDATES.md) for what can and cannot be automatic.

## Documentation

[Architecture](docs/ARCHITECTURE.md) · [Protocol](docs/PROTOCOL.md) · [Streaming pipeline](docs/PIPELINE.md) ·
[Encoder/decoder selection](docs/ENCODER_DECODER_SELECTION.md) · [Network](docs/NETWORK.md) · [Security](docs/SECURITY.md) ·
[Build](docs/BUILD.md) · [Development](docs/DEVELOPMENT.md) · [Testing](docs/TESTING.md) · [Performance](docs/PERFORMANCE.md) ·
[Troubleshooting](docs/TROUBLESHOOTING.md) · [Updates](docs/UPDATES.md) · [Status](docs/STATUS.md)

## Roadmap

MVP (this repo): Android + Windows senders, Android receiver, discovery, pairing, H.264 HW, 1080p30/60, audio (Android),
adaptive bitrate, low-latency mode, auto-reconnect, encryption, diagnostics.
Next: HEVC/1440p/USB hardening on real devices, Windows audio, benchmark-driven defaults, Keystore/DPAPI key storage.
Later: AV1, HDR, game-specific capture, QUIC, iOS/macOS/Linux/TV receivers.

License: Apache-2.0.
