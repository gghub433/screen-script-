package app.revizor.receiver

import android.content.Context
import android.os.Build
import android.os.PowerManager
import android.view.Surface
import app.revizor.core.Codec
import app.revizor.core.Core
import app.revizor.core.Log
import app.revizor.core.Native
import app.revizor.core.ReceiverEvent
import app.revizor.core.StatsView
import app.revizor.core.StreamParams
import app.revizor.sender.CodecCaps
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import java.util.concurrent.ConcurrentHashMap

enum class RecvState { Stopped, Listening, Streaming }

data class ReceiverUi(
    val state: RecvState = RecvState.Stopped,
    val sender: String? = null,
    val pin: String? = null,
    val lastPaired: String? = null,
    val pairingLocked: Boolean = false,
    val params: StreamParams? = null,
    val decoder: String? = null,
    val decoderHardware: Boolean? = null,
    val stats: StatsView? = null,
)

/** Process-wide receiver: owns the Rust receiver session, the decode loop and the audio player. */
object ReceiverController {
    private val tag = "Receiver"
    private val _ui = MutableStateFlow(ReceiverUi())
    val ui: StateFlow<ReceiverUi> = _ui

    private var handle = 0L
    private var decode: DecodeLoop? = null
    private var audio: AudioPlayer? = null
    private val paramsByEpoch = ConcurrentHashMap<Int, StreamParams>()
    @Volatile private var surface: Surface? = null
    @Volatile private var e2eUs = 0L
    @Volatile private var jitterUs = 0L
    @Volatile var lowLatency = false
    private var statsThread: Thread? = null
    val isRunning get() = handle != 0L

    fun setSurface(s: Surface?) {
        surface = s
    }

    fun start(ctx: Context, core: Core, deviceName: String, port: Int = 47721) {
        if (handle != 0L) return
        val cb = object : Native.Callback {
            override fun onEvent(kind: Int, nums: LongArray, text: String) = onCoreEvent(kind, nums, text)
        }
        handle = Native.receiverStart(core.handle, cb, port, deviceName, CodecCaps.flat(false), CodecCaps.audioFlat(), false)
        if (handle == 0L) {
            Log.e(tag, "could not open port $port")
            return
        }
        _ui.value = ReceiverUi(state = RecvState.Listening)
        decode = DecodeLoop(handle, { surface }, { paramsByEpoch[it] }, { playoutUs() }) { name, hw ->
            _ui.update { it.copy(decoder = name, decoderHardware = hw) }
        }.also { it.start() }
        audio = AudioPlayer(handle) { e2eUs }.also { it.start() }
        val pm = ctx.getSystemService(Context.POWER_SERVICE) as PowerManager
        statsThread = Thread({
            while (handle != 0L && !Thread.currentThread().isInterrupted) {
                try {
                    val h = handle
                    if (h == 0L) break
                    val s = StatsView.parse(Native.receiverStats(h))
                    e2eUs = s?.double("e2eUs")?.toLong() ?: 0
                    jitterUs = s?.double("jitterUs")?.toLong() ?: 0
                    val thermal = if (Build.VERSION.SDK_INT >= 29) pm.currentThermalStatus else -1
                    Native.receiverSignals(h, -1, thermal)
                    _ui.update { it.copy(stats = s) }
                    Thread.sleep(1000)
                } catch (_: InterruptedException) { break }
            }
        }, "rvz-recv-stats").also { it.start() }
    }

    /** Bounded smoothing delay: enough for measured jitter, none in low-latency mode. */
    private fun playoutUs(): Long {
        if (lowLatency) return 0
        val frame = 16_667L
        return (jitterUs * 3).coerceIn(frame / 4, frame * 2)
    }

    fun openPairing() {
        if (handle != 0L) Native.receiverOpenPairing(handle)
    }

    fun closePairing() {
        if (handle != 0L) Native.receiverClosePairing(handle)
    }

    fun stop() {
        val h = handle
        handle = 0
        statsThread?.interrupt(); statsThread = null
        decode?.running = false; decode = null
        audio?.shutdown(); audio = null
        if (h != 0L) Native.receiverStop(h)
        paramsByEpoch.clear()
        _ui.value = ReceiverUi()
    }

    private fun onCoreEvent(kind: Int, n: LongArray, text: String) {
        when (kind) {
            ReceiverEvent.STATE -> _ui.update {
                when (n.getOrElse(0) { 0 }.toInt()) {
                    1 -> it.copy(state = RecvState.Streaming, sender = text)
                    2 -> it.copy(state = RecvState.Stopped)
                    else -> it.copy(state = RecvState.Listening, sender = null, params = null)
                }
            }
            ReceiverEvent.PAIRING_OPENED -> _ui.update { it.copy(pin = text, pairingLocked = false) }
            ReceiverEvent.PAIRING_CLOSED -> _ui.update { it.copy(pin = null) }
            ReceiverEvent.PAIRED -> _ui.update { it.copy(lastPaired = text.substringBefore('|'), pin = null) }
            ReceiverEvent.PAIRING_LOCKED -> _ui.update { it.copy(pin = null, pairingLocked = true) }
            ReceiverEvent.PARAMS -> StreamParams.from(n)?.let { p ->
                paramsByEpoch[p.epoch] = p
                if (paramsByEpoch.size > 8) paramsByEpoch.keys.minOrNull()?.let { paramsByEpoch.remove(it) }
                _ui.update { it.copy(params = p) }
            }
            ReceiverEvent.SENDER_DISCONNECTED -> _ui.update { it.copy(sender = null) }
            ReceiverEvent.SENDER_CONNECTED -> Log.i(tag, "sender connected")
        }
    }
}
