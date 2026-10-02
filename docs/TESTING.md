# Testing

## Automated suite (`cd core && cargo test --workspace`)

171 tests in the workspace (170 run by default + the opt-in soak), plus 7 in the standalone Windows sender project;
about 45 s of test time once compiled:

| Crate | Tests | What they prove |
|---|---|---|
| `revizor-proto` | 19 | byte-exact wire round trips, truncated/garbage input rejected, negotiation (codec choice, hardware preference, 1080p vs 1440p caps, portrait aspect), resolution/letterbox maths |
| `revizor-crypto` | 20 | handshake in both directions, **unpaired peers refused on both sides, MITM identity rejected, tampered hello/finish fail, version mismatch reported**, per-session keys, **replay/tamper/wrong-key/reflection rejected**, replay window across word boundaries, forged packets cannot poison the window, SPAKE2 pairing (right PIN pairs both ways; wrong PIN pairs nobody), attempt lock-out, trust store persistence |
| `revizor-media` | 29 | packetizer coverage, FEC overhead, **single and burst loss recovered by interleaved FEC (exact lengths incl. the short last packet)**, NACK retransmit, NACK rate/attempt limits, **no NACKs for a frame that is merely still being paced out**, tail-loss NACK, abandoned frame → keyframe request → deltas discarded → resume, repeated/rate-limited keyframe requests, epoch change, memory bound, history eviction, pacer (burst credit, long-run rate), jitter/loss/rate meters |
| `revizor-adaptive` | 20 | bitrate-before-tier ordering, dwell/cooldown, **no flapping on an oscillating link, flap penalty doubles the next upgrade wait**, gradual recovery, thermal steps + slow release, critical → floor, unknown thermal ignored, receiver overheating, low battery/charging, encoder/decoder overload with reason, custom locked tier/bitrate, FEC hysteresis and FEC-aware video bitrate, ladders per profile and ceiling, global bitrate cap |
| `revizor-transport` | 7 | UDP loopback, TCP framing/coalescing, impaired-link simulator (delay, loss %, bandwidth + queue, blackout), LAN discovery request/response over real sockets |
| `revizor-session` | 17 | clock sync maths; **end-to-end sessions** (see below); real-UDP loopback stream |
| `revizor-cast` | 58 | TV casting without an app: MPEG-TS muxer, HLS, HTTP server, SSDP/UPnP/mDNS/CASTV2, sessions against simulated TVs, **ffmpeg as an independent decoder/player** (see below) |
| `revizor-jni` | 1 | host build of the JNI crate (symbol parity with Kotlin is checked by `tools/android-typecheck.sh`) |
| `revizor-win-sender` (own project) | 7 | source id parsing; controller logic: TV entries need no pairing, a paired Revizor TV is listed once, remembered-for-30 s list merge, TV-gone error, one background scan at a time (the capture path is Windows-only) |

### TV casting tests (`crates/revizor-cast`)

* **`tests/mux_fixture.rs` (3):** a real libx264 + AAC fixture is muxed to MPEG-TS and `ffprobe` must report one H.264 and one AAC
  stream without complaints, `ffmpeg` must decode it with `-xerror`, timestamps must be strictly increasing at 1/30 s with
  keyframes flagged on exactly the IDR frames, and a stream that starts at a later keyframe (a TV joining late) must decode.
* **`tests/ffmpeg_client.rs` (2):** ffmpeg, acting as the TV, plays our **live** HLS playlist and the progressive TS in real
  time from the real HTTP server (frames counted, no decoder errors, quick start).
* **`tests/session_e2e.rs` (11):** whole sessions against `FakeDlnaTv` / `FakeChromecast`: DLNA end to end; a TV that refuses
  `Play`; a TV that never fetches (honest timeout message); stop pressed on the remote; no picture from the encoder; Chromecast
  end to end over HLS; Chromecast error and load failure; an unreachable Chromecast falling back to DLNA; every method failing
  (the user is told why *each* one failed); a TV fetching from an
  unexpected address (blocked and explained); discovery of a fake TV over real sockets.
* **`tests/cast_tls.rs` (1):** CASTV2 over real TLS against a self-signed fake Chromecast.
* **unit tests (41):** TS packets (CRC, PSI/AUD/PCR on IDR, cached SPS/PPS, continuity counters and exact payloads for large
  frames, stuffing for tiny ones, audio held back until a time base exists, repaired non-increasing timestamps, keep-alive only
  when idle); HLS (cuts only at keyframes, real durations, bounded window, never starts mid-GOP, runaway segment capped);
  server (HLS flow with CORS, token + IP allow-list, progressive viewer starts at a keyframe, HEAD, slow viewer dropped with
  bounded memory, viewer limit); XML/SOAP/DIDL escaping; SSDP and mDNS parsing (name-compression loops, audio-only speakers
  excluded, end-to-end discovery with follow-up queries); CASTV2 protobuf (garbage input, partial frames, oversized frames);
  merge-by-IP with Cast first.

