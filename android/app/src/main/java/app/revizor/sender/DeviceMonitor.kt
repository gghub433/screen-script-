package app.revizor.sender

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.BatteryManager
import android.os.PowerManager
import app.revizor.core.Native

/** Feeds REAL thermal and battery state into the adaptive engine. No state → nothing is reported. */
class DeviceMonitor(private val context: Context, private val sender: Long) {
    private val pm = context.getSystemService(Context.POWER_SERVICE) as PowerManager
    private val thermalListener = PowerManager.OnThermalStatusChangedListener { status -> Native.senderThermal(sender, status) }
    private val batteryReceiver = object : BroadcastReceiver() {
        override fun onReceive(c: Context, i: Intent) = push(i)
    }

    fun start() {
        // Thermal API exists on Android 10+ (our minSdk). Some devices always report NONE; that is what the OS says.
        pm.addThermalStatusListener(thermalListener)
        Native.senderThermal(sender, pm.currentThermalStatus)
        val sticky = context.registerReceiver(batteryReceiver, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        sticky?.let { push(it) }
        context.registerReceiver(batteryReceiver, IntentFilter(PowerManager.ACTION_POWER_SAVE_MODE_CHANGED))
    }

    private fun push(i: Intent) {
        val level = i.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
        val scale = i.getIntExtra(BatteryManager.EXTRA_SCALE, 100)
        if (level < 0 || scale <= 0) return
        val status = i.getIntExtra(BatteryManager.EXTRA_STATUS, -1)
        val charging = status == BatteryManager.BATTERY_STATUS_CHARGING || status == BatteryManager.BATTERY_STATUS_FULL
        Native.senderBattery(sender, level * 100 / scale, charging, pm.isPowerSaveMode)
    }

    fun stop() {
        runCatching { pm.removeThermalStatusListener(thermalListener) }
        runCatching { context.unregisterReceiver(batteryReceiver) }
    }
}
