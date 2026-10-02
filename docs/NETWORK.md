# Network architecture

## Topology

Direct device-to-device on the LAN. There is no server, relay or account.

```
 Android / Windows sender ── UDP (encrypted datagrams) ──► receiver :47721
        ▲  ▲                                                   │
        │  └──────── control + feedback (same socket) ◄────────┘
        └── discovery probe  ─► UDP broadcast :47720  ─► announcements
```

* **Media/control**: one UDP socket per side, media port default `47721`. All traffic is AEAD-encrypted datagrams of
  ≤ 1400 bytes (no IP fragmentation on typical 1500-MTU paths). The socket asks for 4 MiB receive / 2 MiB send buffers
  and DSCP AF41 (video class) so Wi-Fi WMM places it in the video queue (best effort; ignored where unsupported).
* **Discovery**: sender broadcasts a probe to the limited broadcast and every interface's directed broadcast; receivers
  answer and also announce every 2 s. Android holds a `MulticastLock` so the Wi-Fi chip does not filter these frames.
  Manual connect by IP exists for networks where broadcast is blocked (Windows → Advanced → "Connect by IP").
* **Transports** (`Transport` trait): UDP (default, lowest latency, controlled loss recovery) and TCP (length-prefixed
  datagrams; fallback when UDP is blocked and the way to use USB, see below). QUIC is intentionally *not* implemented
  yet; the trait is where it would plug in.

## Wi-Fi, Ethernet, USB

* **Wi-Fi / Ethernet**: same code path. Wired Ethernet on either end gives the lowest jitter; 5 GHz/6 GHz Wi-Fi is
  strongly preferred over 2.4 GHz. The engine measures loss, jitter and queueing delay and reacts; it cannot create
  bandwidth that the radio does not have.
* **USB**: there is no separate USB protocol. Two supported ways, both using the existing transports:
  1. *USB tethering* (RNDIS/NCM) from the phone gives the PC an IP link over the cable; discovery and UDP work as usual.
  2. *adb reverse* for development: `adb reverse tcp:47721 tcp:47721`, run the receiver with `--tcp`, connect to
     `127.0.0.1` with the TCP transport. TCP's head-of-line blocking makes it worse than UDP under loss, but a USB link is
     normally loss-free.
  A custom USB accessory/AOA transport is not implemented.

## Loss recovery strategy (ordered by cost)

1. **Pacing** avoids self-inflicted loss: packets of a frame are spread at 3× the video bitrate (token bucket with a
   24-packet burst credit so coarse OS timers do not reduce throughput).
2. **FEC** (XOR, interleaved): costs `1/k` bandwidth, no latency. `k` follows measured loss
   (off < 0.3 %, 16, 10, 6, 4 at 1 / 3 / 8 %), raised quickly and relaxed slowly.
3. **NACK/retransmit**: only for *known* losses FEC could not repair; ≤ 4 tries; ≤ 25 % of the stream bitrate.
4. **Keyframe request** when a frame is given up on: a single lost packet never stalls the picture; the worst case is
   one keyframe round trip (typically 50–200 ms of frozen picture, then a clean image).

## Congestion control

Per 500 ms report the engine uses: smoothed loss, queueing delay (recent-minimum RTT − 30 s baseline), receiver
throughput, drops, encoder/decoder load. It reduces bitrate first (×0.85 per 600 ms, ×0.7 for > 10 % loss), grows
additively after 2 s of clean feedback, changes resolution/fps only after bitrate has sat at the floor for 3 s and
never twice within 5 s, and upgrades only after ≥ 20 s of clean network with encoder/decoder headroom. An upgrade
that is followed by a downgrade within 30 s doubles the next upgrade wait (up to 5 min), which prevents
1440p → 1080p → 1440p flapping.
