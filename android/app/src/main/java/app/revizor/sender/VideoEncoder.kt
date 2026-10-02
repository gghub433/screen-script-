package app.revizor.sender

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.view.Surface
import app.revizor.core.Log
import app.revizor.core.Native
import app.revizor.core.StreamParams
import java.nio.ByteBuffer

/**
 * Hardware encoder fed by a Surface: `VirtualDisplay → Surface → MediaCodec` stays on the GPU/codec
 * side, no Bitmap and no CPU copy of pixels exist anywhere in this path.
 */
class VideoEncoder(private val params: StreamParams, private val sink: FrameSink, private val tuning: Tuning = Tuning()) {
    private val tag = "Encoder"
    private val thread = HandlerThread("rvz-enc-out").apply { start() }
    private val handler = Handler(thread.looper)
    private lateinit var codec: MediaCodec
    lateinit var inputSurface: Surface
        private set
    var codecName: String = ""
        private set
    var hardware: Boolean = false
        private set
    @Volatile private var config: ByteArray? = null
    @Volatile private var released = false
    private var scratch: ByteBuffer = ByteBuffer.allocateDirect(512 * 1024)

    fun start() {
        val cap = CodecCaps.best(params.codec, true) ?: error("No ${params.codec} encoder on this device")
        codecName = cap.info.name
        hardware = cap.hardware
        val fmt = MediaFormat.createVideoFormat(params.codec.mime, params.width, params.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, params.videoBps)
            setInteger(MediaFormat.KEY_FRAME_RATE, params.fps)
            // Keyframes are produced on demand (loss, new viewer, config change); this is only the safety net.
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, tuning.iFrameIntervalSec)
            val vc = cap.info.getCapabilitiesForType(params.codec.mime).encoderCapabilities
            setInteger(
                MediaFormat.KEY_BITRATE_MODE,
                if (vc.isBitrateModeSupported(MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)) MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR
                else MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR,
            )
            setInteger(MediaFormat.KEY_PRIORITY, 0) // real-time
            if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_OPERATING_RATE, params.fps)
            if (tuning.preferHighProfile && params.codec == app.revizor.core.Codec.H264 &&
                cap.info.getCapabilitiesForType(params.codec.mime).profileLevels.any { it.profile == MediaCodecInfo.CodecProfileLevel.AVCProfileHigh }
            ) {
                setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileHigh)
            }
            if (Build.VERSION.SDK_INT >= 29) setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
            // A static screen produces no new frames; repeat the last one so keyframe requests and
            // liveness keep working (costs a few hundred bytes per repeat).
            setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 100_000L)
            setInteger("prepend-sps-pps-to-idr-frames", 1)
        }
        codec = MediaCodec.createByCodecName(codecName)
        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(c: MediaCodec, index: Int) {}
            override fun onOutputBufferAvailable(c: MediaCodec, index: Int, info: MediaCodec.BufferInfo) = handleOutput(c, index, info)
            override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
                Log.e(tag, "encoder error ${e.diagnosticInfo}", e)
            }
            override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {}
        }, handler)
        codec.configure(fmt, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        inputSurface = codec.createInputSurface()
        codec.start()
        Log.i(tag, "started $codecName hw=$hardware ${params.width}x${params.height}@${params.fps} ${params.videoBps / 1000} kbit/s epoch=${params.epoch}")
    }

    private fun handleOutput(c: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
        try {
            if (released) return
            val buf = c.getOutputBuffer(index) ?: return
            if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                config = ByteArray(info.size).also { buf.position(info.offset); buf.get(it) }
                return
            }
            if (info.size <= 0) return
            val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
            // Capture→encoder-output time, measured on the same CLOCK_MONOTONIC as the surface timestamps.
            val latencyUs = (Native.nowUs() - info.presentationTimeUs).coerceIn(0, 2_000_000)
            sink.encodeTime(latencyUs.toInt())

            val cfg = config
            val needsCfg = key && cfg != null && !startsWithParameterSets(buf, info)
            if (needsCfg || !buf.isDirect) {
                val total = info.size + (if (needsCfg) cfg!!.size else 0)
                if (scratch.capacity() < total) scratch = ByteBuffer.allocateDirect(total * 2)
                scratch.clear()
                if (needsCfg) scratch.put(cfg!!)
                buf.position(info.offset).limit(info.offset + info.size)
                scratch.put(buf)
                sink.video(params.epoch, info.presentationTimeUs, key, scratch, 0, total)
            } else {
                sink.video(params.epoch, info.presentationTimeUs, key, buf, info.offset, info.size)
            }
        } finally {
            if (!released) runCatching { c.releaseOutputBuffer(index, false) }
        }
    }

    private fun startsWithParameterSets(b: ByteBuffer, info: MediaCodec.BufferInfo): Boolean {
        val p = info.offset
        if (info.size < 5) return false
        // Annex-B start code followed by SPS (H.264 type 7 / HEVC VPS type 32).
        val nal = if (b.get(p + 2).toInt() == 1) b.get(p + 3).toInt() else b.get(p + 4).toInt()
        val h264 = nal and 0x1f
        val hevc = (nal shr 1) and 0x3f
        return h264 == 7 || hevc == 32
    }

    fun setBitrate(bps: Int) {
        if (released) return
        runCatching { codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, bps) }) }
            .onFailure { Log.w(tag, "setBitrate failed: ${it.message}") }
    }

    fun requestKeyframe() {
        if (released) return
        runCatching { codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) }) }
            .onFailure { Log.w(tag, "requestKeyframe failed: ${it.message}") }
    }

    fun release() {
        if (released) return
        released = true
        runCatching { codec.stop() }
        runCatching { codec.release() }
        runCatching { inputSurface.release() }
        thread.quitSafely()
    }
}
