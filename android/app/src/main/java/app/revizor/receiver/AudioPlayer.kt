package app.revizor.receiver

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import app.revizor.core.Log
import app.revizor.core.Native
import java.nio.ByteBuffer
import java.util.ArrayDeque

/**
 * ADTS-AAC → MediaCodec → AudioTrack (low-latency mode). Playback is scheduled against the sender's
 * capture timestamps (via the synchronised clock) so sound lines up with the video, whose
 * capture→screen latency is passed in by the caller.
 */
class AudioPlayer(private val rx: Long, private val videoLatencyUs: () -> Long) : Thread("rvz-audio-play") {
    private val tag = "AudioPlayer"
    @Volatile var running = true
    private var codec: MediaCodec? = null
    private var track: AudioTrack? = null
    private val buf = ByteBuffer.allocateDirect(8192)
    private val meta = LongArray(5)
    private class Chunk(val ptsUs: Long, val pcm: ByteArray)
    private val pending = ArrayDeque<Chunk>()

    override fun run() {
        try {
            val c = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_AAC)
            c.configure(MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_AAC, 48_000, 2).apply { setInteger(MediaFormat.KEY_IS_ADTS, 1) }, null, null, 0)
            c.start(); codec = c
            val minBuf = AudioTrack.getMinBufferSize(48_000, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT)
            track = AudioTrack.Builder()
                .setAudioAttributes(AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA).setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build())
                .setAudioFormat(AudioFormat.Builder().setEncoding(AudioFormat.ENCODING_PCM_16BIT).setSampleRate(48_000).setChannelMask(AudioFormat.CHANNEL_OUT_STEREO).build())
                .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                .setBufferSizeInBytes(minBuf * 2)
                .build().also { it.play() }
        } catch (t: Throwable) {
            Log.e(tag, "audio output unavailable", t)
            return
        }
        val info = MediaCodec.BufferInfo()
        val c = codec!!
        val t = track!!
        while (running) {
            buf.clear()
            val n = Native.receiverNextAudio(rx, buf, meta)
            if (n > 0) {
                val i = c.dequeueInputBuffer(2_000)
                if (i >= 0) {
                    val ib = c.getInputBuffer(i)!!
                    ib.clear(); buf.position(0).limit(n); ib.put(buf)
                    c.queueInputBuffer(i, 0, n, meta[0], 0)
                }
            }
            while (true) {
                val o = c.dequeueOutputBuffer(info, 0)
                if (o < 0) break
                val ob = c.getOutputBuffer(o)!!
                val pcm = ByteArray(info.size)
                ob.position(info.offset); ob.get(pcm, 0, info.size)
                c.releaseOutputBuffer(o, false)
                pending.addLast(Chunk(info.presentationTimeUs, pcm))
            }
            play(t)
            if (n <= 0) sleep(2)
        }
        runCatching { t.stop(); t.release() }
        runCatching { c.stop(); c.release() }
    }

    private fun play(t: AudioTrack) {
        val offset = Native.receiverClockOffset(rx)
        while (pending.isNotEmpty()) {
            val ch = pending.peekFirst()!!
            if (offset != Long.MIN_VALUE) {
                // time on the sender's clock now, and when this chunk should reach the speaker
                val senderNow = Native.nowUs() + offset
                val due = ch.ptsUs + videoLatencyUs().coerceAtLeast(30_000) - 20_000 // ~20 ms AudioTrack pipeline
                if (due > senderNow + 5_000) break
                if (senderNow - due > 150_000) { pending.pollFirst(); continue } // hopelessly late
            }
            pending.pollFirst()
            t.write(ch.pcm, 0, ch.pcm.size)
        }
    }

    fun shutdown() {
        running = false
    }
}
