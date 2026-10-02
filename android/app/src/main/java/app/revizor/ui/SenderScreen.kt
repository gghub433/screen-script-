package app.revizor.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import app.revizor.RevizorApp
import app.revizor.core.Discovered
import app.revizor.core.PairResult
import app.revizor.core.StatsView
import app.revizor.sender.CaptureService
import app.revizor.sender.SendState
import app.revizor.sender.SenderController
import app.revizor.sender.SenderUi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

@Composable
fun SenderScreen(activity: MainActivity, onBack: () -> Unit) {
    val ui by SenderController.ui.collectAsStateCompat()
    Column(Modifier.fillMaxSize()) {
        Header("SHARE", onBack = if (ui.state == SendState.Idle) onBack else null)
        if (ui.state == SendState.Idle) DeviceList(activity) else LivePanel(ui)
    }
}

@Composable
private fun DeviceList(activity: MainActivity) {
    val core = RevizorApp.core
    var devices by remember { mutableStateOf<List<Discovered>>(emptyList()) }
    var scanning by remember { mutableStateOf(true) }
    var pairing by remember { mutableStateOf<Discovered?>(null) }
    var message by remember { mutableStateOf<String?>(null) }
    var failure by remember { mutableStateOf(SenderController.ui.value.error) }

    // Keep looking while this screen is open so a TV that just opened pairing shows up by itself.
    LaunchedEffect(Unit) {
        while (true) {
            scanning = true
            devices = withContext(Dispatchers.IO) { core.scan(1500) }
            scanning = false
            delay(1500)
        }
    }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp)) {
        failure?.let { Notice(it, bad = true) }
        message?.let { Notice(it) }
        SectionTitle("Send to")
        if (devices.isEmpty()) {
            Text(
                if (scanning) "Looking for receivers…" else "No receivers found.\nOpen Revizor on your TV or PC and make sure both are on the same Wi-Fi.",
                color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(vertical = 18.dp),
            )
        }
        devices.forEach { d ->
            RowCard(
                title = d.name,
                subtitle = "${d.ip} · up to ${d.maxW}×${d.maxH} @ ${d.maxFps} fps",
                trailing = {
                    when {
                        d.trusted -> Tag("Ready", Ok)
                        d.pairingOpen -> Tag("Tap to pair", Warn)
                        else -> Tag("Pairing is off")
                    }
                },
            ) {
                failure = null
                when {
                    d.trusted -> activity.startSharing(d)
                    d.pairingOpen -> pairing = d
                    else -> message = "On ${d.name}, choose “Pair a new device”, then tap it here."
                }
            }
        }
        Gap(8)
        Text("Quality, resolution and frame rate are chosen automatically and adapt while you stream. Change the profile in Settings.", fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }

    pairing?.let { d -> PinDialog(d, onDone = { ok -> pairing = null; if (ok) { message = "Paired with ${d.name}. Tap it to start."; } }) }
}

@Composable
private fun PinDialog(d: Discovered, onDone: (Boolean) -> Unit) {
    var pin by remember { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    AlertDialog(
        onDismissRequest = { if (!busy) onDone(false) },
        title = { Text("Pair with ${d.name}") },
        text = {
            Column {
                Text("Enter the 6-digit code shown on ${d.name}.")
                Gap(8)
                OutlinedTextField(value = pin, onValueChange = { if (it.length <= 6 && it.all(Char::isDigit)) pin = it }, singleLine = true, textStyle = androidx.compose.ui.text.TextStyle(fontSize = 26.sp, letterSpacing = 8.sp, textAlign = TextAlign.Center))
                error?.let { Text(it, color = Bad, fontSize = 13.sp) }
            }
        },
        confirmButton = {
            Button(enabled = pin.length == 6 && !busy, onClick = {
                busy = true; error = null
                scope.launch {
                    val r = withContext(Dispatchers.IO) { RevizorApp.core.pair(d, pin, RevizorApp.instance.let { app.revizor.update.Prefs(it).deviceName }) }
                    busy = false
                    when (r) {
                        PairResult.Ok -> onDone(true)
                        PairResult.WrongPin -> error = "Wrong code. Check the screen of ${d.name}."
                        PairResult.NoAnswer -> error = "${d.name} did not answer. Is pairing still open?"
                        PairResult.Error -> error = "Could not reach ${d.name}."
                    }
                }
            }) { Text(if (busy) "Pairing…" else "Pair") }
        },
        dismissButton = { TextButton(enabled = !busy, onClick = { onDone(false) }) { Text("Cancel") } },
    )
}

@Composable
private fun LivePanel(ui: SenderUi) {
    val ctx = androidx.compose.ui.platform.LocalContext.current
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp), horizontalAlignment = Alignment.CenterHorizontally) {
        Gap(20)
        val title = when (ui.state) {
            SendState.Streaming -> "Sharing to ${ui.peer ?: "receiver"}"
            SendState.Reconnecting -> "Connection lost — reconnecting…"
            SendState.Failed -> "Sharing stopped"
            else -> "Connecting…"
        }
        Text(title, fontSize = 22.sp, fontWeight = FontWeight.Bold, textAlign = TextAlign.Center)
        ui.params?.let { p ->
            Gap(6)
            Text("${minOf(p.width, p.height)}p · ${p.fps} fps · ${"%.1f".format(p.videoBps / 1e6)} Mbit/s · ${p.codec.name}", color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        ui.hardware?.let { Text(if (it) "Hardware video encoder" else "Software video encoder — higher battery use", color = if (it) MaterialTheme.colorScheme.onSurfaceVariant else Warn, fontSize = 13.sp) }
        Gap(10)
        ui.error?.let { Notice(it, bad = true) }
        ui.limitedBy?.userText()?.let { Notice("Quality was lowered because of $it. It will recover automatically.") }
        if (ui.state == SendState.Streaming) Text("Encrypted end-to-end on your local network.", fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)

        SectionTitle("Live measurements")
        val s = ui.stats
        TileGrid(
            listOf(
                "Delay (screen to screen)" to StatsView.ms(s?.double("e2eUs")),
                "Round trip" to StatsView.ms(s?.double("rttUs")),
                "Packet loss" to StatsView.pct(s?.double("lossPct")),
                "Jitter" to StatsView.ms(s?.double("jitterUs")),
                "Sending" to StatsView.mbps(s?.double("sentBps")),
                "Encoder time" to StatsView.ms(s?.double("encodeUs")),
                "Receiver decode" to StatsView.ms(s?.double("decodeUs")),
                "Dropped here" to (s?.long("framesDroppedSender")?.toString() ?: "—"),
                "Repaired packets" to (s?.long("retransmitted")?.toString() ?: "—"),
                "Error protection" to (s?.long("fecK")?.let { if (it == 0L) "off" else "1 in $it" } ?: "—"),
            ),
        )
        Gap(20)
        if (ui.state == SendState.Failed) {
            Button(onClick = { SenderController.reset() }) { Text("OK") }
        } else {
            OutlinedButton(onClick = { CaptureService.stop(ctx) }, colors = ButtonDefaults.outlinedButtonColors(contentColor = Bad)) { Text("Stop sharing") }
        }
    }
}
