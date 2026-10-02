package app.revizor.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import app.revizor.benchmark.BenchResult
import app.revizor.benchmark.Benchmark
import kotlinx.coroutines.launch

@Composable
fun BenchmarkScreen(onBack: () -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val results = remember { mutableStateListOf<BenchResult>() }
    var running by remember { mutableStateOf(false) }
    Column(Modifier.fillMaxSize()) {
        Header("PERFORMANCE TEST", onBack = if (running) null else onBack)
        Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp)) {
            Text(
                "Encodes and decodes an animated test pattern with this device's video hardware for ${5} seconds per mode and reports what it actually achieved. " +
                    "Keep the screen on. Takes about 30 seconds and warms the device a little.",
                color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 13.5.sp,
            )
            Gap(10)
            Button(enabled = !running, onClick = {
                results.clear(); running = true
                scope.launch { Benchmark.run(ctx) { r -> results.add(r) }; running = false }
            }) { Text(if (running) "Testing…" else "Start test") }
            results.forEach { r ->
                SectionTitle(r.mode.label)
                if (!r.supported) {
                    Text(r.note ?: "Not supported", color = MaterialTheme.colorScheme.onSurfaceVariant)
                } else {
                    fun f(v: Double?, unit: String, d: Int = 1) = v?.let { "%.${d}f $unit".format(it) } ?: "—"
                    TileGrid(
                        listOf(
                            "Frames per second" to f(r.avgFps, "fps"),
                            "Frames lost" to f(r.droppedPct, "%"),
                            "Encode time" to f(r.encodeAvgMs, "ms"),
                            "Encode (95th %)" to f(r.encodeP95Ms, "ms"),
                            "Decode time" to f(r.decodeAvgMs, "ms"),
                            "Bitrate used" to f(r.bitrateMbps, "Mbit/s"),
                            "CPU (this app)" to f(r.cpuPct, "%", 0),
                            "Temperature state" to (if (r.thermalStart != null) "${r.thermalStart} → ${r.thermalEnd}" else "—"),
                            "Battery drawn" to (r.chargeUsedUah?.let { "$it µAh" } ?: "—"),
                        ),
                    )
                    r.note?.let { Text(it, fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                }
            }
            if (results.isNotEmpty()) Text(
                "\nGPU load, and network loss/jitter, cannot be measured here and are not shown. Battery drawn is the charge-counter change during the run and is coarse on short tests.",
                fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Gap(30)
        }
    }
}
