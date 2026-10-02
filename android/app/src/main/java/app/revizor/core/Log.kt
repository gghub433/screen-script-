package app.revizor.core

import android.content.Context
import java.io.File
import java.text.SimpleDateFormat
import java.util.ArrayDeque
import java.util.Date
import java.util.Locale

/**
 * In-memory ring buffer of diagnostic lines. Only operational facts go in here
 * (states, parameters, errors) — never screen or audio content, IPs are kept because they are
 * needed to debug connectivity but contain no personal data.
 */
object Log {
    private const val MAX = 800
    private val lines = ArrayDeque<String>(MAX)
    private val fmt = SimpleDateFormat("HH:mm:ss.SSS", Locale.US)

    @Synchronized
    fun i(tag: String, msg: String) {
        android.util.Log.i("Revizor/$tag", msg)
        add("I", tag, msg)
    }

    @Synchronized
    fun w(tag: String, msg: String) {
        android.util.Log.w("Revizor/$tag", msg)
        add("W", tag, msg)
    }

    @Synchronized
    fun e(tag: String, msg: String, t: Throwable? = null) {
        android.util.Log.e("Revizor/$tag", msg, t)
        add("E", tag, msg + (t?.let { " (${it.javaClass.simpleName}: ${it.message})" } ?: ""))
    }

    private fun add(level: String, tag: String, msg: String) {
        if (lines.size >= MAX) lines.removeFirst()
        lines.addLast("${fmt.format(Date())} $level $tag: $msg")
    }

    @Synchronized
    fun snapshot(): List<String> = lines.toList()

    /** Writes the log to the cache dir and returns the file (for a share intent via FileProvider). */
    @Synchronized
    fun export(context: Context): File {
        val dir = File(context.cacheDir, "logs").apply { mkdirs() }
        val f = File(dir, "revizor-log.txt")
        f.writeText("Revizor ${app.revizor.BuildConfig.VERSION_NAME} (${app.revizor.BuildConfig.VERSION_CODE})\n" +
            "Android ${android.os.Build.VERSION.RELEASE} / ${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}\n\n" +
            lines.joinToString("\n"))
        return f
    }
}
