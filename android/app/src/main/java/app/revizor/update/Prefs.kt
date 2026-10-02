package app.revizor.update

import android.content.Context

/** Small settings store. Everything defaults to the privacy-preserving / least surprising choice. */
class Prefs(ctx: Context) {
    private val sp = ctx.getSharedPreferences("revizor", Context.MODE_PRIVATE)

    /** Check GitHub for new releases (the app's only internet request). */
    var autoUpdateCheck: Boolean
        get() = sp.getBoolean("autoUpdateCheck", true)
        set(v) = sp.edit().putBoolean("autoUpdateCheck", v).apply()

    /** Download and start the install as soon as an update is found (Android still confirms). */
    var autoInstall: Boolean
        get() = sp.getBoolean("autoInstall", false)
        set(v) = sp.edit().putBoolean("autoInstall", v).apply()

    var profile: Int
        get() = sp.getInt("profile", 1)
        set(v) = sp.edit().putInt("profile", v).apply()

    var audioMode: Int
        get() = sp.getInt("audioMode", 0)
        set(v) = sp.edit().putInt("audioMode", v).apply()

    var allowHevc: Boolean
        get() = sp.getBoolean("allowHevc", false)
        set(v) = sp.edit().putBoolean("allowHevc", v).apply()

    var lowLatencyReceiver: Boolean
        get() = sp.getBoolean("lowLatencyReceiver", false)
        set(v) = sp.edit().putBoolean("lowLatencyReceiver", v).apply()

    var deviceName: String
        get() = sp.getString("deviceName", null) ?: android.os.Build.MODEL
        set(v) = sp.edit().putString("deviceName", v).apply()

    var onboarded: Boolean
        get() = sp.getBoolean("onboarded", false)
        set(v) = sp.edit().putBoolean("onboarded", v).apply()
}
