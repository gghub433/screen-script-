package app.revizor.receiver

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.view.Surface
import app.revizor.core.Log
import app.revizor.core.Native
import app.revizor.core.StreamParams
import app.revizor.sender.CodecCaps
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap

/**
 * Pulls reassembled frames from the core and feeds a hardware MediaCodec that renders straight to the
 * SurfaceView's Surface (`decoder → Surface → display`, no CPU bitmap anywhere).
 */
class DecodeLoop(
    private val rx: Long,
    private val surface: () -> Surface?,
    private val params: (Int) -> StreamParams?,
    private val playoutUs: () -> Long,
    private val onStats: (name: String, hardware: Boolean) -> Unit,
) : Thread("rvz-decode") {
    private val tag = "Decoder"
    @Volatile var running = true
    private var codec: MediaCodec? = null
    private var curEpoch = -1
    private var curSurface: Surface? = null
    private val queuedAt = ConcurrentHashMap<Long, Long>() // pts → System.nanoTime() when queued
    private val renderThread = HandlerThread("rvz-render-cb").apply { start() }
    private val buf: ByteBuffer = ByteBuffer.allocateDirect(8 * 1024 * 1024)
    private val meta = LongArray(5)
    private var anchorNs = 0L
    private var anchorPts = 0L

    override fun run() {
        priority = MAX_PRIORITY
        while (running) {
            buf.clear()
            val n = Native.receiverNextFrame(rx, 20, buf, meta)
            drain()
            if (n <= 0) continue
            val pts = meta[0]
            val key = meta[1] == 1L
            val epoch = meta[3].toInt()
            val s = surface() ?: continue
            val p = params(epoch) ?: continue
            if (codec == null || epoch != curEpoch || s !== curSurface) {
                if (!key) { Native.receiverDecodeError(rx); continue } // can only (re)start on a keyframe
                if (!restart(p, s)) continue
                curEpoch = epoch; curSurface = s
            }
            feed(n, pts, key)
        }
        release()
        renderThread.quitSafely()
    }

    private fun restart(p: StreamParams, s: Surface): Boolean {
        release()
        return try {
            val cap = CodecCaps.best(p.codec, false) ?: error("No ${p.codec} decoder on this device")
            val f = MediaFormat.createVideoFormat(p.codec.mime, p.width, p.height).apply {
                setInteger(MediaFormat.KEY_PRIORITY, 0)
                setInteger(MediaFormat.KEY_OPERATING_RATE, p.fps)
                if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
                // Qualcomm's vendor low-latency switch; ignored by other decoders.
                setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
            }
            val c = MediaCodec.createByCodecName(cap.info.name)
            c.configure(f, s, null, 0)
            c.setOnFrameRenderedListener({ _, pts, _ ->
                val q = queuedAt.remove(pts)
                val decUs = if (q != null) ((System.nanoTime() - q) / 1000).toInt() else 0
                Native.receiverPresented(rx, pts, decUs.coerceAtLeast(0))
            }, Handler(renderThread.looper))
            c.start()
            codec = c
            anchorNs = 0
            onStats(cap.info.name, cap.hardware)
            Log.i(tag, "started ${cap.info.name} hw=${cap.hardware} ${p.width}x${p.height} epoch=${p.epoch}")
            true
        } catch (t: Throwable) {
            Log.e(tag, "decoder start failed", t)
            codec = null
            Native.receiverDecodeError(rx)
            false
        }
    }

    private fun feed(n: Int, pts: Long, key: Boolean) {
        val c = codec ?: return
        try {
            val idx = c.dequeueInputBuffer(8_000)
            if (idx < 0) { // decoder is behind: drop and resync instead of letting latency grow
                Native.receiverDecodeError(rx); return
            }
            val ib = c.getInputBuffer(idx)!!
            ib.clear()
            buf.position(0).limit(n)
            ib.put(buf)
            queuedAt[pts] = System.nanoTime()
            if (queuedAt.size > 64) queuedAt.keys.minOrNull()?.let { queuedAt.remove(it) }
            c.queueInputBuffer(idx, 0, n, pts, if (key) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0)
        } catch (e: MediaCodec.CodecException) {
            Log.e(tag, "decode error ${e.diagnosticInfo}", e)
            release(); Native.receiverDecodeError(rx)
        } catch (e: IllegalStateException) {
            release(); Native.receiverDecodeError(rx)
        }
    }

    private fun drain() {
        val c = codec ?: return
        val info = MediaCodec.BufferInfo()
        try {
            while (true) {
                val i = c.dequeueOutputBuffer(info, 0)
                if (i < 0) break
                val now = System.nanoTime()
                c.releaseOutputBuffer(i, renderTime(info.presentationTimeUs, now))
            }
        } catch (e: IllegalStateException) {
            release()
        }
    }

    /** Smooths network jitter with a bounded playout delay; with 0 delay frames are shown as soon as decoded. */
    private fun renderTime(ptsUs: Long, nowNs: Long): Long {
        val delayNs = playoutUs() * 1000
        if (delayNs <= 0) return nowNs
        if (anchorNs == 0L) { anchorNs = nowNs; anchorPts = ptsUs }
        val target = anchorNs + (ptsUs - anchorPts) * 1000 + delayNs
        if (target < nowNs || target > nowNs + 100_000_000L) { // late, or clocks jumped: re-anchor
            anchorNs = nowNs; anchorPts = ptsUs
            return nowNs
        }
        return target
    }

    private fun release() {
        codec?.let { runCatching { it.stop() }; runCatching { it.release() } }
        codec = null; curEpoch = -1; curSurface = null
        queuedAt.clear()
    }
}
