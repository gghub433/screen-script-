package app.revizor.update

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import androidx.work.Constraints
import androidx.work.NetworkType
import app.revizor.R
import app.revizor.RevizorApp
import java.util.concurrent.TimeUnit

/** Checks for a new release about twice a day (when on an unmetered network) and posts a notification. */
class UpdateWorker(ctx: Context, p: WorkerParameters) : CoroutineWorker(ctx, p) {
    override suspend fun doWork(): Result {
        val prefs = Prefs(applicationContext)
        if (!prefs.autoUpdateCheck) return Result.success()
        val info = runCatching { Updater.check() }.getOrNull() ?: return Result.success()
        if (prefs.autoInstall && Updater.canInstall(applicationContext)) {
            // Download in the background and hand over to the installer. Android still asks for
            // confirmation unless the app is already the installer of record (Android 12+).
            runCatching { Updater.install(applicationContext, Updater.download(applicationContext, info)) }
            return Result.success()
        }
        val nm = applicationContext.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel("updates", applicationContext.getString(R.string.update_channel), NotificationManager.IMPORTANCE_DEFAULT))
        val open = PendingIntent.getActivity(
            applicationContext, 3, applicationContext.packageManager.getLaunchIntentForPackage(applicationContext.packageName)!!.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE,
        )
        nm.notify(
            31,
            NotificationCompat.Builder(applicationContext, "updates").setSmallIcon(android.R.drawable.stat_sys_download_done)
                .setContentTitle("Revizor ${info.versionName} is available").setContentText("Open the app to update").setContentIntent(open).setAutoCancel(true).build(),
        )
        return Result.success()
    }

    companion object {
        fun schedule(ctx: Context) {
            val req = PeriodicWorkRequestBuilder<UpdateWorker>(12, TimeUnit.HOURS)
                .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.UNMETERED).build()).build()
            WorkManager.getInstance(ctx).enqueueUniquePeriodicWork("revizor-update", ExistingPeriodicWorkPolicy.KEEP, req)
        }
        fun cancel(ctx: Context) = WorkManager.getInstance(ctx).cancelUniqueWork("revizor-update")
    }
}
