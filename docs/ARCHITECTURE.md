# Architecture

Revizor is a real-time screen-streaming engine with thin platform shells. The shells talk to hardware
(capture, encoder, decoder, display). Everything else — protocol, security, packetization, loss repair, adaptive
control, sessions, discovery — is **one Rust core** shared by every platform.

```
                 ┌──────────────────────── platform shell ────────────────────────┐
 Android sender  │ MediaProjection → VirtualDisplay → Surface → MediaCodec (HW)    │
 Windows sender  │ Windows.Graphics.Capture → D3D11 VideoProcessor → MF HW H.264   │──encoded frames──┐
                 └──────────────────────────────────────────────────────────────────┘                  │
                                                                                                       ▼
 ┌─────────────────────────────────────────── Rust core ─────────────────────────────────────────────────────┐
 │ revizor-session   sender / receiver state machines, threads, reconnect, clock sync, statistics             │
 │ revizor-adaptive  Adaptive Streaming Engine (bitrate / FEC / tier; thermal, battery, overload)             │
 │ revizor-media     packetizer · XOR-FEC · NACK history · pacer · frame assembler · jitter/loss meters       │
 │ revizor-crypto    Ed25519 identity · SPAKE2 PIN pairing · authenticated key exchange · ChaCha20-Poly1305   │
 │ revizor-proto     wire formats, capabilities, negotiation, discovery messages (no I/O)                     │
 │ revizor-transport Transport trait · UDP · TCP · LAN discovery · impaired-link simulator (tests)            │
 └─────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
                                                                                                       │
                 ┌──────────────────────── platform shell ────────────────────────┐                  ▼
 Android receiver│ MediaCodec (HW decoder) → SurfaceView  ·  AAC → AudioTrack     │◄──encoded frames─┘
 Dev receiver    │ revizor-recv (CLI): writes the elementary stream, prints stats  │
                 └──────────────────────────────────────────────────────────────────┘
```

## Why a shared Rust core

* The hard, bug-prone parts (crypto, loss recovery, adaptation) exist **once** and are tested on a laptop with a
  simulated lossy network — 100+ automated tests — instead of being re-implemented per platform.
* Rust gives predictable latency (no GC pauses on the media path), memory safety for untrusted network input, and
  compiles for Android (`cdylib` + JNI), Windows (native) and, later, iOS/macOS/Linux/Android TV.
* The shells are intentionally small: they only exchange **encoded frames**, **device signals** (thermal, battery,
  encoder timing) and **commands** (reconfigure, set bitrate, force keyframe) with the core.

## Layers and their contracts

| Layer | Knows about | Does not know about |
|---|---|---|
| Capture + encoder (shell) | OS capture API, codec | network, crypto, adaptive logic |
| `revizor-media` | frames, packets, loss | sockets, clocks, UI |
| `revizor-crypto` | keys, handshakes | transport, video |
| `revizor-transport` | datagrams | what is inside them |
| `revizor-session` | all of the above, via traits | platform APIs |
| UI | session state + statistics | transport, encoder |

Because the UI only observes `SenderUi` / `ReceiverUi` state and the session only exchanges frames and events with
the shell, each of these can be replaced independently:

* **H.264 → HEVC → AV1**: the codec is a field in `StreamParams`; the Android encoder/decoder pick the MIME type
  from it. The core never parses the bitstream (it only needs the keyframe flag).
* **UDP → QUIC/other**: implement `Transport` (`send_to` / `recv_from`); nothing else changes.
* **New receiver platform**: implement "decode frames, render, report presentation"; use the same core.

## Threads

Sender (Android): capture is driven by the OS (VirtualDisplay → encoder), encoder callback thread →
`senderSubmitVideo` (queues, never blocks) → **media thread** (packetize, pace, encrypt, send) ·
**control thread** (handshake, reports, NACK, adaptive engine, reconnect) · stats poll on a handler thread · UI on
the main thread (never touches media).

Receiver: **receive thread** (decrypt, FEC/NACK reassembly, reports) → bounded frame queue → **decode thread**
(MediaCodec → SurfaceView) · **audio thread** · UI.

All queues are bounded (video send queue: 8 frames, receiver decode queue: 6 frames, assembler: 128 partial
frames, retransmit history: 500 ms / 8 MB). When a bound is hit the system drops and **resynchronises on a keyframe**
rather than letting latency grow. This is what keeps hours-long sessions from degrading.

## Product principle: automatic by default

The default path is *open app → pick the device → tap*. The Adaptive Engine chooses codec, resolution, fps, bitrate,
FEC and buffer size. Users only see a short, honest explanation when quality was reduced ("because of temperature
protection"). Profiles (Save power / Balanced / Best quality / Lowest delay) steer the engine's preferences; they are
*not* fixed presets. Manual values (Custom) exist in the core (`Profile::Custom`) and are reserved for advanced
settings.

## Design decisions (and the alternatives rejected)

| Decision | Why | Rejected |
|---|---|---|
| Custom UDP protocol with FEC + selective NACK | Lowest latency; repair costs ≤ 1 RTT only when FEC cannot fix; full control over pacing | RTP/WebRTC (heavy stack, ICE/DTLS not needed on a LAN, harder to embed on Windows), TCP (head-of-line blocking), QUIC (stream loss recovery adds latency; kept behind `Transport` for later) |
| XOR parity, **interleaved** groups | Recovers any single loss per group and bursts up to `groups` packets, tiny CPU cost, no extra latency | Reed–Solomon (more CPU, only worth it for >5 % loss), per-packet retransmit only (≥ 1 RTT for every loss) |
| Keyframe on unrecoverable loss (no reference invalidation) | Works with every MediaCodec/MFT; simple and robust | LTR/reference-invalidation (not exposed uniformly by Android encoders) |
| ChaCha20-Poly1305 | Fast on every ARM core without AES instructions → less heat/battery | AES-GCM |
| SPAKE2 PIN pairing + pinned Ed25519 identities | A 6-digit PIN is safe against offline brute force; later connections need no PIN | Plain PIN-keyed HMAC (offline-guessable), unauthenticated discovery trust |
| Sender runs the adaptive engine | It owns the encoder, thermal/battery state and bitrate control; receiver feeds real measurements back every 500 ms | Receiver-driven (cannot see encoder state) |
| Local web UI for the Windows sender | Cross-checks easily, small attack surface when bound to 127.0.0.1 with token + Host checks, no GUI-toolkit dependency | Win32/WPF/Electron (not needed for the MVP) |
| Android UI in Jetpack Compose | Current platform standard; same code for phone/tablet/TV | XML Views |

See [STATUS.md](STATUS.md) for what is implemented, what is only type-checked and what is not done.
