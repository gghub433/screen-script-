# Casting to a TV with nothing installed on it

Revizor has two ways to put a screen on another device:

| | **Revizor receiver** (app on the other device) | **Standard TV** (nothing installed) |
|---|---|---|
| Needs an app on the receiver | yes | **no** |
| Pairing | once, with a 6-digit PIN | none |
| Delay | designed for ≪ 200 ms; the core itself adds ≈ 4 ms in simulation, real glass-to-glass numbers still have to be measured on devices ([TESTING.md](TESTING.md)) | **typically 1–6 s, set by the TV's own buffering** |
| Picture adapts to the network | yes (bitrate, fps, resolution, FEC) | **no** (fixed ≤ 1080p, 30 fps) |
| Loss repair | FEC + selective retransmit | none — the TV buffers or stutters |
| Encrypted end to end | **yes** | **no** — plain HTTP on your LAN, limited to the TV |
| Works with | Android phones/tablets/TVs running Revizor | Chromecast, Android TV / Google TV, and DLNA smart TVs |

Both appear in **one list** in the Android app and in the Windows sender. A device that runs Revizor *and* is already
paired is listed once, as the Revizor entry. Everything else that answers as a TV is tagged **No app needed**.

Use the Revizor receiver when delay or privacy matter (games, video calls, anything sensitive). Use a standard TV when you
just want to put slides, photos or a video on a TV *now*.

## What the user does

1. Phone and TV (or PC and TV) on the same Wi-Fi / network.
2. Open Revizor → the list fills in by itself (it keeps searching while the screen is open).
3. Tap the TV → **Start sharing**. Android shows its own "start recording or casting?" consent; on Windows the first cast may
   trigger a firewall prompt — choose *Private networks* / allow.
4. To stop: **Stop sharing** (or press stop on the TV remote).

If the TV is not listed, see "If the TV does not show up" below. As a last resort the Android app has a button that opens
**Android's own cast screen** (Smart View / Cast / Screen mirroring). That path is built into the phone, supports Miracast TVs
and is not controlled by Revizor at all.

## How it works

```
 capture ─► hardware H.264 ─► MPEG-TS muxer ─► small HTTP server on this device ◄── GET ── the TV's own player
 (Android: MediaProjection→MediaCodec; Windows: WGC→D3D11→MF)       │
                                                                    └─ control: tells the TV which URL to play
                                                                       Google Cast (CASTV2 over TLS :8009)   or
                                                                       DLNA/UPnP  (SOAP AVTransport over HTTP)
```

The encoder is the same hardware H.264 path as for Revizor receivers, with three differences that TVs need: a keyframe
**every second** (HLS segments can only start at keyframes), a fixed 30 fps, and a fixed size (≤ 1080p, aspect ratio kept;
if the source changes size or rotates, the picture is letterboxed into the same size instead of renegotiating).
Bitrate = pixels × 30 fps ÷ 10 clamped to 3–8 Mbit/s (Windows) or 2.5–8 Mbit/s (Android; falls back to 720p if the encoder
cannot do 1080p30).

### 1. Discovery (`revizor-cast::discover`)

* **Google Cast:** mDNS query for `_googlecast._tcp.local` from every local IPv4 address, sent from an ephemeral port (a
  "legacy unicast" query per RFC 6762 §6.7, so responders answer by unicast and no multicast *receive* lock should be needed —
  not yet verified on real phones). Follow-up queries fetch SRV/TXT/A records when the answer was incomplete. TXT `fn` is the
  friendly name, `md` the model, and the `ca` capability bit excludes audio-only speakers.
* **DLNA / UPnP:** SSDP `M-SEARCH` for `urn:schemas-upnp-org:device:MediaRenderer:1`, then each device description is
  fetched and parsed; only devices that really expose an `AVTransport` service are kept.
* Results are merged **by IP address**: one TV that speaks both protocols is one entry, Cast first (HLS is the most robust
  path on Cast devices), DLNA second.
* Scans repeat while the setup screen is open; an entry stays for 30 s after the last answer so multicast packet loss does
  not make the list flicker.

### 2. The stream the TV pulls (`ts.rs`, `hls.rs`, `server.rs`)

* **MPEG-TS** muxer: PAT/PMT before every keyframe and at least every 100 ms, a PCR on every video access unit, an
  access-unit delimiter in front of every frame, SPS/PPS repeated on every IDR, optional AAC (ADTS) audio. Timestamps follow
  the capture clock with `PTS = capture + 150 ms` and `PCR = capture`, so a decoder needs to buffer only ≈ 150 ms (file-oriented
  muxers use ≈ 0.7 s). The streams have no B-frames, so PTS = DTS. When the screen is static and the encoder produces nothing,
  the muxer sends PSI plus PCR-only keep-alive packets so the TV's clock keeps running; additionally, in TV mode the Windows
  pipeline re-encodes the last frame after 200 ms of silence, and Android forces a keyframe every second.
