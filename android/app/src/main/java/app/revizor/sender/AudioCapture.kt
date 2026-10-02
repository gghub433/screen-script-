package app.revizor.sender

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.MediaRecorder
import android.media.projection.MediaProjection
import app.revizor.core.Log
import app.revizor.core.Native
import java.nio.ByteBuffer

enum class AudioMode { None, System, Mic, Both }

/**
 * System audio (AudioPlaybackCapture) and/or microphone → AAC-LC (MediaCodec) → ADTS frames → core.
 * Frames carry capture timestamps on the session clock so the receiver can align them with video.
 */
class AudioCapture(private val mode: AudioMode, private val projection: MediaProjection?, private val sink: FrameSink) {
    private val tag = "Audio"
    private val rate = 48_000
    private var system: AudioRecord? = null
    private var mic: AudioRecord? = null
    private var codec: MediaCodec? = null
    @Volatile private var running = false
    private var thread: Thread? = null

    @SuppressLint("MissingPermission") // RECORD_AUDIO is checked before the service starts audio
    fun start() {
        val fmt = AudioFormat.Builder().setEncoding(AudioFormat.ENCODING_PCM_16BIT).setSampleRate(rate).setChannelMask(AudioFormat.CHANNEL_IN_STEREO).build()
        val bufSize = AudioRecord.getMinBufferSize(rate, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT) * 2
        if ((mode == AudioMode.System || mode == AudioMode.Both) && projection != null) {
            val cfg = AudioPlaybackCaptureConfiguration.Builder(projection)
                .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
                .addMatchingUsage(AudioAttributes.USAGE_GAME)
                .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
                .build()
            system = AudioRecord.Builder().setAudioFormat(fmt).setBufferSizeInBytes(bufSize).setAudioPlaybackCaptureConfig(cfg).build()
        }
        if (mode == AudioMode.Mic || mode == AudioMode.Both) {
            mic = AudioRecord.Builder().setAudioSource(MediaRecorder.AudioSource.MIC).setAudioFormat(fmt).setBufferSizeInBytes(bufSize).build()
        }
        val f = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_AAC, rate, 2).apply {
            setInteger(MediaFormat.KEY_AAC_PROFILE, MediaCodecInfo.CodecProfileLevel.AACObjectLC)
            setInteger(MediaFormat.KEY_BIT_RATE, 128_000)
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 16 * 1024)
        }
        codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_AAC).also {
            it.configure(f, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            it.start()
        }
        system?.startRecording()
        mic?.startRecording()
        running = true
        thread = Thread({ loop() }, "rvz-audio").also { it.priority = Thread.MAX_PRIORITY; it.start() }
        Log.i(tag, "audio capture started mode=$mode")
    }

    private fun loop() {
        val c = codec ?: return
        val frameBytes = 1024 * 2 * 2 // one AAC frame of stereo 16-bit
        val a = ByteArray(frameBytes)
        val b = ByteArray(frameBytes)
        val info = MediaCodec.BufferInfo()
        val out = ByteBuffer.allocateDirect(4096)
        while (running) {
            val n1 = system?.let { readFully(it, a) } ?: 0
            val n2 = mic?.let { readFully(it, b) } ?: 0
            if (!running) break
            val ptsUs = Native.nowUs() - 1024L * 1_000_000 / rate
            val pcm = when {
                system != null && mic != null -> mix(a, n1, b, n2)
                system != null -> a.copyOf(frameBytes).also { if (n1 < frameBytes) java.util.Arrays.fill(it, n1, frameBytes, 0) }
                else -> b.copyOf(frameBytes).also { if (n2 < frameBytes) java.util.Arrays.fill(it, n2, frameBytes, 0) }
            }
            val idx = c.dequeueInputBuffer(20_000)
            if (idx >= 0) {
                val ib = c.getInputBuffer(idx)!!
                ib.clear(); ib.put(pcm)
                c.queueInputBuffer(idx, 0, pcm.size, ptsUs, 0)
            }
            while (true) {
                val oi = c.dequeueOutputBuffer(info, 0)
                if (oi < 0) break
                val ob = c.getOutputBuffer(oi)!!
                if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0 && info.size > 0) {
                    out.clear()
                    out.put(adts(info.size))
                    ob.position(info.offset).limit(info.offset + info.size)
                    out.put(ob)
                    sink.audio(info.presentationTimeUs, out, 0, info.size + 7)
                }
                c.releaseOutputBuffer(oi, false)
            }
        }
    }

    private fun readFully(r: AudioRecord, buf: ByteArray): Int {
        var off = 0
        while (running && off < buf.size) {
            val n = r.read(buf, off, buf.size - off)
            if (n <= 0) break
            off += n
        }
        return off
    }

    private fun mix(a: ByteArray, na: Int, b: ByteArray, nb: Int): ByteArray {
        val out = ByteArray(a.size)
        val bb = ByteBuffer.wrap(out).order(java.nio.ByteOrder.LITTLE_ENDIAN)
        val ba = ByteBuffer.wrap(a).order(java.nio.ByteOrder.LITTLE_ENDIAN)
        val bm = ByteBuffer.wrap(b).order(java.nio.ByteOrder.LITTLE_ENDIAN)
        for (i in 0 until a.size / 2) {
            val x = if (i * 2 < na) ba.getShort(i * 2).toInt() else 0
            val y = if (i * 2 < nb) bm.getShort(i * 2).toInt() else 0
            bb.putShort(i * 2, (x + y).coerceIn(-32768, 32767).toShort())
        }
        return out
    }

    /** 7-byte ADTS header: AAC-LC, 48 kHz (index 3), stereo. */
    private fun adts(payload: Int): ByteArray {
        val len = payload + 7
        return byteArrayOf(
            0xFF.toByte(), 0xF1.toByte(),
            (((2 - 1) shl 6) or (3 shl 2) or (2 shr 2)).toByte(),
            (((2 and 3) shl 6) or (len shr 11)).toByte(),
            ((len shr 3) and 0xFF).toByte(),
            (((len and 7) shl 5) or 0x1F).toByte(),
            0xFC.toByte(),
        )
    }

    fun stop() {
        running = false
        runCatching { thread?.join(500) }
        runCatching { system?.stop(); system?.release() }
        runCatching { mic?.stop(); mic?.release() }
        runCatching { codec?.stop(); codec?.release() }
    }
}
