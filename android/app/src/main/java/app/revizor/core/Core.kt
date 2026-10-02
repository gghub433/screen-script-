package app.revizor.core

import android.content.Context
import java.io.File

data class Discovered(
    val id: String, val name: String, val ip: String, val port: Int, val pairingOpen: Boolean, val trusted: Boolean,
    val maxW: Int, val maxH: Int, val maxFps: Int,
)

data class TrustedDevice(val id: String, val name: String)

enum class PairResult { Ok, WrongPin, NoAnswer, Error }

/** Owns the device identity and the trust store (files in app-private storage). */
class Core(context: Context) {
    val handle: Long = Native.coreOpen(File(context.filesDir, "revizor").absolutePath).also {
        check(it != 0L) { "could not open the identity store" }
    }

    val deviceId: String get() = Native.coreDeviceId(handle)

    fun trusted(): List<TrustedDevice> = Native.coreTrusted(handle).lineSequence().filter { it.isNotBlank() }.map {
        val p = it.split('\t', limit = 2)
        TrustedDevice(p[0], p.getOrElse(1) { "" })
    }.toList()

    fun forget(id: String) = Native.coreForget(handle, id)

    /** Blocks for [timeoutMs]; call from a background dispatcher. */
    fun scan(timeoutMs: Int = 1500): List<Discovered> = Native.scan(handle, timeoutMs).lineSequence().filter { it.isNotBlank() }.mapNotNull {
        val p = it.split('|')
        if (p.size < 10) return@mapNotNull null
        Discovered(p[0], p[1], p[2], p[3].toIntOrNull() ?: return@mapNotNull null, p[4] == "1", p[5] == "1",
            p[6].toIntOrNull() ?: 0, p[7].toIntOrNull() ?: 0, p[8].toIntOrNull() ?: 0)
    }.toList()

    fun pair(d: Discovered, pin: String, myName: String): PairResult = when (Native.pairAsSender(handle, d.ip, d.port, pin, myName)) {
        0 -> PairResult.Ok
        1 -> PairResult.WrongPin
        2 -> PairResult.NoAnswer
        else -> PairResult.Error
    }
}
