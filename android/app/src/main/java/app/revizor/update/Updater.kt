package app.revizor.update

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.net.Uri
import android.os.Build
import android.provider.Settings
import app.revizor.BuildConfig
import app.revizor.core.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

data class UpdateInfo(val versionName: String, val versionCode: Int, val apkUrl: String, val sha256Url: String, val notes: String, val sizeBytes: Long)

/**
 * In-app updater over GitHub Releases.
 *
 * Trust model: the APK is only installed if (1) its SHA-256 matches the checksum asset published
 * next to it, and (2) Android accepts it as an update of this very app, which requires the same
 * signing key — an attacker who can edit a release but does not hold the key cannot get code onto
 * the phone. This is the app's only connection to the internet and can be turned off in Settings;
 * it never sends any user data (an unauthenticated GET of the public release list).
 */
object Updater {
    private const val TAG = "Updater"
    // revizor-<versionName>-<versionCode>.apk  (+ the same name with .sha256)
    private val apkName = Regex("""^revizor-(\d+\.\d+\.\d+)-(\d+)\.apk$""")
    private val allowedHosts = setOf("github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com")

    suspend fun check(): UpdateInfo? = withContext(Dispatchers.IO) {
        val json = http("https://api.github.com/repos/${BuildConfig.UPDATE_REPO}/releases/latest", accept = "application/vnd.github+json")
            ?.toString(Charsets.UTF_8) ?: return@withContext null
        val rel = JSONObject(json)
        val assets = rel.optJSONArray("assets") ?: return@withContext null
        var best: UpdateInfo? = null
        for (i in 0 until assets.length()) {
            val a = assets.getJSONObject(i)
            val m = apkName.matchEntire(a.optString("name")) ?: continue
            val code = m.groupValues[2].toInt()
            val sha = (0 until assets.length()).map { assets.getJSONObject(it) }.firstOrNull { it.optString("name") == a.optString("name") + ".sha256" } ?: continue
            if (best == null || code > best.versionCode) {
                best = UpdateInfo(m.groupValues[1], code, a.getString("browser_download_url"), sha.getString("browser_download_url"), rel.optString("body"), a.optLong("size"))
            }
        }
        best?.takeIf { it.versionCode > BuildConfig.VERSION_CODE }
    }

    /** Downloads and verifies the APK; returns the file. [progress] receives 0..1. */
    suspend fun download(ctx: Context, info: UpdateInfo, progress: (Float) -> Unit = {}): File = withContext(Dispatchers.IO) {
        val expected = http(info.sha256Url, maxBytes = 4096)?.toString(Charsets.UTF_8)?.trim()?.split(Regex("\\s+"))?.firstOrNull()?.lowercase()
            ?: error("The checksum for this update could not be downloaded")
        require(expected.length == 64) { "Malformed checksum" }
        val dir = File(ctx.cacheDir, "updates").apply { deleteRecursively(); mkdirs() }
        val out = File(dir, "revizor-${info.versionName}-${info.versionCode}.apk")
        val md = MessageDigest.getInstance("SHA-256")
        val c = open(info.apkUrl)
        try {
            val total = c.contentLengthLong.takeIf { it > 0 } ?: info.sizeBytes
            var done = 0L
            c.inputStream.use { ins -> out.outputStream().use { os ->
                val buf = ByteArray(64 * 1024)
                while (true) {
                    val n = ins.read(buf); if (n < 0) break
                    md.update(buf, 0, n); os.write(buf, 0, n); done += n
                    if (total > 0) progress((done.toFloat() / total).coerceIn(0f, 1f))
                    require(done <= 200L * 1024 * 1024) { "Update is unexpectedly large" }
                }
            } }
        } finally { c.disconnect() }
        val actual = md.digest().joinToString("") { "%02x".format(it) }
        if (actual != expected) { out.delete(); error("The downloaded update failed its integrity check") }
        Log.i(TAG, "downloaded and verified ${out.name}")
        out
    }

    fun canInstall(ctx: Context) = ctx.packageManager.canRequestPackageInstalls()

    /** Opens the system screen where the user allows Revizor to install updates (one-time). */
    fun requestInstallPermission(ctx: Context) {
        ctx.startActivity(Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${ctx.packageName}")).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }

    /** Hands the verified APK to the system installer. Android shows its confirmation (silent only for follow-up updates on 12+). */
    fun install(ctx: Context, apk: File) {
        val pi = ctx.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
            setAppPackageName(ctx.packageName)
            if (Build.VERSION.SDK_INT >= 31) setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
        }
        val id = pi.createSession(params)
        pi.openSession(id).use { s ->
            apk.inputStream().use { ins -> s.openWrite("revizor.apk", 0, apk.length()).use { os -> ins.copyTo(os); s.fsync(os) } }
            val intent = Intent(ctx, InstallResultReceiver::class.java).setPackage(ctx.packageName)
            val pending = PendingIntent.getBroadcast(ctx, id, intent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE)
            s.commit(pending.intentSender)
        }
    }

    private fun readLimited(ins: java.io.InputStream, max: Int): ByteArray {
        val out = java.io.ByteArrayOutputStream()
        val buf = ByteArray(8192)
        while (out.size() < max) {
            val n = ins.read(buf, 0, minOf(buf.size, max - out.size()))
            if (n < 0) break
            out.write(buf, 0, n)
        }
        return out.toByteArray()
    }

    private fun open(url: String): HttpURLConnection {
        var u = URL(url)
        repeat(5) {
            require(u.protocol == "https" && (u.host in allowedHosts || u.host == "api.github.com")) { "Refusing to download from ${u.host}" }
            val c = u.openConnection() as HttpURLConnection
            c.instanceFollowRedirects = false
            c.connectTimeout = 10_000; c.readTimeout = 20_000
            c.setRequestProperty("User-Agent", "Revizor/${BuildConfig.VERSION_NAME}")
            if (c.responseCode in 300..399) {
                val loc = c.getHeaderField("Location") ?: error("Redirect without location")
                c.disconnect(); u = URL(u, loc); return@repeat
            }
            if (c.responseCode != 200) { c.disconnect(); error("HTTP ${c.responseCode}") }
            return c
        }
        error("Too many redirects")
    }

    private fun http(url: String, accept: String? = null, maxBytes: Int = 512 * 1024): ByteArray? = runCatching {
        var u = URL(url)
        repeat(5) {
            require(u.protocol == "https" && (u.host in allowedHosts || u.host == "api.github.com")) { "host" }
            val c = u.openConnection() as HttpURLConnection
            c.instanceFollowRedirects = false
            c.connectTimeout = 10_000; c.readTimeout = 15_000
            c.setRequestProperty("User-Agent", "Revizor/${BuildConfig.VERSION_NAME}")
            accept?.let { c.setRequestProperty("Accept", it) }
            if (c.responseCode in 300..399) { val loc = c.getHeaderField("Location"); c.disconnect(); u = URL(u, loc); return@repeat }
            if (c.responseCode != 200) { c.disconnect(); return null }
            return c.inputStream.use { readLimited(it, maxBytes) }.also { c.disconnect() }
        }
        null
    }.getOrElse { Log.w(TAG, "request failed: ${it.message}"); null }
}
