package app.revizor.sender

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.util.DisplayMetrics
import android.view.Display
import app.revizor.R
import app.revizor.core.Codec
import app.revizor.core.Log
import app.revizor.core.Native
import app.revizor.core.QualityReason
import app.revizor.core.SenderEvent
import app.revizor.core.StatsView
import app.revizor.core.StreamParams
import app.revizor.RevizorApp

/**
 * Foreground service owning the whole sender pipeline:
 * MediaProjection → VirtualDisplay → (Surface) → MediaCodec → Rust core (packetize, encrypt, send).
 */
class CaptureService : Service() {
    companion object {
        const val ACTION_START = "app.revizor.START"
        const val ACTION_STOP = "app.revizor.STOP"
        const val EXTRA_RESULT_CODE = "resultCode"
        const val EXTRA_RESULT_DATA = "resultData"
        const val EXTRA_IP = "ip"
        const val EXTRA_PORT = "port"
        const val EXTRA_NAME = "name"
        const val EXTRA_MY_NAME = "myName"
        const val EXTRA_PROFILE = "profile"
        const val EXTRA_AUDIO = "audio"
        const val EXTRA_HEVC = "hevc"
        private const val CHANNEL = "capture"
        private const val NOTIF_ID = 17

        fun stop(ctx: Context) {
            ctx.startService(Intent(ctx, CaptureService::class.java).setAction(ACTION_STOP))
        }
    }

