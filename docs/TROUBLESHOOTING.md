# Troubleshooting

The app shows a plain-language message; this table maps it to causes. Export **Settings → Export diagnostic log** for a
bug report (no screen/audio content inside).

| What you see | Likely cause | What to do |
|---|---|---|
| "No receivers found" | Different networks/VLANs, AP "client isolation", broadcast blocked, receiver app closed | Same Wi-Fi/SSID; disable AP isolation/guest network; on Windows use *Advanced → Connect by IP*; allow UDP 47720/47721 in the firewall (Windows asks on first run — choose *Private networks*) |
| "Pairing is off" | The receiver is not showing a PIN | On the receiver tap *Pair a new device*; the code is valid for 5 attempts |
| "Wrong code" | PIN typo, or an old PIN | Re-open pairing on the receiver for a fresh code. After 5 wrong tries the window locks |
| "This receiver is not paired…" | Trust was removed on one side | Pair again (remove the old entry under *Paired devices* first) |
| "…incompatible Revizor version" | Protocol versions do not overlap | Update both apps |
| "The receiver failed identity verification" | The device at that address is not the one you paired (new install, different device, possible impostor) | Re-pair only if you expected the change |
| "Reconnecting…" repeats | Weak Wi-Fi, device sleeping, router roaming | Move closer / use 5 GHz or Ethernet; the stream resumes by itself and gives up after 45 s with an error |
| Picture freezes ~½ s now and then | Packet loss that FEC could not repair; the receiver waits for a keyframe | Normal under interference; see Diagnostics → *Packet loss*, *Repaired packets*; switch to 5 GHz |
| Quality dropped | The explanation line says why (network / this device busy / receiver busy / temperature / battery) | It recovers automatically after ≥ 20 s of good conditions (≥ 45 s after temperature events) |
| "Software encoder" notice | No hardware H.264 encoder exposed by the device/driver | Update GPU drivers (Windows) / expect higher battery use (Android) |
| Black screen with protected content | DRM apps (Netflix, banking apps) set secure flags that block screen capture | Not a Revizor bug; Android refuses to give the content to any capture API |
| No sound | Sound sharing is off (Settings), or the app plays audio that opts out of capture | Settings → *Sound when sharing*; some apps disallow playback capture. Windows audio capture is not implemented yet |
| Stream stops when the screen turns off | Battery optimisation killing the service | Allow background activity for Revizor; the foreground notification must stay |
| Update does nothing | "Install unknown apps" not allowed, or the release has no APK + `.sha256` | Allow it when prompted; check the repository's latest release assets |
| "Update failed its integrity check" | Corrupted download or tampered file | Retry; if it persists, do not install |

## Windows specifics

* Needs Windows 10 version 1903+ for window/monitor capture. The yellow capture border is removable on Windows 11 only.
* Exclusive-fullscreen games may not be capturable by Windows Graphics Capture; use borderless/windowed fullscreen.
* Hybrid-GPU laptops: the encoder runs on the GPU that owns the capture device (the system default). Select the high-performance GPU for
  `revizor-sender.exe` in *Settings → System → Display → Graphics* if the game runs on the dGPU.

## Android specifics

* Android 14+ asks for screen-share consent **every time** and may offer "a single app" — pick "entire screen" for full-device mirroring.
* Some OEMs (aggressive battery managers) need *Unrestricted* battery usage for long sessions.
* Thermal status is reported by Android 10+ on devices that expose it; if the device always reports "none" the engine
  correctly has nothing to react to (it never invents a temperature).

## Reading the numbers

*Delay (screen to screen)* — measured at the receiver when the frame is rendered, using the clock synchronised with the sender.
*Round trip* — Ping/Pong. *Packet loss* — before repair. *Repaired packets* — rebuilt by FEC or retransmission.
"—" means not measured yet or not available on that platform.
