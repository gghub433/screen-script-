# Development setup

```bash
git clone https://github.com/gghub433/screen-script-.git && cd screen-script-/core
cargo test --workspace
```

## Try the whole stack without any phone

Two terminals on one machine (or two machines on a LAN):

```bash
# receiver: prints a pairing PIN, writes the received H.264 elementary stream to stdout
cargo run -p revizor-recv-cli -- --pair --name "Dev receiver" --out - | ffplay -f h264 -fflags nobuffer -i -

# Windows sender UI (on Windows) – or any sender build – then pick "Dev receiver", enter the PIN
```

`revizor-recv` prints once per second: fps, bitrate, loss, jitter, RTT, arrival latency, FEC/retransmit counts,
NACKs, keyframe requests, drops. Trusted devices: `--list-trusted`, `--remove <device id>`.

## Repository conventions

* `revizor-proto` stays free of I/O so the wire format is easy to review and fuzz.
* New behaviour in `revizor-media`/`-adaptive` needs a unit or scenario test; behaviour crossing threads/network gets an
  end-to-end test in `crates/revizor-session/tests`.
* No fake data: statistics are `Option`/`null` when unmeasured; test doubles exist only in tests (`sim` link, synthetic
  encoder/decoder in `e2e.rs`).
* Platform shells call the core only through `Native.kt` / the session API; keep the JNI surface in sync
  (`tools/android-typecheck.sh` checks symbol parity).

## Useful commands

```bash
cargo test -p revizor-adaptive                  # engine scenarios
cargo test -p revizor-session --test e2e        # full sessions over an impaired link (~15 s)
REVIZOR_SOAK_SECS=3600 cargo test --release -p revizor-session --test e2e soak -- --ignored --nocapture   # 1 h soak
tools/android-typecheck.sh                       # type-check Android platform code without the Android SDK
(cd crates/revizor-win-sender && cargo check --target x86_64-pc-windows-gnu)
```
