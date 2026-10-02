# Revizor protocol v1

All integers are little-endian. Implementation: `core/crates/revizor-proto`. A peer announces
`min_version..=max_version` in its hello; the responder picks the highest common version or rejects with a
`Version` reason that the UI shows as "incompatible version, update both apps".

## Datagram envelope (16 bytes, cleartext, authenticated as AEAD associated data)

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | magic `0x52` (`R`) |
| 1 | 1 | protocol version |
| 2 | 1 | kind: `1` handshake, `2` pairing, `3` data |
| 3 | 1 | flags (reserved) |
| 4 | 4 | `session_id` (0 before a session exists) |
| 8 | 8 | `packet_seq` (per direction, starts at 1, never reused within a session) |

`kind=3` body = `ChaCha20-Poly1305(key_dir, nonce = salt(4) ‖ seq(8), aad = header)(channel(1) ‖ payload) ‖ tag(16)`.
Maximum datagram size is 1400 bytes, so the largest media payload per packet is
`1400 − 16 − 1 − 16 − 20 = 1347` bytes.

Channels (first plaintext byte): `1` control, `2` video, `3` audio, `4` video FEC.

## Media header (video and audio, 20 bytes)

`epoch u16 · frame_id u32 · pkt_idx u16 · pkt_count u16 · flags u8 · pts_us u64 · fec_k u8`

* `epoch` changes whenever resolution/fps/codec/bitrate class changes; the receiver never mixes epochs.
* `flags`: `1` keyframe, `2` in-band codec config present, `4` retransmitted copy.
* `pts_us` is the **capture time on the sender's monotonic clock**. It is used for latency measurement, jitter
  estimation and audio/video alignment — it is not a codec DTS/PTS. (Encoders are configured without B-frames, so
  decode order = display order and no DTS is needed.)
* `fec_k`: data packets per parity packet for this frame (0 = no FEC).

## FEC header (channel 4, 14 bytes + parity payload)

`epoch u16 · frame_id u32 · group u16 · groups u16 · pkt_count u16 · xor_len u16`

Data packet *i* belongs to group `i mod groups`; the parity payload is the XOR of the (zero-padded) payloads of its
members and `xor_len` is the XOR of their lengths. Consecutive packets fall into different groups, so a burst of up
to `groups` lost packets is recoverable without any round trip.

## Control messages (channel 1)

| Type | Message | Direction |
|---|---|---|
| 1 | `Params(StreamParams)` — epoch, codec, size, fps, bitrate, GOP hint, audio params | sender → receiver (sent 3×, plus on `ParamsRequest`) |
| 2 | `KeyframeRequest{epoch, reason}` — start, packet loss, decoder error, reconnect, config change | receiver → sender |
| 3 | `Nack{epoch, frame_id, missing[]}` | receiver → sender |
| 4/5 | `Ping{id,t0}` / `Pong{id,t0,t1,t2}` — NTP-style clock offset + RTT | both |
| 6 | `Report` — interval, packets expected/lost/recovered, jitter, receive bitrate, frames complete/dropped, decode µs, capture→screen latency µs, highest frame, CPU %, thermal | receiver → sender, every 500 ms |
| 7 | `Bye(reason)` | both |
| 8 | `ParamsRequest` | receiver → sender |

Unmeasured report fields are `255` (u8) or `0` (µs) and are displayed as "—", never as a number.

## Handshake (kind 1) — see SECURITY.md for the proof

```
Initiator (sender)                                Responder (receiver)
  ClientHello(1): ver_min, ver_max, eph_c(32), nonce_c(16), id_c(32), caps_c   ─►
                                                  ◄─ ServerHello(2): ver, session_id, eph_s, nonce_s, id_s, caps_s, Sig_s(H(CH‖SH))
  ClientFinish(3): Sig_c(H(CH‖SH))                ─►
                                                  ◄─ Accept(5): HMAC(confirm_key, H)      (or Reject(4): reason)
```

`H = SHA-256(ClientHello ‖ ServerHello-without-signature)`. Keys:
`HKDF-SHA-256(salt = H, ikm = X25519(eph_c, eph_s), info = "revizor v1 session keys")` →
`k_i2r(32) ‖ k_r2i(32) ‖ salt_i2r(4) ‖ salt_r2i(4) ‖ confirm(32)`.
Reject reasons: 1 not paired, 2 version, 3 busy, 4 malformed.

`caps` = device name, platform, per-codec `{max_w, max_h, max_fps, hardware}`, audio codecs, transports, HDR flag
(always false in v1), max bitrate.

## Pairing (kind 2) — SPAKE2 over Ed25519 with the PIN

```
P1  sender → receiver : spake_a
P2  receiver → sender : spake_b, id_r, name_r, HMAC_K("R" ‖ a ‖ b ‖ id_r ‖ name_r)
    (sender verifies the MAC, then stores the receiver's identity)
P3  sender → receiver : id_s, name_s, HMAC_K("S" ‖ a ‖ b ‖ id_s ‖ name_s)
P4  receiver → sender : ok            (receiver stores the sender's identity)
```

SPAKE2 identities are the strings `revizor-sender` / `revizor-receiver`. The receiver allows 5 starts per displayed
PIN, then locks the pairing window.

## Discovery (UDP, port 47720)

`"RVZD" ‖ kind` — kind 1 `Probe` (version), kind 2 `Announce`: protocol version, device id, name, media port,
transport mask, codecs, max size/fps, `pairing_open`. Announcements are unauthenticated hints; trust comes only from
the handshake. Media/receiver default port: 47721.

## Frame delivery rules (receiver)

* Strictly in order; a frame waits at most 60 ms (keyframes 150 ms) for repair, then it is **abandoned**.
* After an abandoned frame, delta frames are **not** passed to the decoder (they would corrupt the picture); the
  receiver requests a keyframe (immediately, then every 250 ms) and resumes on the next complete keyframe.
* NACK only when loss is *known* (a later packet of the same frame or a later frame has arrived, or the frame went
  idle), after 2 ms (6 ms while parity is still due), at most 4 times, spaced by ≈ 1.5 × RTT.
* The sender answers NACKs from a 500 ms / 8 MB history, using at most 25 % of the stream bitrate.
