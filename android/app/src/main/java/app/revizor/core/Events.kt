package app.revizor.core

/** Reasons the adaptive engine reports, in the order of `reason_idx` in the Rust bridge. */
enum class QualityReason {
    Network, EncoderOverload, DecoderOverload, Thermal, Battery, Recovery, Profile;

    /** Short, user-facing explanation of why quality was reduced; null when it is not a reduction. */
    fun userText(): String? = when (this) {
        Network -> "network conditions"
        EncoderOverload -> "this device being busy"
        DecoderOverload -> "the receiver being busy"
        Thermal -> "temperature protection"
        Battery -> "battery saving"
        Recovery, Profile -> null
    }

    companion object {
        fun from(i: Long): QualityReason? = entries.getOrNull(i.toInt())
    }
}

enum class Codec(val id: Int, val mime: String) {
    H264(1, "video/avc"), H265(2, "video/hevc"), AV1(3, "video/av01");

    companion object {
        fun from(id: Int): Codec? = entries.firstOrNull { it.id == id }
    }
}

/** Stream parameters, decoded from the event number arrays. */
data class StreamParams(
    val epoch: Int, val codec: Codec, val width: Int, val height: Int, val fps: Int, val videoBps: Int,
    val audioCodec: Int, val audioRate: Int, val audioChannels: Int, val audioBps: Int,
) {
    companion object {
        fun from(n: LongArray): StreamParams? {
            if (n.size < 10) return null
            return StreamParams(
                n[0].toInt(), Codec.from(n[1].toInt()) ?: return null, n[2].toInt(), n[3].toInt(), n[4].toInt(), n[5].toInt(),
                n[6].toInt(), n[7].toInt(), n[8].toInt(), n[9].toInt(),
            )
        }
    }
}

/** Sender event kinds (see `revizor-jni/src/events.rs`). */
object SenderEvent {
    const val STATE = 1          // nums[0]: 0 connecting, 1 streaming, 2 reconnecting(nums[1]=attempt), 3 stopped, 4 failed (text = message)
    const val PEER = 2           // text = "name|deviceId", nums[0] = hardware codecs on both ends
    const val RECONFIGURE = 3    // StreamParams numbers + [reason(0 initial,1 source,2+ adaptive), limitedBy(-1 none)]
    const val SET_BITRATE = 4    // nums[0] = video bps
    const val KEYFRAME = 5       // nums[0] = reason
}

/** Receiver event kinds. */
object ReceiverEvent {
    const val STATE = 1          // nums[0]: 0 listening, 1 streaming (text = sender), 2 stopped
    const val PAIRING_OPENED = 2 // text = pin
    const val PAIRING_CLOSED = 3
    const val PAIRED = 4         // text = "name|deviceId"
    const val PAIRING_LOCKED = 5
    const val SENDER_CONNECTED = 6 // text = "name|deviceId"
    const val PARAMS = 7         // StreamParams numbers
    const val SENDER_DISCONNECTED = 8
}
