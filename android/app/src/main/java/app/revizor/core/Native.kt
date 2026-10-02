package app.revizor.core

import java.nio.ByteBuffer

/**
 * JNI surface of the Rust core (`librevizor_core.so`). Method names are camelCase without
 * underscores on purpose: the JNI symbol names are derived from them.
 *
 * Contract: every `…Start` returns a handle (0 = failure) that must be passed to the matching `…Stop` once.
 */
object Native {
    interface Callback {
        /** Called on a Rust thread. Do not block. Kinds are documented in [SenderEvent] / [ReceiverEvent]. */
        fun onEvent(kind: Int, nums: LongArray, text: String)
    }

    init {
        System.loadLibrary("revizor_core")
    }

    // identity / trust
    external fun coreOpen(dir: String): Long
    external fun coreFree(core: Long)
    external fun coreDeviceId(core: Long): String
    external fun coreTrusted(core: Long): String
    external fun coreForget(core: Long, id: String): Boolean

    // discovery / pairing
    external fun scan(core: Long, timeoutMs: Int): String
    external fun pairAsSender(core: Long, ip: String, port: Int, pin: String, name: String): Int

    // sender
    external fun senderStart(
        core: Long, cb: Callback, ip: String, port: Int, name: String,
        codecCaps: IntArray, audioCaps: IntArray, maxBitrate: Int, codecOrder: IntArray, wantAudio: Boolean,
        profile: Int, custom: IntArray, srcW: Int, srcH: Int, srcHz: Int, sizeAlign: Int, tcp: Boolean,
    ): Long
    external fun senderStop(h: Long)
    external fun senderSubmitVideo(h: Long, epoch: Int, ptsUs: Long, keyframe: Boolean, buf: ByteBuffer, offset: Int, len: Int): Boolean
    external fun senderSubmitAudio(h: Long, ptsUs: Long, buf: ByteBuffer, offset: Int, len: Int): Boolean
    external fun senderEncodeTime(h: Long, us: Int)
    external fun senderCaptureDrop(h: Long)
    external fun senderThermal(h: Long, status: Int)
    external fun senderBattery(h: Long, pct: Int, charging: Boolean, powerSave: Boolean)
    external fun senderSetSource(h: Long, w: Int, hh: Int, hz: Int)
    external fun senderStats(h: Long): String
    external fun nowUs(): Long

    // casting to TVs that have nothing of Revizor installed (Google Cast / DLNA)
    /** One TV per line, tab-separated: name, ip, model, manufacturer, methods. */
    external fun castScan(timeoutMs: Int): String
    /** Events: 1 state (0 preparing, 1 starting, 2 playing, 3 buffering, 4 reconnecting, 5 stopped, 6 failed + text), 2 trying next method (text), 3 keyframe needed. */
    external fun castStart(cb: Callback, ip: String, name: String, methods: String, hasAudio: Boolean): Long
    external fun castStop(h: Long)
    external fun castSubmitVideo(h: Long, ptsUs: Long, keyframe: Boolean, buf: ByteBuffer, offset: Int, len: Int): Boolean
    external fun castSubmitAudio(h: Long, ptsUs: Long, buf: ByteBuffer, offset: Int, len: Int): Boolean
    external fun castStats(h: Long): String

    // receiver
    external fun receiverStart(core: Long, cb: Callback, port: Int, name: String, codecCaps: IntArray, audioCaps: IntArray, tcp: Boolean): Long
    external fun receiverStop(h: Long)
    external fun receiverOpenPairing(h: Long): String
    external fun receiverClosePairing(h: Long)
    external fun receiverNextFrame(h: Long, timeoutMs: Int, dst: ByteBuffer, meta: LongArray): Int
    external fun receiverNextAudio(h: Long, dst: ByteBuffer, meta: LongArray): Int
    external fun receiverPresented(h: Long, ptsUs: Long, decodeUs: Int)
    external fun receiverDecodeError(h: Long)
    external fun receiverSignals(h: Long, cpuPct: Int, thermalStatus: Int)
    external fun receiverStats(h: Long): String
    /** sender_clock − local_clock (µs); Long.MIN_VALUE until synchronised. */
    external fun receiverClockOffset(h: Long): Long
}
