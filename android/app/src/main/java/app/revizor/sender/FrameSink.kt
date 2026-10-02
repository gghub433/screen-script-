package app.revizor.sender

import app.revizor.core.Native
import java.nio.ByteBuffer

/** Where encoded frames go: the Revizor protocol engine, or the standard-TV (Cast / DLNA) streamer. */
interface FrameSink {
    fun video(epoch: Int, ptsUs: Long, key: Boolean, buf: ByteBuffer, off: Int, len: Int)
    fun audio(ptsUs: Long, buf: ByteBuffer, off: Int, len: Int)
    /** Measured capture→encoder-output time of one frame (µs); only the Revizor engine uses it. */
    fun encodeTime(us: Int)
}

class RevizorSink(private val h: Long) : FrameSink {
    override fun video(epoch: Int, ptsUs: Long, key: Boolean, buf: ByteBuffer, off: Int, len: Int) {
        Native.senderSubmitVideo(h, epoch, ptsUs, key, buf, off, len)
    }
    override fun audio(ptsUs: Long, buf: ByteBuffer, off: Int, len: Int) {
        Native.senderSubmitAudio(h, ptsUs, buf, off, len)
    }
    override fun encodeTime(us: Int) = Native.senderEncodeTime(h, us)
}

class TvSink(private val h: Long) : FrameSink {
    override fun video(epoch: Int, ptsUs: Long, key: Boolean, buf: ByteBuffer, off: Int, len: Int) {
        Native.castSubmitVideo(h, ptsUs, key, buf, off, len)
    }
    override fun audio(ptsUs: Long, buf: ByteBuffer, off: Int, len: Int) {
        Native.castSubmitAudio(h, ptsUs, buf, off, len)
    }
    override fun encodeTime(us: Int) {}
}

/** Encoder tuning that differs between the two modes. */
data class Tuning(
    /** Safety-net keyframe interval; TV mode needs ~1 s because HLS segments can only start at keyframes. */
    val iFrameIntervalSec: Int = 10,
    /** Ask for the High profile (better compression of sharp screen content) when the encoder offers it. Chromecast and most TVs decode High up to level 4.1 (1080p30); a TV that does not would show up as "refused the stream". */
    val preferHighProfile: Boolean = false,
)

/** Same maths as the Rust core's `fit_short_side`: keep the aspect ratio, never upscale, align to the encoder. */
fun fitShortSide(srcW: Int, srcH: Int, short: Int, align: Int): Pair<Int, Int> {
    val s = minOf(srcW, srcH).coerceAtLeast(1)
    val t = minOf(short, s)
    val a = align.coerceAtLeast(1)
    fun al(v: Long) = (((v / a) * a).coerceAtLeast(a.toLong())).toInt()
    return al(srcW.toLong() * t / s) to al(srcH.toLong() * t / s)
}
