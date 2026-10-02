package app.revizor.ui

import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import app.revizor.RevizorApp
import app.revizor.core.StatsView
import app.revizor.receiver.ReceiverController
import app.revizor.receiver.RecvState

@Composable
fun ReceiverScreen(activity: MainActivity, onExit: () -> Unit) {
    val ui by ReceiverController.ui.collectAsStateCompat()
    val view = LocalView.current
    var overlay by remember { mutableStateOf(false) }
    val streaming = ui.state == RecvState.Streaming && ui.params != null

    DisposableEffect(Unit) {
        ReceiverController.lowLatency = activity.prefs.lowLatencyReceiver
        ReceiverController.start(activity, RevizorApp.core, activity.prefs.deviceName)
        view.keepScreenOn = true
        onDispose {
            view.keepScreenOn = false
            ReceiverController.stop()
        }
    }
    // Fullscreen (hide system bars) while a stream is on screen.
    DisposableEffect(streaming) {
        val c = WindowCompat.getInsetsController(activity.window, view)
        if (streaming) {
            c.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            c.hide(WindowInsetsCompat.Type.systemBars())
        }
        onDispose { c.show(WindowInsetsCompat.Type.systemBars()) }
    }

    Box(Modifier.fillMaxSize().background(Color.Black).clickable { overlay = !overlay }) {
        // The decoder renders straight into this Surface (no bitmap copies). Aspect ratio comes from the stream: never stretched.
        val ratio = ui.params?.let { it.width.toFloat() / it.height.toFloat() } ?: (16f / 9f)
        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            AndroidView(
                modifier = Modifier.aspectRatio(ratio),
                factory = { ctx ->
                    SurfaceView(ctx).apply {
                        holder.addCallback(object : SurfaceHolder.Callback {
                            override fun surfaceCreated(h: SurfaceHolder) = ReceiverController.setSurface(h.surface)
                            override fun surfaceChanged(h: SurfaceHolder, f: Int, w: Int, hh: Int) = ReceiverController.setSurface(h.surface)
                            override fun surfaceDestroyed(h: SurfaceHolder) = ReceiverController.setSurface(null)
                        })
                    }
                },
            )
        }
        if (!streaming) WaitingPanel(ui.pin, ui.lastPaired, ui.pairingLocked, ui.state, activity.prefs.deviceName, onExit)
        else if (overlay) StatsOverlay(ui.stats, ui.decoder, ui.decoderHardware, ui.sender)
    }
}

@Composable
private fun WaitingPanel(pin: String?, lastPaired: String?, locked: Boolean, state: RecvState, name: String, onExit: () -> Unit) {
    Column(
        Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background).padding(24.dp),
        verticalArrangement = Arrangement.Center, horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("REVIZOR", fontSize = 16.sp, fontWeight = FontWeight.Bold, letterSpacing = 6.sp, color = Accent)
        Gap(18)
        if (pin != null) {
            Text("Enter this code on your phone or PC", color = MaterialTheme.colorScheme.onSurfaceVariant)
            Gap(10)
            Text(pin.chunked(3).joinToString(" "), fontSize = 54.sp, fontWeight = FontWeight.Bold, letterSpacing = 6.sp)
            Gap(14)
            OutlinedButton(onClick = { ReceiverController.closePairing() }) { Text("Cancel") }
        } else {
            Text(if (state == RecvState.Streaming) "Starting…" else "Ready for a screen", fontSize = 26.sp, fontWeight = FontWeight.Bold, textAlign = TextAlign.Center)
            Gap(6)
            Text("This device is called “$name”.\nOpen Revizor on the other device and pick it.", textAlign = TextAlign.Center, color = MaterialTheme.colorScheme.onSurfaceVariant)
            lastPaired?.let { Gap(10); Text("Paired with $it ✓", color = Ok) }
            if (locked) { Gap(10); Text("Too many wrong codes. Start pairing again.", color = Bad) }
            Gap(22)
            Button(onClick = { ReceiverController.openPairing() }) { Text("Pair a new device") }
            Gap(8)
            OutlinedButton(onClick = onExit) { Text("Back") }
        }
    }
}

@Composable
private fun StatsOverlay(s: StatsView?, decoder: String?, hw: Boolean?, sender: String?) {
    Box(Modifier.fillMaxSize().padding(16.dp), contentAlignment = Alignment.TopStart) {
        Column(Modifier.background(Color(0xCC000000)).padding(12.dp)) {
            Text(sender ?: "", fontWeight = FontWeight.Bold, color = Color.White)
            val lines = listOf(
                "Stream  ${s?.long("width") ?: "—"}×${s?.long("height") ?: "—"} @ ${s?.long("fpsTarget") ?: "—"} (showing ${"%.0f".format(s?.double("fps") ?: 0.0)} fps)",
                "Bitrate ${StatsView.mbps(s?.double("recvBps"))}",
                "Delay   ${StatsView.ms(s?.double("e2eUs"))}   (arrival ${StatsView.ms(s?.double("arrivalLatencyUs"))})",
                "RTT ${StatsView.ms(s?.double("rttUs"))}   Jitter ${StatsView.ms(s?.double("jitterUs"))}   Loss ${StatsView.pct(s?.double("lossPct"))}",
                "Decode  ${StatsView.ms(s?.double("decodeUs"))}   ${decoder ?: "—"} ${if (hw == true) "(hardware)" else if (hw == false) "(software)" else ""}",
                "Repaired fec ${s?.long("recoveredFec") ?: "—"} / retx ${s?.long("recoveredRetx") ?: "—"}   dropped ${(s?.long("framesAbandoned") ?: 0) + (s?.long("framesDiscarded") ?: 0) + (s?.long("framesDroppedApp") ?: 0)}",
            )
            lines.forEach { Text(it, color = Color.White, fontSize = 12.sp, fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace) }
        }
    }
}
