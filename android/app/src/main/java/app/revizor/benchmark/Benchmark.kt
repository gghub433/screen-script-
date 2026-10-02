package app.revizor.benchmark

import android.content.Context
import android.graphics.Color
import android.graphics.ImageFormat
import android.graphics.Paint
import android.media.ImageReader
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.BatteryManager
import android.os.Build
import android.os.PowerManager
import android.os.Process
import android.os.SystemClock
import app.revizor.core.Codec
import app.revizor.core.Log
import app.revizor.sender.CodecCaps
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

data class Mode(val label: String, val w: Int, val h: Int, val fps: Int)

data class BenchResult(
    val mode: Mode,
    val supported: Boolean,
    val note: String? = null,
    val avgFps: Double? = null,
    val droppedPct: Double? = null,
    val encodeAvgMs: Double? = null,
    val encodeP95Ms: Double? = null,
    val decodeAvgMs: Double? = null,
    val bitrateMbps: Double? = null,
    val cpuPct: Double? = null,
    val thermalStart: Int? = null,
    val thermalEnd: Int? = null,
    val chargeUsedUah: Long? = null,
)

/**
 * Measures what THIS device's hardware encoder and decoder really do for each mode, using a synthetic
 * animated test pattern drawn into the encoder's input Surface (no CPU pixel path) and decoding the
 * result again. Nothing is estimated or pre-filled; modes the hardware cannot do are reported as unsupported.
 *
 * Network metrics (loss, jitter) cannot be measured on a single device and are only shown in live sessions.
 */
object Benchmark {
    private const val TAG = "Benchmark"
    val modes = listOf(
        Mode("720p30", 1280, 720, 30), Mode("1080p30", 1920, 1080, 30), Mode("1080p60", 1920, 1080, 60),
        Mode("1440p30", 2560, 1440, 30), Mode("1440p60", 2560, 1440, 60),
    )

    suspend fun run(ctx: Context, seconds: Int = 5, onResult: (BenchResult) -> Unit): List<BenchResult> = withContext(Dispatchers.Default) {
        val out = mutableListOf<BenchResult>()
        for (m in modes) {
            val r = runCatching { runMode(ctx, m, seconds) }.getOrElse {
                Log.e(TAG, "${m.label} failed", it)
                BenchResult(m, supported = false, note = "Failed: ${it.message}")
            }
            out += r; onResult(r)
            Thread.sleep(500) // let the codec settle between modes
        }
        out
    }

