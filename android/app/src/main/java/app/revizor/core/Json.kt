package app.revizor.core

import org.json.JSONObject

/** Typed view over the JSON statistics produced by the Rust core. `null` = not measured (show "—"). */
class StatsView(private val j: JSONObject) {
    private fun num(k: String): Double? = if (j.isNull(k) || !j.has(k)) null else j.optDouble(k)
    fun double(k: String): Double? = num(k)
    fun long(k: String): Long? = num(k)?.toLong()
    fun string(k: String): String? = if (j.isNull(k) || !j.has(k)) null else j.optString(k)

    companion object {
        fun parse(s: String): StatsView? = runCatching { StatsView(JSONObject(s)) }.getOrNull()
        fun ms(us: Double?): String = us?.let { "%.1f ms".format(it / 1000.0) } ?: "—"
        fun mbps(bps: Double?): String = bps?.let { "%.2f Mbit/s".format(it / 1e6) } ?: "—"
        fun pct(v: Double?): String = v?.let { "%.1f %%".format(v) } ?: "—"
    }
}
