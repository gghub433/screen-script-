package app.revizor.ui

import android.content.Intent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.FileProvider
import app.revizor.BuildConfig
import app.revizor.RevizorApp
import app.revizor.core.Log
import app.revizor.sender.AudioMode
import app.revizor.update.UpdateInfo
import app.revizor.update.UpdateWorker
import app.revizor.update.Updater
import kotlinx.coroutines.launch

@Composable
fun SettingsScreen(activity: MainActivity, onBack: () -> Unit, onBenchmark: () -> Unit) {
    val prefs = activity.prefs
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    var profile by remember { mutableStateOf(prefs.profile) }
    var audio by remember { mutableStateOf(prefs.audioMode) }
    var hevc by remember { mutableStateOf(prefs.allowHevc) }
    var lowLat by remember { mutableStateOf(prefs.lowLatencyReceiver) }
    var check by remember { mutableStateOf(prefs.autoUpdateCheck) }
    var auto by remember { mutableStateOf(prefs.autoInstall) }
    var name by remember { mutableStateOf(prefs.deviceName) }
    var trusted by remember { mutableStateOf(RevizorApp.core.trusted()) }
    var update by remember { mutableStateOf<UpdateInfo?>(null) }
    var updateMsg by remember { mutableStateOf<String?>(null) }

    Column(Modifier.fillMaxSize()) {
        Header("SETTINGS", onBack = onBack)
        Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp)) {
            SectionTitle("Quality")
            val profiles = listOf("Save power" to 0, "Balanced" to 1, "Best quality" to 2, "Lowest delay" to 3)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.fillMaxWidth()) {
                profiles.forEach { (label, v) ->
                    FilterChip(selected = profile == v, onClick = { profile = v; prefs.profile = v }, label = { Text(label, fontSize = 12.sp, maxLines = 1) })
                }
            }
            Text("Resolution, frame rate and bitrate always adapt automatically; the profile only changes what the app prefers when it has to choose.", fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)

            SectionTitle("Sound when sharing")
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                listOf("Off" to AudioMode.None, "Device sound" to AudioMode.System, "Microphone" to AudioMode.Mic, "Both" to AudioMode.Both).forEach { (label, m) ->
                    FilterChip(selected = audio == m.ordinal, onClick = { audio = m.ordinal; prefs.audioMode = m.ordinal }, label = { Text(label, fontSize = 12.sp, maxLines = 1) })
                }
            }
            Text("Microphone access is only requested if you pick Microphone or Both.", fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)

            SectionTitle("This device")
            OutlinedTextField(value = name, onValueChange = { name = it.take(40); prefs.deviceName = name.ifBlank { android.os.Build.MODEL } }, label = { Text("Name shown to other devices") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            Gap(6)
            Toggle("Lowest delay when receiving", "Shows frames the moment they are decoded. May look slightly less smooth on shaky Wi-Fi.", lowLat) { lowLat = it; prefs.lowLatencyReceiver = it }

            SectionTitle("Paired devices")
            if (trusted.isEmpty()) Text("None yet.", color = MaterialTheme.colorScheme.onSurfaceVariant)
            trusted.forEach { t ->
                Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) { Text(t.name); Text(t.id.take(8), fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                    TextButton(onClick = { RevizorApp.core.forget(t.id); trusted = RevizorApp.core.trusted() }) { Text("Remove", color = Bad) }
                }
            }

            SectionTitle("Updates")
            Toggle("Check for updates", "Asks GitHub for the newest release now and then. This is the only internet request the app makes. Nothing about you is sent.", check) {
                check = it; prefs.autoUpdateCheck = it; if (it) UpdateWorker.schedule(ctx) else UpdateWorker.cancel(ctx)
            }
            Toggle("Install updates automatically", "Downloads the update and opens Android's installer. Android may still ask you to confirm.", auto && check, enabled = check) { auto = it; prefs.autoInstall = it }
            Gap(6)
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                OutlinedButton(onClick = {
                    updateMsg = "Checking…"
                    scope.launch {
                        val u = runCatching { Updater.check() }.getOrNull()
                        update = u
                        updateMsg = if (u == null) "You have the latest version (${BuildConfig.VERSION_NAME})." else null
                    }
                }) { Text("Check now") }
                Text("Version ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE})", fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            updateMsg?.let { Text(it, fontSize = 13.sp, color = MaterialTheme.colorScheme.onSurfaceVariant) }
            update?.let { UpdateBanner(activity, it) }

            SectionTitle("Advanced")
            Toggle("Allow HEVC (H.265)", "Experimental. Uses less bandwidth when both devices have a hardware HEVC codec; H.264 is always the fallback.", hevc) { hevc = it; prefs.allowHevc = it }
            Gap(8)
            OutlinedButton(onClick = onBenchmark, modifier = Modifier.fillMaxWidth()) { Text("Test this device's video performance") }
            Gap(8)
            OutlinedButton(modifier = Modifier.fillMaxWidth(), onClick = {
                val f = Log.export(ctx)
                val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.files", f)
                ctx.startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_STREAM, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION), "Share diagnostic log"))
            }) { Text("Export diagnostic log") }
            Text("The log lists states, settings and errors. It never contains your screen or sound.", fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Gap(8)
            Text("Device ID ${RevizorApp.core.deviceId.take(16)}", fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Gap(30)
        }
    }
}

@Composable
private fun Toggle(title: String, sub: String, value: Boolean, enabled: Boolean = true, onChange: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth().padding(vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f).padding(end = 12.dp)) {
            Text(title)
            Text(sub, fontSize = 12.5.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Switch(checked = value, onCheckedChange = onChange, enabled = enabled)
    }
}