* **HLS** for Cast: 1-second segments cut at keyframes, a sliding window of 6, playlist published once 2 segments exist,
  no `EXT-X-ENDLIST` (live), CORS enabled (Cast receivers fetch with XHR).
* **Progressive TS** for DLNA: `GET /<token>/live.ts` returns an endless close-delimited body with DLNA headers
  (`transferMode.dlna.org: Streaming`, `contentFeatures.dlna.org: DLNA.ORG_OP=00;DLNA.ORG_CI=0;…`). A TV that joins starts at
  the next keyframe. A viewer that cannot keep up is **dropped**, never allowed to slow the encoder (bounded queue per viewer,
  at most 4 viewers, at most 16 concurrent requests).
* The server listens on an ephemeral port on all interfaces, but answers **only** the TV's IP address and only for URLs that
  contain a 128-bit random token; anything else gets 403/404 and is counted as *blocked requests* in the diagnostics.

### 3. Telling the TV to play (`castv2.rs`, `upnp.rs`, `session.rs`)

* **Google Cast:** TLS to `<tv>:8009` → `CONNECT` → `LAUNCH` the **Default Media Receiver** (app id `CC1AD845`; this is a web app
  that the Chromecast loads from Google, so **the TV needs internet access** for this method — DLNA does not) → `LOAD` the
  HLS playlist as a `LIVE` stream → watch `MEDIA_STATUS` (`PLAYING`, `BUFFERING`, `IDLE`+reason) and heartbeat. Messages are
  hand-encoded protobuf (no code generation).
* **DLNA:** `Stop` (some renderers refuse a new URI while playing), `SetAVTransportURI` with a DIDL-Lite item, `Play`, then
  poll `GetTransportInfo` once a second and watch whether the TV actually opens our URL.