    private val tag = "CaptureService"
    private lateinit var worker: HandlerThread
    private lateinit var handler: Handler
    private var projection: MediaProjection? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var encoder: VideoEncoder? = null
    private var audio: AudioCapture? = null
    private var monitor: DeviceMonitor? = null
    private var sender = 0L
    private var dpi = 320
    private var lastSource = Triple(0, 0, 0)
    private var displayListener: DisplayManager.DisplayListener? = null
    private var running = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        worker = HandlerThread("rvz-capture-ctl").also { it.start() }
        handler = Handler(worker.looper)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> handler.post { teardown(); stopSelf() }
            ACTION_START -> {
                val mic = AudioMode.entries.getOrNull(intent.getIntExtra(EXTRA_AUDIO, 0)).let { it == AudioMode.Mic || it == AudioMode.Both }
                goForeground(intent.getStringExtra(EXTRA_NAME) ?: "receiver", mic)
                handler.post { begin(intent) }
            }
        }
        return START_NOT_STICKY
    }

    private fun goForeground(target: String, mic: Boolean) {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, getString(R.string.capture_channel), NotificationManager.IMPORTANCE_LOW))
        val stop = PendingIntent.getService(this, 1, Intent(this, CaptureService::class.java).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE)
        val open = PendingIntent.getActivity(this, 2, packageManager.getLaunchIntentForPackage(packageName), PendingIntent.FLAG_IMMUTABLE)
        val n = Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.ic_menu_share)
            .setContentTitle("Sharing your screen")
            .setContentText("to $target")
            .setContentIntent(open)
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
            .setOngoing(true)
            .build()
        var type = ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
        if (mic) type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
        startForeground(NOTIF_ID, n, type)
    }

    // ───────────────────────────── lifecycle ─────────────────────────────

    private fun begin(i: Intent) {
        if (running) return
        running = true
        try {
            val core = RevizorApp.core
            val data = i.getParcelableExtra<Intent>(EXTRA_RESULT_DATA) ?: error("missing projection grant")
            val mpm = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            val proj = mpm.getMediaProjection(i.getIntExtra(EXTRA_RESULT_CODE, 0), data)
            // Android 14 requires a callback before the first virtual display is created.
            proj.registerCallback(object : MediaProjection.Callback() {
                override fun onStop() {
                    Log.i(tag, "projection stopped by the system or the user")
                    handler.post { teardown(); stopSelf() }
                }
            }, handler)
            projection = proj

            val (w, h, hz) = displayInfo()
            lastSource = Triple(w, h, hz)
            val audioMode = AudioMode.entries.getOrElse(i.getIntExtra(EXTRA_AUDIO, 0)) { AudioMode.None }
            val codecOrder = if (i.getBooleanExtra(EXTRA_HEVC, false)) intArrayOf(Codec.H265.id, Codec.H264.id) else intArrayOf(Codec.H264.id)

            SenderController.update { it.copy(state = SendState.Connecting, error = null) }
            sender = Native.senderStart(
                core.handle, callback, i.getStringExtra(EXTRA_IP)!!, i.getIntExtra(EXTRA_PORT, 47721), i.getStringExtra(EXTRA_MY_NAME) ?: "Android",
                CodecCaps.flat(true), CodecCaps.audioFlat(), 100_000_000, codecOrder, audioMode != AudioMode.None,
                i.getIntExtra(EXTRA_PROFILE, 1), intArrayOf(0, 0, 0), w, h, hz, CodecCaps.sizeAlign(), false,
            )
            check(sender != 0L) { "could not start the streaming engine" }

            monitor = DeviceMonitor(this, sender).also { it.start() }
            if (audioMode != AudioMode.None) {
                audio = AudioCapture(audioMode, proj, sender).also { runCatching { it.start() }.onFailure { e -> Log.e(tag, "audio failed", e); audio = null } }
            }
            watchDisplay()
            scheduleStats()
        } catch (t: Throwable) {
            Log.e(tag, "start failed", t)
            SenderController.update { it.copy(state = SendState.Failed, error = t.message ?: "Could not start sharing") }
            teardown(keepError = true)
            stopSelf()
        }
    }

    private fun displayInfo(): Triple<Int, Int, Int> {
        val dm = getSystemService(DisplayManager::class.java)
        val d = dm.getDisplay(Display.DEFAULT_DISPLAY)
        val m = DisplayMetrics()
        @Suppress("DEPRECATION") d.getRealMetrics(m)
        dpi = m.densityDpi
        return Triple(m.widthPixels, m.heightPixels, d.refreshRate.toInt().coerceAtLeast(30))
    }

    private fun watchDisplay() {
        val dm = getSystemService(DisplayManager::class.java)
        val l = object : DisplayManager.DisplayListener {
            override fun onDisplayAdded(id: Int) {}
            override fun onDisplayRemoved(id: Int) {}
            override fun onDisplayChanged(id: Int) {
                if (id != Display.DEFAULT_DISPLAY || sender == 0L) return
                val now = displayInfo()
                if (now != lastSource) {
                    lastSource = now
                    Log.i(tag, "display changed to ${now.first}x${now.second}@${now.third}")
                    Native.senderSetSource(sender, now.first, now.second, now.third)
                }
            }
        }
        dm.registerDisplayListener(l, handler)
        displayListener = l
    }

    private fun scheduleStats() {
        handler.postDelayed({
            if (!running || sender == 0L) return@postDelayed
            SenderController.update { it.copy(stats = StatsView.parse(Native.senderStats(sender))) }
            scheduleStats()
        }, 1000)
    }

    private fun teardown(keepError: Boolean = false) {
        if (!running && sender == 0L && projection == null) return
        running = false
        displayListener?.let { getSystemService(DisplayManager::class.java).unregisterDisplayListener(it) }
        displayListener = null
        monitor?.stop(); monitor = null
        audio?.stop(); audio = null
        runCatching { virtualDisplay?.release() }; virtualDisplay = null
        encoder?.release(); encoder = null
        runCatching { projection?.stop() }; projection = null
        if (sender != 0L) {
            Native.senderStop(sender)
            sender = 0
        }
        if (!keepError) SenderController.reset()
        stopForeground(STOP_FOREGROUND_REMOVE)
    }

    override fun onDestroy() {
        handler.post { teardown() }
        worker.quitSafely()
        super.onDestroy()
    }

    // ───────────────────────────── events from the Rust core ─────────────────────────────

    private val callback = object : Native.Callback {
        override fun onEvent(kind: Int, nums: LongArray, text: String) {
            handler.post { onCoreEvent(kind, nums, text) }
        }
    }

    private fun onCoreEvent(kind: Int, n: LongArray, text: String) {
        when (kind) {
            SenderEvent.STATE -> {
                when (n.getOrElse(0) { 0 }.toInt()) {
                    0 -> SenderController.update { it.copy(state = SendState.Connecting) }
                    1 -> SenderController.update { it.copy(state = SendState.Streaming, error = null) }
                    2 -> SenderController.update { it.copy(state = SendState.Reconnecting) }
                    3 -> { /* stopped by us */ }
                    4 -> {
                        SenderController.update { it.copy(state = SendState.Failed, error = text) }
                        teardown(keepError = true); stopSelf()
                    }
                }
            }
            SenderEvent.PEER -> SenderController.update { it.copy(peer = text.substringBefore('|'), hardware = n.getOrNull(0) == 1L) }
            SenderEvent.RECONFIGURE -> {
                val p = StreamParams.from(n) ?: return
                val limited = n.getOrNull(11)?.takeIf { it >= 0 }?.let { QualityReason.from(it) }
                applyConfig(p)
                SenderController.update { it.copy(params = p, limitedBy = limited) }
            }
            SenderEvent.SET_BITRATE -> encoder?.setBitrate(n[0].toInt())
            SenderEvent.KEYFRAME -> encoder?.requestKeyframe()
        }
    }

    /** New epoch: build a fresh encoder at the new size, retarget the virtual display, drop the old encoder. */
    private fun applyConfig(p: StreamParams) {
        val proj = projection ?: return
        try {
            val old = encoder
            val enc = VideoEncoder(p, sender).also { it.start() }
            val vd = virtualDisplay
            if (vd == null) {
                virtualDisplay = proj.createVirtualDisplay(
                    "Revizor", p.width, p.height, dpi, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, enc.inputSurface, null, handler,
                )
            } else {
                vd.resize(p.width, p.height, dpi)
                vd.surface = enc.inputSurface
            }
            encoder = enc
            old?.release()
            SenderController.update { it.copy(encoderName = enc.codecName, hardware = enc.hardware) }
            if (!enc.hardware) Log.w(tag, "using a software encoder: ${enc.codecName}")
        } catch (t: Throwable) {
            Log.e(tag, "cannot apply ${p.width}x${p.height}@${p.fps}", t)
            SenderController.update { it.copy(state = SendState.Failed, error = "This phone's video encoder could not start (${t.message}).") }
            teardown(keepError = true)
            stopSelf()
        }
    }
}
