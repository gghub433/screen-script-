#!/data/data/com.termux/files/usr/bin/bash
# Build, sign and (optionally) install the Revizor Android APK directly on a phone, inside Termux.
#
#   tools/termux-build.sh                 build the APK            -> dist/revizor-<ver>-<code>.apk
#   tools/termux-build.sh --install       build, then open the installer
#   tools/termux-build.sh --download      do NOT build; fetch the newest signed release APK and install it
#
# Needs: Termux (from F-Droid/GitHub, not the old Play Store build), aarch64, ~6 GB free storage,
# 4 GB+ RAM, and an unrestricted internet connection for the first run.
#
# HONEST NOTE: building Android apps on a phone is heavy and depends on Termux package versions.
# The Rust core builds natively (Termux's rustc targets aarch64-linux-android, the same ABI Android
# apps use); Gradle runs on Termux's JDK with Termux's aapt2 substituted for Google's x86 one.
# If the Gradle step fails on your device, use --download (CI-built APK) instead.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFIX="${PREFIX:-/data/data/com.termux/files/usr}"
SDK="${ANDROID_SDK_ROOT:-$HOME/android-sdk}"
STATE="$HOME/.revizor-build"
MODE="build"; INSTALL=0
for a in "$@"; do
  case "$a" in
    --install) INSTALL=1 ;;
    --download) MODE="download" ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown option $a"; exit 2 ;;
  esac
done

say() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[ -d /data/data/com.termux ] || die "this script is meant to run inside Termux"
[ "$(uname -m)" = "aarch64" ] || die "an aarch64 phone is required (found $(uname -m))"
mkdir -p "$STATE" "$ROOT/dist"

open_installer() {
  local apk="$1"
  say "Opening the Android installer"
  echo "If Android says installing from Termux is blocked, allow it once in Settings > Apps > Special access > Install unknown apps > Termux."
  termux-open --view --content-type application/vnd.android.package-archive "$apk" || echo "Open $apk from your file manager."
}

# ───────────────────────── download mode ─────────────────────────
if [ "$MODE" = "download" ]; then
  say "Installing prerequisites"
  pkg install -y curl jq termux-tools >/dev/null
  REPO="$(grep -o 'gghub433/[A-Za-z0-9_.-]*' "$ROOT/android/app/build.gradle.kts" | head -1 || true)"
  REPO="${REVIZOR_REPO:-${REPO:-gghub433/screen-script-}}"
  say "Looking up the latest release of $REPO"
  json="$(curl -fsSL -H 'Accept: application/vnd.github+json' "https://api.github.com/repos/$REPO/releases/latest")" || die "no release found (is the repository public and has CI published one?)"
  apk_url="$(echo "$json" | jq -r '[.assets[] | select(.name | test("^revizor-.*\\.apk$"))][0].browser_download_url')"
  sha_url="$(echo "$json" | jq -r '[.assets[] | select(.name | test("^revizor-.*\\.apk\\.sha256$"))][0].browser_download_url')"
  [ "$apk_url" != "null" ] && [ "$sha_url" != "null" ] || die "the release has no APK + checksum"
  out="$ROOT/dist/$(basename "$apk_url")"
  curl -fL "$apk_url" -o "$out"
  want="$(curl -fsSL "$sha_url" | awk '{print $1}')"
  have="$(sha256sum "$out" | awk '{print $1}')"
  [ "$want" = "$have" ] || { rm -f "$out"; die "checksum mismatch - refusing to install"; }
  echo "verified $(basename "$out")"
  open_installer "$out"
  exit 0
fi

# ───────────────────────── build mode ─────────────────────────
say "Installing build tools (JDK 17, Rust, clang, aapt2, binutils)"
pkg update -y >/dev/null
pkg install -y openjdk-17 rust clang git curl unzip aapt2 binutils termux-tools >/dev/null
export JAVA_HOME="${JAVA_HOME:-$PREFIX/lib/jvm/java-17-openjdk}"

