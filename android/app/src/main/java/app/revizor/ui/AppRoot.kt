package app.revizor.ui

import android.content.pm.PackageManager
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import app.revizor.BuildConfig
import app.revizor.sender.SendState
import app.revizor.sender.SenderController
import app.revizor.update.UpdateInfo
import app.revizor.update.Updater
import kotlinx.coroutines.launch

enum class Screen { Home, Send, Receive, Settings, Benchmark }

@Composable
fun AppRoot(activity: MainActivity) {
    val tv = remember { activity.packageManager.hasSystemFeature(PackageManager.FEATURE_LEANBACK) }
    var screen by remember { mutableStateOf(if (tv) Screen.Receive else Screen.Home) }
    val sending = SenderController.ui.collectAsStateCompat().value.state != SendState.Idle
    LaunchedEffect(sending) { if (sending) screen = Screen.Send }
    BackHandler(enabled = screen != Screen.Home && !(tv && screen == Screen.Receive)) { screen = if (screen == Screen.Benchmark) Screen.Settings else Screen.Home }

    Column(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background).statusBarsPadding().navigationBarsPadding()) {
        when (screen) {
            Screen.Home -> HomeScreen(activity, onSend = { screen = Screen.Send }, onReceive = { screen = Screen.Receive }, onSettings = { screen = Screen.Settings })
            Screen.Send -> SenderScreen(activity, onBack = { screen = Screen.Home })
            Screen.Receive -> ReceiverScreen(activity, onExit = { screen = Screen.Home })
            Screen.Settings -> SettingsScreen(activity, onBack = { screen = Screen.Home }, onBenchmark = { screen = Screen.Benchmark })
            Screen.Benchmark -> BenchmarkScreen(onBack = { screen = Screen.Settings })
        }
    }
}

@Composable
fun Header(title: String, onBack: (() -> Unit)? = null, action: (@Composable () -> Unit)? = null) {
    Row(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 10.dp), verticalAlignment = Alignment.CenterVertically) {
        if (onBack != null) TextButton(onClick = onBack) { Text("‹ Back") }
        Text(title, fontSize = 15.sp, fontWeight = FontWeight.Bold, letterSpacing = 4.sp, modifier = Modifier.weight(1f).padding(start = if (onBack == null) 8.dp else 0.dp))
        action?.invoke()
    }
}

@Composable
fun HomeScreen(activity: MainActivity, onSend: () -> Unit, onReceive: () -> Unit, onSettings: () -> Unit) {
    val scope = rememberCoroutineScope()
    var update by remember { mutableStateOf<UpdateInfo?>(null) }
    LaunchedEffect(Unit) { if (activity.prefs.autoUpdateCheck) update = runCatching { Updater.check() }.getOrNull() }
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp)) {
        Header("REVIZOR", action = { TextButton(onClick = onSettings) { Text("Settings") } })
        update?.let { u -> UpdateBanner(activity, u) }
        Gap(18)
        BigChoice("Share this screen", "Send this device's screen to a TV, PC or another phone.", onSend)
        Gap(10)
        BigChoice("Use this device as a screen", "Show another device's screen here, full size.", onReceive)
        Gap(24)
        Text("Everything stays on your local network and is end-to-end encrypted.", color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 13.sp)
    }
}

@Composable
private fun BigChoice(title: String, sub: String, onClick: () -> Unit) {
    Surface(onClick = onClick, shape = RoundedCornerShape(18.dp), color = MaterialTheme.colorScheme.surface, border = androidx.compose.foundation.BorderStroke(1.dp, MaterialTheme.colorScheme.outline), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(22.dp)) {
            Text(title, fontSize = 20.sp, fontWeight = FontWeight.Bold)
            Gap(4)
            Text(sub, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

@Composable
fun UpdateBanner(activity: MainActivity, info: UpdateInfo) {
    val scope = rememberCoroutineScope()
    val ctx = LocalContext.current
    var progress by remember { mutableStateOf<Float?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    Surface(shape = RoundedCornerShape(14.dp), color = Accent.copy(alpha = 0.12f), border = androidx.compose.foundation.BorderStroke(1.dp, Accent.copy(alpha = 0.5f)), modifier = Modifier.fillMaxWidth().padding(top = 8.dp)) {
        Column(Modifier.padding(14.dp)) {
            Text("Update available: ${info.versionName}", fontWeight = FontWeight.SemiBold)
            Text("You have ${BuildConfig.VERSION_NAME}.", fontSize = 13.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
            error?.let { Text(it, color = Bad, fontSize = 13.sp) }
            Gap(8)
            Button(
                enabled = progress == null,
                onClick = {
                    if (!Updater.canInstall(ctx)) { Updater.requestInstallPermission(ctx); error = "Allow installing updates for Revizor, then press Update again."; return@Button }
                    error = null; progress = 0f
                    scope.launch {
                        runCatching { Updater.install(ctx, Updater.download(ctx, info) { progress = it }) }.onFailure { error = it.message; progress = null }
                    }
                },
                colors = ButtonDefaults.buttonColors(containerColor = Accent, contentColor = Ink),
            ) { Text(if (progress == null) "Update now" else "Downloading… ${(progress!! * 100).toInt()}%") }
        }
    }
}
