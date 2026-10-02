package app.revizor.sender

import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.os.Build
import app.revizor.core.Codec

/** Real capabilities read from MediaCodecList — nothing is assumed from the device model. */
object CodecCaps {
    data class Cap(val codec: Codec, val info: MediaCodecInfo, val maxW: Int, val maxH: Int, val maxFps: Int, val hardware: Boolean, val alignW: Int, val alignH: Int)

    private fun isHardware(i: MediaCodecInfo): Boolean =
        if (Build.VERSION.SDK_INT >= 29) i.isHardwareAccelerated && !i.isSoftwareOnly
        else !i.name.startsWith("OMX.google.") && !i.name.startsWith("c2.android.")

    /** Best codec for [codec]: hardware first, then by name for determinism. */
    fun best(codec: Codec, encoder: Boolean): Cap? {
        val list = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
            .filter { it.isEncoder == encoder && codec.mime in it.supportedTypes }
            .sortedWith(compareByDescending<MediaCodecInfo> { isHardware(it) }.thenBy { it.name })
        for (info in list) {
            val vc = runCatching { info.getCapabilitiesForType(codec.mime).videoCapabilities }.getOrNull() ?: continue
            val maxFps = runCatching { vc.getSupportedFrameRatesFor(1920, 1080).upper.toInt() }.getOrElse { vc.supportedFrameRates.upper.toInt() }
            return Cap(codec, info, vc.supportedWidths.upper, vc.supportedHeights.upper, maxFps, isHardware(info), vc.widthAlignment, vc.heightAlignment)
        }
        return null
    }

    fun supports(cap: Cap, w: Int, h: Int, fps: Int): Boolean {
        val vc = cap.info.getCapabilitiesForType(cap.codec.mime).videoCapabilities
        return runCatching { vc.areSizeAndRateSupported(w, h, fps.toDouble()) }.getOrDefault(false)
    }

    /** Flat `[codec, maxW, maxH, maxFps, hardware]*` for the core. */
    fun flat(encoder: Boolean): IntArray = Codec.entries.mapNotNull { best(it, encoder) }
        .flatMap { listOf(it.codec.id, it.maxW, it.maxH, it.maxFps, if (it.hardware) 1 else 0) }.toIntArray()

    /** Encoder size alignment to use for all codecs we may pick (max over codecs, at least 2). */
    fun sizeAlign(): Int = Codec.entries.mapNotNull { best(it, true) }.maxOfOrNull { maxOf(it.alignW, it.alignH) }?.coerceAtLeast(2) ?: 2

    /** Audio codecs we can encode/decode: AAC-LC is mandatory on Android, Opus is optional (API 29+). */
    fun audioFlat(): IntArray = intArrayOf(1)
}
