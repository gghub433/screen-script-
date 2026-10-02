# Security model

**Goal:** only devices the user has paired can view or control a stream; nobody on the same Wi-Fi can read,
modify, replay or inject video/audio; nothing leaves the local network unless the user chooses so.

## Threat model

In scope: a passive or active attacker on the same LAN/Wi-Fi (sniff, spoof, replay, MITM, flood) and a malicious
device that wants to connect or to be streamed to. Out of scope: a compromised phone/PC (it can read its own
screen anyway), traffic analysis of packet sizes/timing, and physical access to an unlocked device.

## Mechanisms (all in `core/crates/revizor-crypto`)

| Property | Mechanism |
|---|---|
| Device identity | Long-term **Ed25519** key per device; `device_id = hex(SHA-256(pubkey)[..16])` |
| Pairing | **SPAKE2** (balanced PAKE) keyed by a 6-digit PIN shown on the receiver. A PAKE gives an attacker exactly one PIN guess per run and nothing to brute-force offline. After the shared key is derived each side proves it (HMAC over the transcript) while exchanging its identity key; both store the other in the trust list. The receiver allows 5 pairing starts per displayed PIN and then locks. |
| Authentication of every session | Authenticated key exchange: ephemeral **X25519** + Ed25519 signatures over the transcript hash. Each side checks the peer's identity against the trust list **before** signing/accepting; an unknown or substituted identity fails (`UnknownPeer` / `BadProof`). Discovery results are never trusted. |
| Confidentiality + integrity | **ChaCha20-Poly1305** per packet; the 16-byte header is authenticated as associated data; separate keys and nonce salts per direction, so a reflected packet cannot be accepted. |
| Forward secrecy | Fresh ephemeral keys each session: leaking a device's long-term key does not reveal past recordings. |
| Replay protection | 64-bit packet numbers (nonce) + a 2048-packet sliding window per receiver; the window is updated only **after** authentication, so forged packets cannot poison it. |
| Key confirmation | The responder's `Accept` is an HMAC under a key derived from the shared secret; the initiator verifies it before streaming. |
| Local DoS limits | The receiver caps handshake attempts (≈ 10/s) since signature verification is the costly step. |

CPU cost: ChaCha20-Poly1305 is implemented in the audited RustCrypto crates and is cheap on every ARM core (no
AES instruction dependency); at 25 Mbit/s the crypto is a few percent of one core.

## Privacy

* Screen and audio content only travels device → device, encrypted. There is no relay server and no account.
* The Android app's **only** internet request is the optional update check against the GitHub Releases API
  (a plain `GET` with the app's version in the User-Agent). It can be switched off in Settings → Updates.
  There is no analytics, crash reporting or telemetry SDK.
* Diagnostic logs contain states, parameters and errors — never frames, audio, window titles or IP addresses of
  third parties — and are only exported when the user taps "Export diagnostic log".
* Permissions: `INTERNET`/network state (LAN), `FOREGROUND_SERVICE_MEDIA_PROJECTION` (capture while the screen is
  off the app), `RECORD_AUDIO` only if the user enables microphone or system-sound sharing,
  `POST_NOTIFICATIONS` (the "you are sharing" notification), `REQUEST_INSTALL_PACKAGES` (only used by the updater).
  Android additionally shows its own "start recording or casting?" consent each time capture starts.

## Windows sender UI

The control UI is a local web page. It listens on `127.0.0.1` only, and every API call needs a random per-run token
plus a matching `Host` header, so other web pages (CSRF / DNS-rebinding) cannot drive the app.

## Updater trust

An update is installed only if (1) its SHA-256 matches the `.sha256` asset published next to it, **and** (2) Android
accepts it as an update to this app, which requires the same APK signing key. Downloads are HTTPS-only, restricted
to GitHub hosts, size-capped, and redirects are followed manually with the host re-checked each hop. Treat the
signing key like a password: anyone holding it can ship an update to every installed copy.

## Known limitations (honest list)

* Identity keys are stored as a file in app-private storage (`filesDir`), not wrapped by Android Keystore / Windows
  DPAPI yet. They are protected by the OS app sandbox only. *(Planned.)*
* No address-validation cookie in the handshake: a spoofed-source flood of `ClientHello` can make the receiver send
  unsolicited `ServerHello`s (amplification factor ≈ 1×) and burn some CPU (rate limited).
* Discovery announcements are unauthenticated and reveal device name, model-level capabilities and IP to the LAN.
* The 6-digit PIN protects *pairing*; during the pairing window anyone on the LAN can try 5 guesses (1 in 200 000 to
  succeed per window) — the user sees the window and the "Paired with …" confirmation on the receiver.
* Packet sizes and timing are visible and reveal activity (e.g. static screen vs video).
* There is no key rotation inside one session; the packet counter is 64-bit so nonce reuse is not a practical concern.
