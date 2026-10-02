package app.revizor.sender

import app.revizor.core.QualityReason
import app.revizor.core.StatsView
import app.revizor.core.StreamParams
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update

enum class SendState { Idle, Connecting, Streaming, Reconnecting, Failed }

/** Revizor protocol (app on the other device) or a standard TV (Google Cast / DLNA, nothing installed there). */
enum class SendMode { Revizor, Tv }

data class SenderUi(
    val state: SendState = SendState.Idle,
    val error: String? = null,
    val peer: String? = null,
    val params: StreamParams? = null,
    /** Why quality is currently below the best tier (null = not limited). */
    val limitedBy: QualityReason? = null,
    val hardware: Boolean? = null,
    val encoderName: String? = null,
    val stats: StatsView? = null,
    val mode: SendMode = SendMode.Revizor,
    /** TV mode: how we reach the TV ("Google Cast" / "DLNA"). */
    val method: String? = null,
    /** TV mode: finer state text ("Waiting for the TV…", "Buffering…") and one-off notices. */
    val tvStatus: String? = null,
    val tvStats: StatsView? = null,
)

/** Process-wide sender state, written by [CaptureService] and observed by the UI. */
object SenderController {
    private val _ui = MutableStateFlow(SenderUi())
    val ui: StateFlow<SenderUi> = _ui

    fun update(f: (SenderUi) -> SenderUi) = _ui.update(f)
    fun reset() {
        _ui.value = SenderUi()
    }
}