* **Fallback:** if the first method fails (cannot connect, refuses, never fetches within 20 s) the next one is tried and the
  user sees "That did not work, trying DLNA…". If every method fails the message says what happened ("accepted the command but
  never started playing. It may not support live network streams, or a firewall on this device is blocking it", "tried to fetch the picture from an unexpected address and
  was blocked", "Casting to … was ended on the TV").

## Delay: why 1–6 seconds, and what could change it

A TV is a *player*: it downloads the stream and decides how much to buffer before it shows anything, usually 2–3 segments
for HLS and a few seconds for progressive TS. Revizor can shorten its side (1 s segments, 150 ms mux delay, a new viewer starts at the next
keyframe, nothing is buffered on our side beyond the segment window) but it cannot reach into the TV's buffer. Expect:

* Chromecast / Google TV (HLS): typically 3–6 s.
* DLNA smart TVs (progressive TS): typically 1–4 s, but some models buffer much more.

There is no "glass-to-glass" number in the diagnostics for TV mode **because a TV cannot report one**; the app shows only what
it measures itself (stream bitrate, bytes delivered, TV connections, segments, requests, blocked requests, uptime).

## Quality, loss and Wi-Fi

There is no feedback channel from a TV, hence no adaptive bitrate, no FEC and no keyframe-on-loss. A weak Wi-Fi link shows up
as the TV buffering ("The TV is buffering…") or stuttering; the fix is a better link (5 GHz / Ethernet on the PC), not a
setting. 1080p30 at 3–8 Mbit/s is comfortable on any decent 5 GHz network.

* **Audio:** Android can send system sound (AAC) to the TV. Windows audio capture is not implemented yet, so Windows TV
  casts are video-only.
* **Not supported:** 4K, HDR, multiple TVs at once, remote control of the TV, casting over the internet, IPv6-only networks
  (discovery and the stream use IPv4).

## Security — what is and is not protected

| Question | Answer |
|---|---|
| Is the picture encrypted? | **No.** It is plain HTTP on your LAN, like any DLNA/Cast media server. Anyone who can sniff your network traffic (e.g. someone else on the same open Wi-Fi) can see it. Use the Revizor receiver when that matters. |
| Can another device on the network fetch it? | Not by asking: the server only answers the **TV's IP address** and a URL containing a fresh 128-bit random token, and logs refused requests. Someone who can both spoof the TV's IP *and* sniff the token is no better off than a sniffer. |
| Is the TV authenticated? | Partly. The Cast control connection is TLS, but Cast devices present device-specific certificates and Revizor does **not** implement Google's full device-authentication check (it accepts any certificate on port 8009). A LAN attacker impersonating a Chromecast could therefore receive the stream URL. DLNA control is unauthenticated plain HTTP by design. |
| What runs on my PC/phone? | One listening TCP port for the duration of the cast; it is closed when you stop. Windows will ask once whether to allow it on private networks — **allow it**, otherwise the TV cannot connect. |
| Does anything go to the internet? | Revizor itself: no. But with Google Cast the *TV* loads Google's Default Media Receiver app from the internet (and may talk to Google as Cast devices normally do). The picture itself never leaves your LAN. DLNA needs no internet. |

## If the TV does not show up

1. Same network? Guest Wi-Fi, "AP/client isolation" and separate Wi-Fi vs Ethernet VLANs hide devices from each other.
2. The TV must be **on** (not in standby with the network off). Some TVs only answer when the feature is enabled:
   Chromecast built-in (Android TV / Google TV), or the TV's network media / "DLNA", "media renderer", "screen sharing" or
   "AllShare / Smart View"-type setting. Names differ per brand and model year.
3. Windows: allow `revizor-sender.exe` on **Private** networks when the firewall asks (discovery replies and the video fetch
   both need it).
4. Press **Search again**; discovery needs up to ~3 s.
5. Still nothing: use **Android's built-in cast** button (Miracast / Smart View) or install the Revizor receiver.

See also [TROUBLESHOOTING.md](TROUBLESHOOTING.md#tv-casting-no-app-needed).

## Compatibility — expectations, not guarantees

**None of this has been run against a real TV by the authors.** The protocol code is exercised by simulated devices and by
ffmpeg as an independent client (see Testing below); real TVs are known to have vendor quirks. Treat the table as "what the
design should work with":

| Device | Method | Expectation |
|---|---|---|
| Chromecast, Chromecast with Google TV, Android TV / Google TV sets (Sony, Philips, TCL, Xiaomi, …) | Google Cast (HLS) | Best chance of working out of the box |
| Samsung, LG, Sony (non-Android), Panasonic, Hisense, Philips smart TVs | DLNA (progressive TS) | Works if the TV exposes a media renderer and accepts live MPEG-TS/H.264; some models/firmware refuse live streams or need a specific DLNA profile |
| Roku, Apple TV / AirPlay | — | **Not supported** (different protocols) |
| Miracast TVs and dongles | — | Not supported by Revizor; use Android's built-in cast button |

If a TV fails, the in-app message names the step that failed, and the "Blocked requests" / "Playlist requests" /
"Segment requests" counters in Diagnostics show whether the TV ever came back to fetch the picture.

## Testing

`cargo test -p revizor-cast` (58 tests) covers, without any TV:

* the MPEG-TS muxer and HLS segmenter, with a **real** H.264/AAC fixture decoded by `ffprobe`/`ffmpeg` (structure, strictly
  monotonic timestamps, keyframe flags, joining at a later keyframe);
* the HTTP server (token + IP allow-list, CORS, DLNA headers, progressive stream, bounded queues, slow viewer dropped);
* **ffmpeg playing our live HLS and progressive streams in real time** (≥ 60 / ≥ 80 decoded frames out of 4 s, quick start);
* SSDP, UPnP SOAP, DIDL-Lite, mDNS, CASTV2 protobuf framing, and TLS (a self-signed fake Chromecast over real TLS sockets);
* full sessions against **simulated TVs** (`FakeDlnaTv`, `FakeChromecast`): successful casts on both methods, refusal, a TV that
  never fetches, the user pressing stop on the remote, no picture from the encoder, Cast failing and falling back to DLNA,
  every method failing (each reason is reported), and a TV that fetches from an unexpected address (blocked and explained).

CI installs ffmpeg and sets `REVIZOR_REQUIRE_FFMPEG=1` so the decoder checks cannot be skipped silently.

## Code map

| Piece | Where |
|---|---|
| Rust: TS muxer, HLS, HTTP server, SSDP/UPnP, mDNS, CASTV2, session | `core/crates/revizor-cast/` (`testkit` feature: simulated TVs) |
| Android bridge | `core/crates/revizor-jni/src/cast.rs`; Kotlin `core/Core.kt` (`scanTvs`), `sender/FrameSink.kt` (`TvSink`), `sender/CaptureService.kt` (`beginTv`) |
| Windows | `revizor-win-sender/src/controller.rs` (`start_tv`), `pipeline.rs` (`TvSink`), `win/runner.rs` (`Mode::Tv`) |
| TLS | `rustls` + `ring` behind the default `tls` cargo feature (needs a C compiler for the target; `--no-default-features` is only for type-checking from Linux) |
