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
import app.revizor.core.CastTv
import app.revizor.core.Discovered
import app.revizor.core.PairResult
import app.revizor.core.StatsView
import app.revizor.sender.CaptureService
import app.revizor.sender.SendMode
import app.revizor.sender.SendState
import app.revizor.sender.SenderController
import app.revizor.sender.SenderUi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
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
    val ctx = androidx.compose.ui.platform.LocalContext.current
    var devices by remember { mutableStateOf<List<Discovered>>(emptyList()) }
    var tvs by remember { mutableStateOf<List<CastTv>>(emptyList()) }
    var scanning by remember { mutableStateOf(true) }
    var pairing by remember { mutableStateOf<Discovered?>(null) }
    var message by remember { mutableStateOf<String?>(null) }
    var failure by remember { mutableStateOf(SenderController.ui.value.error) }

    // Keep looking while this screen is open: a TV that was just switched on, or a receiver that just opened pairing,
    // shows up by itself. Revizor receivers (UDP broadcast) and standard TVs (Google Cast / DLNA) are searched in parallel.
    LaunchedEffect(Unit) {
        while (true) {
            scanning = true
            val (r, t) = withContext(Dispatchers.IO) {
                coroutineScope {
                    val a = async { core.scan(1500) }
                    val b = async { core.scanTvs(2500) }
                    a.await() to b.await()
                }
            }
            devices = r
            tvs = t
            scanning = false
            delay(2000)
        }
    }

    val last = activity.prefs.lastTarget
    val trustedIps = devices.filter { it.trusted }.map { it.ip }.toSet()
    // A TV that already runs a paired Revizor receiver is shown once, as the (faster, encrypted) Revizor entry.
    val tvList = tvs.filter { it.ip !in trustedIps }.sortedByDescending { "tv:${it.ip}" == last }
    val revizorList = devices.sortedWith(compareByDescending<Discovered> { "revizor:${it.id}" == last }.thenByDescending { it.trusted })

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp)) {
        failure?.let { Notice(it, bad = true) }
        message?.let { Notice(it) }
        SectionTitle("Send to")
        if (revizorList.isEmpty() && tvList.isEmpty()) {
            Text(
                if (scanning) "Looking for TVs and screens on this Wi-Fi…" else "Nothing found on this Wi-Fi yet.\nTurn the TV on and make sure the phone and the TV use the same Wi-Fi network.",
                color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(vertical = 18.dp),
            )
        }
        tvList.forEach { tv ->
            RowCard(
                title = tv.name,
                subtitle = listOf(tv.methodLabels, tv.details, if ("tv:${tv.ip}" == last) "last used" else "").filter { it.isNotBlank() }.joinToString(" · "),
                trailing = { Tag("No app needed", Accent) },
            ) {
                failure = null
                activity.startSharingTv(tv)
            }
        }
        revizorList.forEach { d ->
            RowCard(
                title = d.name,
                subtitle = "Revizor · ${d.ip} · up to ${d.maxW}×${d.maxH} @ ${d.maxFps} fps" + if ("revizor:${d.id}" == last) " · last used" else "",
                trailing = {
                    when {
                        d.trusted -> Tag("Fast · encrypted", Ok)
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
        if (tvList.isNotEmpty()) {
            Gap(8)
            Text(
                "TVs marked “No app needed” show your screen using the TV's own Chromecast or DLNA support, so nothing has to be installed on them. " +
                    "The picture arrives a few seconds late and is not end-to-end encrypted (only the TV can open it). " +
                    "Install Revizor on the TV for the fastest, fully encrypted mirroring.",
                fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Gap(14)
        OutlinedButton(onClick = {
            val intents = listOf(android.provider.Settings.ACTION_CAST_SETTINGS, "android.settings.WIFI_DISPLAY_SETTINGS")
            val ok = intents.any { a -> runCatching { ctx.startActivity(android.content.Intent(a).addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK)) }.isSuccess }
            if (!ok) message = "This phone has no built-in cast screen. Look for “Smart View”, “Cast” or “Screen mirroring” in the quick settings."
        }, modifier = Modifier.fillMaxWidth()) { Text("Use Android's built-in screen cast") }
        Text("Works with Miracast TVs and anything the phone maker supports. Revizor is not involved.", fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(top = 4.dp))
        Gap(8)
        Text("Quality, resolution and frame rate for Revizor receivers are chosen automatically. Change the profile in Settings.", fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
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
    val tv = ui.mode == SendMode.Tv
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
        if (tv) {
            ui.tvStatus?.let { Notice(it) }
            ui.tvStats?.string("method")?.let { Text("via $it", fontSize = 13.sp, color = MaterialTheme.colorScheme.onSurfaceVariant) }
            if (ui.state == SendState.Streaming) {
                Text(
                    "The picture reaches the TV a few seconds late (the TV decides how much it buffers). It is not end-to-end encrypted: only this TV's address can open the stream.",
                    fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, textAlign = TextAlign.Center,
                )
            }
        } else {
            ui.limitedBy?.userText()?.let { Notice("Quality was lowered because of $it. It will recover automatically.") }
            if (ui.state == SendState.Streaming) Text("Encrypted end-to-end on your local network.", fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }

        SectionTitle("Live measurements")
        if (tv) {
            val t = ui.tvStats
            TileGrid(
                listOf(
                    "Stream bitrate" to StatsView.mbps(t?.double("inBps")),
                    "Sent to the TV" to (t?.double("bytesServed")?.let { "%.1f MB".format(it / 1e6) } ?: "—"),
                    "Connected TVs" to (t?.long("viewers")?.toString() ?: "—"),
                    "Video segments" to (t?.long("segments")?.toString() ?: "—"),
                    "Frames encoded" to (t?.long("framesIn")?.toString() ?: "—"),
                    "Running for" to (t?.long("uptimeS")?.let { "$it s" } ?: "—"),
                    "TV fetch requests" to (t?.let { "${it.long("playlistRequests") ?: 0} / ${it.long("segmentRequests") ?: 0}" } ?: "—"),
                    "Blocked requests" to (t?.long("rejected")?.toString() ?: "—"),
                ),
            )
        } else {
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
        }
        Gap(20)
        if (ui.state == SendState.Failed) {
            Button(onClick = { SenderController.reset() }) { Text("OK") }
        } else {
            OutlinedButton(onClick = { CaptureService.stop(ctx) }, colors = ButtonDefaults.outlinedButtonColors(contentColor = Bad)) { Text("Stop sharing") }
        }
    }
}
