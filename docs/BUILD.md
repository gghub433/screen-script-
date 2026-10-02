# Building Revizor

Repository layout:

```
core/                    Rust workspace (protocol, crypto, media, adaptive, transport, session, JNI, dev receiver)
core/crates/revizor-win-sender/   Windows sender (standalone Cargo project)
android/                 Android app (Kotlin, Jetpack Compose) – loads librevizor_core.so
tools/                   termux-build.sh, android-typecheck.sh
.github/workflows/       ci.yml, release.yml
```

## 1. Rust core + tests (Linux, macOS, Windows)

```bash
cd core
cargo test --workspace            # ~20 s: unit, scenario, end-to-end and UDP-loopback tests
cargo build --release -p revizor-recv-cli   # headless receiver for development
```

Needs Rust ≥ 1.75. No system libraries are required.

## 2. Windows sender

On Windows 10 1903+ (Windows 11 recommended) with the Rust MSVC toolchain:

```powershell
cd core\crates\revizor-win-sender
cargo build --release          # -> target\release\revizor-sender.exe
.\target\release\revizor-sender.exe
```

It starts a local UI on `127.0.0.1` and opens it as an Edge "app" window. Data directory: `%APPDATA%\Revizor`
(`REVIZOR_HOME` overrides). From Linux you can type-check it:

```bash
rustup target add x86_64-pc-windows-gnu
cd core/crates/revizor-win-sender && cargo check --target x86_64-pc-windows-gnu
```

## 3. Android app

### a) Android Studio / command line (recommended for development)

Prerequisites: JDK 17, Android SDK (platform 34, build-tools 34), Android NDK r26, Rust with Android targets and
[`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk).

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/26.3.11579264

# native library -> android/app/src/main/jniLibs/<abi>/librevizor_core.so
cd core
RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384" \
  cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o ../android/app/src/main/jniLibs build --release -p revizor-jni

# APK
cd ../android
./gradlew assembleDebug            # debug-signed, package app.revizor.debug
./gradlew assembleRelease          # needs the signing variables below
```

Release signing (the SAME key for every build, otherwise in-place updates are impossible):

```bash
keytool -genkeypair -alias revizor -keyalg RSA -keysize 4096 -validity 36500 -keystore release.jks
export REVIZOR_KEYSTORE=$PWD/release.jks REVIZOR_KEYSTORE_PASSWORD=...   # REVIZOR_KEY_ALIAS defaults to "revizor"
```

Version: `android/version.properties` holds `VERSION_NAME`; `versionCode` defaults to it and CI uses the git commit
count (`-PversionCode=$(git rev-list --count HEAD)`) so it only ever increases.

### b) On the phone with Termux

```bash
pkg install git
git clone https://github.com/gghub433/screen-script-.git && cd screen-script-
tools/termux-build.sh --install      # builds the Rust core natively, builds + signs the APK, opens the installer
# or, without compiling anything on the phone:
tools/termux-build.sh --download     # fetches the newest CI-built release APK, verifies its SHA-256, opens the installer
```

What the script does and its limits are documented at its top. In short: Termux's Rust compiles the JNI library for
`aarch64-linux-android` (same ABI as apps), Google's cmdline-tools provide the SDK platform/jars, Gradle runs on
Termux's JDK 17 with **Termux's `aapt2` substituted** (`android.aapt2FromMavenOverride`) because Google's `aapt2`
is x86-only. The first build downloads ≈ 1 GB and needs 4 GB+ RAM; if Gradle fails on your device use `--download`.
The script creates `~/.revizor-build/release.jks` once — **back it up**; builds signed with another key cannot
update an installed Revizor (Android would require uninstalling it first).

### c) GitHub Actions

`ci.yml` runs on every push (tests, Android/Windows type-checks). `release.yml` runs on tags `v*` (or manually) and
publishes the signed APK + `.sha256` and the Windows sender to a GitHub Release, which is what the in-app updater reads.
Add secrets `REVIZOR_KEYSTORE_B64` (`base64 -w0 release.jks`) and `REVIZOR_KEYSTORE_PASSWORD`.

## What has and has not been built

See [STATUS.md](STATUS.md). In short: the Rust core, dev receiver and tests are built and run in CI-like conditions;
the Windows sender and the Android Kotlin sources are **type-checked** against the real Windows/Android API surface,
but the Gradle/Compose build and everything on real hardware must be exercised by you (the authoring environment had no
Android SDK access).