say "Android SDK (command-line tools, platform 34, build-tools 34)"
if [ ! -x "$SDK/cmdline-tools/latest/bin/sdkmanager" ]; then
  mkdir -p "$SDK/cmdline-tools"
  tmp="$(mktemp -d)"
  curl -fL "https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip" -o "$tmp/cmd.zip"
  unzip -q "$tmp/cmd.zip" -d "$tmp"
  mv "$tmp/cmdline-tools" "$SDK/cmdline-tools/latest"
  rm -rf "$tmp"
  termux-fix-shebang "$SDK"/cmdline-tools/latest/bin/* 2>/dev/null || true
fi
SDKM="$SDK/cmdline-tools/latest/bin/sdkmanager"
yes | "$SDKM" --sdk_root="$SDK" --licenses >/dev/null 2>&1 || true
"$SDKM" --sdk_root="$SDK" "platforms;android-34" "build-tools;34.0.0" >/dev/null

say "Building the Rust core for this phone (arm64-v8a)"
(
  cd "$ROOT/core"
  RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384" cargo build --release -p revizor-jni
)
LIB="$ROOT/core/target/release/librevizor_core.so"
[ -f "$LIB" ] || die "librevizor_core.so was not produced"
bad="$(readelf -d "$LIB" | awk '/NEEDED/ {gsub(/[\[\]]/,"",$5); print $5}' | grep -Ev '^(libc\.so|libm\.so|libdl\.so|liblog\.so)$' || true)"
[ -z "$bad" ] || die "the native library links against Termux-only libraries ($bad) and would crash on a normal Android app. Use --download instead."
mkdir -p "$ROOT/android/app/src/main/jniLibs/arm64-v8a"
cp "$LIB" "$ROOT/android/app/src/main/jniLibs/arm64-v8a/"
echo "native library ok: $(readelf -d "$LIB" | awk '/NEEDED/ {printf "%s ", $5}')"

say "Signing key"
KS="$STATE/release.jks"; ENVF="$STATE/keystore.env"
if [ ! -f "$KS" ]; then
  PASS="$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 24)"
  keytool -genkeypair -alias revizor -keyalg RSA -keysize 4096 -validity 36500 \
    -storepass "$PASS" -keypass "$PASS" -dname "CN=Revizor, O=Revizor" -keystore "$KS" >/dev/null
  umask 077; printf 'REVIZOR_KEYSTORE=%s\nREVIZOR_KEYSTORE_PASSWORD=%s\nREVIZOR_KEY_ALIAS=revizor\n' "$KS" "$PASS" > "$ENVF"
  printf '\033[1;33mIMPORTANT:\033[0m a new signing key was created in %s.\nBack up that folder. Android only accepts updates signed by the SAME key; if it is lost you must uninstall Revizor before installing a build signed with a new key.\n' "$STATE"
fi
set -a; . "$ENVF"; set +a

say "Building the APK with Gradle (first run downloads ~1 GB, can take 10-20 minutes)"
cd "$ROOT/android"
echo "sdk.dir=$SDK" > local.properties
CODE="$(git -C "$ROOT" rev-list --count HEAD 2>/dev/null || echo 1)"
NAME="$(sed -n 's/^VERSION_NAME=//p' version.properties)"
./gradlew --no-daemon assembleRelease -x lintVitalRelease \
  -Dorg.gradle.jvmargs=-Xmx1536m \
  -Pkotlin.compiler.execution.strategy=in-process \
  -Pandroid.aapt2FromMavenOverride="$PREFIX/bin/aapt2" \
  -Previzor.abis=arm64-v8a -PversionCode="$CODE"

SRC="$(ls app/build/outputs/apk/release/*.apk | head -1)"
OUT="$ROOT/dist/revizor-$NAME-$CODE.apk"
cp "$SRC" "$OUT"
(cd "$ROOT/dist" && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256")
say "Done: $OUT"
if [ -d "$HOME/storage/downloads" ]; then cp "$OUT" "$HOME/storage/downloads/"; echo "Copied to Downloads."; fi
[ "$INSTALL" = 1 ] && open_installer "$OUT" || echo "Install it with:  termux-open --view --content-type application/vnd.android.package-archive '$OUT'"