    private fun runMode(ctx: Context, m: Mode, seconds: Int): BenchResult {
        val enc = CodecCaps.best(Codec.H264, true) ?: return BenchResult(m, false, "No H.264 encoder")
        if (!CodecCaps.supports(enc, m.w, m.h, m.fps)) return BenchResult(m, false, "Not supported by ${if (enc.hardware) "the hardware" else "any"} encoder")
        val dec = CodecCaps.best(Codec.H264, false)
        val pm = ctx.getSystemService(Context.POWER_SERVICE) as PowerManager
        val bm = ctx.getSystemService(Context.BATTERY_SERVICE) as BatteryManager
        val bitrate = (m.w.toLong() * m.h * m.fps / 10).toInt() // ~0.1 bit/pixel
        val fmt = MediaFormat.createVideoFormat("video/avc", m.w, m.h).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, m.fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        val encoder = MediaCodec.createByCodecName(enc.info.name)
        encoder.configure(fmt, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val surface = encoder.createInputSurface()
        encoder.start()

        // decoder into an ImageReader (consumes frames without needing a window)
        var decoder: MediaCodec? = null
        var reader: ImageReader? = null
        val decodeMs = CopyOnWriteArrayList<Double>()
        val queuedAt = java.util.concurrent.ConcurrentHashMap<Long, Long>()
        if (dec != null && CodecCaps.supports(dec, m.w, m.h, m.fps)) {
            reader = ImageReader.newInstance(m.w, m.h, ImageFormat.YUV_420_888, 3).also { r ->
                r.setOnImageAvailableListener({ it.acquireLatestImage()?.close() }, null)
            }
            decoder = MediaCodec.createByCodecName(dec.info.name).also {
                it.configure(MediaFormat.createVideoFormat("video/avc", m.w, m.h), reader.surface, null, 0)
                it.start()
            }
        }

        val encMs = CopyOnWriteArrayList<Double>()
        val bytes = AtomicLong(); val outputs = AtomicInteger(); var submitted = 0
        val info = MediaCodec.BufferInfo()
        val thermal0 = if (Build.VERSION.SDK_INT >= 29) pm.currentThermalStatus else null
        val charge0 = bm.getLongProperty(BatteryManager.BATTERY_PROPERTY_CHARGE_COUNTER).takeIf { it > 0 }
        val cpu0 = Process.getElapsedCpuTime(); val wall0 = SystemClock.elapsedRealtime()

        val paint = Paint().apply { isAntiAlias = false }
        val interval = 1_000_000_000L / m.fps
        val start = System.nanoTime(); var next = start; var frame = 0
        var csd: ByteArray? = null
        fun pumpEncoder() {
            while (true) {
                val i = encoder.dequeueOutputBuffer(info, 0)
                if (i < 0) break
                val buf = encoder.getOutputBuffer(i)!!
                if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                    csd = ByteArray(info.size).also { buf.position(info.offset); buf.get(it) }
                } else if (info.size > 0) {
                    // surface timestamps and System.nanoTime() are both CLOCK_MONOTONIC
                    val nowUs = System.nanoTime() / 1000
                    encMs += ((nowUs - info.presentationTimeUs).coerceAtLeast(0)) / 1000.0
                    bytes.addAndGet(info.size.toLong()); outputs.incrementAndGet()
                    decoder?.let { d ->
                        val di = d.dequeueInputBuffer(0)
                        if (di >= 0) {
                            val ib = d.getInputBuffer(di)!!; ib.clear()
                            val c = csd
                            val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
                            if (key && c != null) ib.put(c)
                            buf.position(info.offset).limit(info.offset + info.size); ib.put(buf)
                            val len = ib.position()
                            queuedAt[info.presentationTimeUs] = System.nanoTime()
                            d.queueInputBuffer(di, 0, len, info.presentationTimeUs, if (key) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0)
                        }
                    }
                }
                encoder.releaseOutputBuffer(i, false)
            }
            decoder?.let { d ->
                val di = MediaCodec.BufferInfo()
                while (true) {
                    val o = d.dequeueOutputBuffer(di, 0); if (o < 0) break
                    queuedAt.remove(di.presentationTimeUs)?.let { decodeMs += (System.nanoTime() - it) / 1e6 }
                    d.releaseOutputBuffer(o, true)
                }
            }
        }

        while (System.nanoTime() - start < seconds * 1_000_000_000L) {
            val c = surface.lockHardwareCanvas()
            try {
                c.drawColor(Color.rgb((frame * 3) % 255, 40, 90))
                for (k in 0 until 40) { // moving blocks + thin lines: screen-like mix of flat areas and edges
                    paint.color = Color.rgb((k * 53 + frame * 7) % 255, (k * 29) % 255, (k * 97 + frame) % 255)
                    val x = ((k * 211 + frame * (3 + k % 5)) % m.w).toFloat(); val y = ((k * 131 + frame * (2 + k % 3)) % m.h).toFloat()
                    c.drawRect(x, y, x + m.w / 12f, y + m.h / 14f, paint)
                    c.drawLine(0f, y, m.w.toFloat(), y + 3, paint)
                }
            } finally { surface.unlockCanvasAndPost(c) }
            submitted++; frame++
            pumpEncoder()
            next += interval
            val sleep = (next - System.nanoTime()) / 1_000_000
            if (sleep > 0) Thread.sleep(sleep) else if (sleep < -100) next = System.nanoTime()
        }
        val drainUntil = System.nanoTime() + 500_000_000L
        while (System.nanoTime() < drainUntil) { pumpEncoder(); Thread.sleep(5) }

        val wall = SystemClock.elapsedRealtime() - wall0
        val cpu = Process.getElapsedCpuTime() - cpu0
        val cores = Runtime.getRuntime().availableProcessors()
        val thermal1 = if (Build.VERSION.SDK_INT >= 29) pm.currentThermalStatus else null
        val charge1 = bm.getLongProperty(BatteryManager.BATTERY_PROPERTY_CHARGE_COUNTER).takeIf { it > 0 }

        runCatching { encoder.stop(); encoder.release() }; runCatching { surface.release() }
        runCatching { decoder?.stop(); decoder?.release() }; runCatching { reader?.close() }

        val sorted = encMs.sorted()
        val note = buildString {
            if (!enc.hardware) append("software encoder; ")
            if (decoder == null) append("decode not measured; ")
        }.ifEmpty { null }
        return BenchResult(
            mode = m, supported = true, note = note,
            avgFps = outputs.get() * 1000.0 / (seconds * 1000.0),
            droppedPct = if (submitted > 0) 100.0 * (submitted - outputs.get()).coerceAtLeast(0) / submitted else null,
            encodeAvgMs = encMs.takeIf { it.isNotEmpty() }?.average(),
            encodeP95Ms = sorted.getOrNull((sorted.size * 0.95).toInt().coerceAtMost(sorted.size - 1)),
            decodeAvgMs = decodeMs.takeIf { it.isNotEmpty() }?.average(),
            bitrateMbps = bytes.get() * 8.0 / seconds / 1e6,
            cpuPct = if (wall > 0) 100.0 * cpu / wall / cores else null,
            thermalStart = thermal0, thermalEnd = thermal1,
            chargeUsedUah = if (charge0 != null && charge1 != null) charge0 - charge1 else null,
        )
    }
}