ffmpeg tests skip with a message when ffmpeg is not installed; **CI installs it and sets `REVIZOR_REQUIRE_FFMPEG=1`** so they
cannot be skipped silently.


### End-to-end session tests (`crates/revizor-session/tests`)

They run the **real** sender and receiver sessions (handshake, encryption, packetization, FEC/NACK, adaptive engine,
reconnect) across an in-memory link with controllable impairment, plus one test over real UDP sockets. Only the
encoder and decoder are test doubles (a thread producing frames of the requested size/rate that obeys reconfigure / bitrate /
keyframe events; a thread verifying every delivered frame byte-for-byte).

* clean link: params arrive, **RTT and capture→screen latency are measured, not assumed**, ≤ 3 keyframes, 0 corrupted frames;
* 2 % random + burst loss, 3 ms jitter, reordering: ≥ 80 % of frames delivered intact, repair counters > 0, no keyframe storm;
* 4 s link outage: sender goes `Reconnecting`, comes back to `Streaming`, video resumes, receiver sees a second connection;
* bandwidth collapse to 3 Mbit/s: bitrate drops under the link rate, the stream keeps flowing, the UI-visible `limited_by` reason is set;
* unpaired receiver / unknown receiver: clear `Failed` message, no video;
* rotation (portrait ↔ landscape): new epoch, aspect ratio preserved exactly, frames of the new epoch decode;
* decoder error on the receiver → keyframe request reaches the encoder;
* PIN pairing end-to-end including wrong PIN and the 5-attempt lock-out; repeated start/stop does not hang.

## Soak / stress test (`soak`, opt-in)

```bash
REVIZOR_SOAK_SECS=3600 cargo test --release -p revizor-session --test e2e soak -- --ignored --nocapture
```

Streams 1080p60 and every 30 s applies the next fault: clean → 2 % random loss → burst loss → 6 Mbit/s bandwidth squeeze →
8 ms jitter + 2 % reordering → 4 s outage. Every 5 s it prints measured RSS, capture→screen latency, fps, buffered frames,
bitrate and resolution, and at the end it fails if: any corrupted frame was delivered, any outage did not recover, resident
memory grew by > 30 MB between the second and last quarter, or the receiver buffer exceeded its bound.

### Recorded soak run (200 s, release build, authoring machine, 4 vCPU)

40 samples, 6 fault phases, one 4 s outage: **0 corrupted frames, the outage recovered, resident memory stayed
4–6 MB (no growth), receiver buffer ≤ 4 frames**. Capture→delivery latency was 3.2–3.8 ms (median 3.6 ms) on clean and
lossy phases at 60 fps / up to 25 Mbit/s. Under the 6 Mbit/s squeeze the engine dropped to 30 fps and 720p (latency 22–33 ms)
and climbed back to 1080p60 after the faults ended. These are measurements of the core with synthetic frames over a simulated
link — **not** end-to-end glass-to-glass numbers on real phones; hours-long runs on real devices have not been done.

## Not covered by automated tests

Real encoders/decoders, GPU capture, Wi-Fi, thermal throttling on a phone, battery drain, USB tethering, sleep/wake of a
real device, the Compose UI and the Gradle build. **Real TVs and Chromecasts** (vendor quirks in DLNA/Cast, how much a given TV
buffers, which models accept live MPEG-TS) — ffmpeg and the simulated devices only prove that our streams and our side of the
protocols are well-formed. Also not covered: `ring`/TLS compiled for Android (built by the `android-native` CI job, not here),
the Windows firewall prompt. These need hardware; use the checklist below on first bring-up.

### Device bring-up checklist

1. Android → dev receiver on a PC: pair, stream 1080p30, then 1080p60; confirm `revizor-recv` fps/loss/latency lines.
2. Rotate the phone during streaming; stream must continue at the new aspect ratio.
3. Toggle Wi-Fi off/on for 10 s; stream must resume without user action.
4. Run *Test this device's video performance* and record the table.
5. Windows sender with a hardware GPU: confirm "GPU encoder" in the UI; switch between screen and window sources.
6. Leave a session running for hours with `adb shell dumpsys meminfo app.revizor` snapshots; check no growth.
7. Release build: publish a higher `versionCode` and confirm the in-app update installs.
8. **TV casting:** with a Chromecast / Google TV, then a Samsung/LG/Sony DLNA TV: confirm the TV appears in both apps, the cast
   starts, and note the real delay (film the TV next to a running clock); rotate the phone; turn the screen off/on; press Stop on
   the TV remote; check *Blocked requests* stays 0. Record which models work in `docs/CASTING.md`.
