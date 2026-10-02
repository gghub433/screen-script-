package app.revizor

import android.app.Application
import android.net.wifi.WifiManager
import app.revizor.core.Core
import app.revizor.core.Log
import app.revizor.update.Prefs
import app.revizor.update.UpdateWorker

class RevizorApp : Application() {
    private var multicastLock: WifiManager.MulticastLock? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        core = Core(this)
        Log.i("App", "Revizor ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE}) device ${core.deviceId.take(8)}")
        // Wi-Fi drivers drop broadcast/multicast frames to save power unless asked not to; discovery needs them.
        val wifi = applicationContext.getSystemService(WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("revizor-discovery").apply { setReferenceCounted(false); acquire() }
        if (Prefs(this).autoUpdateCheck) UpdateWorker.schedule(this) else UpdateWorker.cancel(this)
    }

    companion object {
        lateinit var instance: RevizorApp
            private set
        lateinit var core: Core
            private set
    }
}
