#!/usr/bin/env bash
# Type-checks the Android Kotlin sources that only use the Android platform API (everything except the
# Jetpack Compose UI and WorkManager glue) against the real Android 14 API surface, using only Maven
# Central. This is NOT a build: it exists so the native-facing code can be verified where the Google
# Maven repository (AGP, Compose) is unreachable.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
A="$ROOT/android/app/src/main/java/app/revizor"
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
mkdir -p "$T/src/app/revizor"/{core,sender,receiver,benchmark,update}
cat > "$T/settings.gradle.kts" <<'K'
rootProject.name = "kcheck"
pluginManagement { repositories { gradlePluginPortal(); mavenCentral() } }
K
cat > "$T/build.gradle.kts" <<'K'
plugins { kotlin("jvm") version "2.0.20" }
repositories { mavenCentral() }
dependencies {
    compileOnly("org.robolectric:android-all:14-robolectric-10818077")
    compileOnly("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.1")
}
sourceSets { main { kotlin.srcDir("src") } }
K
cp "$A"/core/{Native,Events,Json,Core}.kt "$T/src/app/revizor/core/"
cp "$A"/sender/{CodecCaps,VideoEncoder,AudioCapture,DeviceMonitor,CaptureService,SenderController}.kt "$T/src/app/revizor/sender/"
cp "$A"/receiver/{VideoDecoder,AudioPlayer,ReceiverController}.kt "$T/src/app/revizor/receiver/"
cp "$A"/benchmark/Benchmark.kt "$T/src/app/revizor/benchmark/"
cp "$A"/update/{Updater,InstallResultReceiver,Prefs}.kt "$T/src/app/revizor/update/"
cat > "$T/src/app/revizor/Stubs.kt" <<'K'
package app.revizor
object BuildConfig { const val UPDATE_REPO = "x/y"; const val VERSION_NAME = "0"; const val VERSION_CODE = 1 }
class R { object string { const val capture_channel = 1; const val update_channel = 2; const val app_name = 3 } }
object RevizorApp { lateinit var core: app.revizor.core.Core }
K
cat > "$T/src/app/revizor/core/Log.kt" <<'K'
package app.revizor.core
object Log { fun i(t: String, m: String) {}; fun w(t: String, m: String) {}; fun e(t: String, m: String, th: Throwable? = null) {} }
K

cd "$T" && gradle compileKotlin --no-daemon -q
echo "Android platform-API Kotlin type-checks against Android 14."
# JNI symbol parity between Kotlin `external fun` and the Rust bridge
python3 - "$ROOT" <<'P'
import re, sys
root = sys.argv[1]
k = open(f"{root}/android/app/src/main/java/app/revizor/core/Native.kt").read()
r = open(f"{root}/core/crates/revizor-jni/src/lib.rs").read()
ext = set(re.findall(r"external fun (\w+)\(", k)); sym = set(re.findall(r"Java_app_revizor_core_Native_(\w+)", r))
assert ext == sym, f"JNI mismatch: kotlin-only={ext-sym} rust-only={sym-ext}"
print(f"JNI symbols match ({len(ext)}).")
P
