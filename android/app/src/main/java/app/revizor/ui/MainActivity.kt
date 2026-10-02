package app.revizor.ui

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.view.WindowCompat
import app.revizor.core.Discovered
import app.revizor.sender.AudioMode
import app.revizor.sender.CaptureService
import app.revizor.update.Prefs

class MainActivity : ComponentActivity() {
    /** Adds the chosen target's details to the service intent once permissions are granted. */
    private var pending: ((Intent) -> Unit)? = null
    lateinit var prefs: Prefs

    private val projectionLauncher = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { r ->
        val addTarget = pending ?: return@registerForActivityResult
        pending = null
        val data = r.data
        if (r.resultCode != RESULT_OK || data == null) {
            Toast.makeText(this, "Screen sharing was not allowed", Toast.LENGTH_SHORT).show()
            return@registerForActivityResult
        }
        val i = Intent(this, CaptureService::class.java).setAction(CaptureService.ACTION_START)
            .putExtra(CaptureService.EXTRA_RESULT_CODE, r.resultCode)
            .putExtra(CaptureService.EXTRA_RESULT_DATA, data)
            .putExtra(CaptureService.EXTRA_MY_NAME, prefs.deviceName)
            .putExtra(CaptureService.EXTRA_PROFILE, prefs.profile).putExtra(CaptureService.EXTRA_AUDIO, prefs.audioMode)
            .putExtra(CaptureService.EXTRA_HEVC, prefs.allowHevc)
        addTarget(i)
        ContextCompat.startForegroundService(this, i)
    }

    private val audioPermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (!granted) {
            Toast.makeText(this, "Sound will not be shared without the audio permission", Toast.LENGTH_LONG).show()
            prefs.audioMode = 0
        }
        requestProjection()
    }

    private val notificationPermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = Prefs(this)
        WindowCompat.setDecorFitsSystemWindows(window, false)
        setContent { RevizorTheme { AppRoot(this) } }
    }

    /** Called by the UI when the user picks a receiver: ask only for what this session needs, then start. */
    fun startSharing(d: Discovered) {
        prefs.lastTarget = "revizor:${d.id}"
        pending = { i -> i.putExtra(CaptureService.EXTRA_KIND, "revizor").putExtra(CaptureService.EXTRA_IP, d.ip).putExtra(CaptureService.EXTRA_PORT, d.port).putExtra(CaptureService.EXTRA_NAME, d.name) }
        askPermissionsThenProject()
    }

    /** A TV with nothing of Revizor installed (Google Cast / DLNA). */
    fun startSharingTv(tv: app.revizor.core.CastTv) {
        prefs.lastTarget = "tv:${tv.ip}"
        pending = { i -> i.putExtra(CaptureService.EXTRA_KIND, "tv").putExtra(CaptureService.EXTRA_IP, tv.ip).putExtra(CaptureService.EXTRA_NAME, tv.name).putExtra(CaptureService.EXTRA_TV_METHODS, tv.methods) }
        askPermissionsThenProject()
    }

    private fun askPermissionsThenProject() {
        if (android.os.Build.VERSION.SDK_INT >= 33 && ActivityCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        val needsAudio = AudioMode.entries.getOrElse(prefs.audioMode) { AudioMode.None } != AudioMode.None
        if (needsAudio && ActivityCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            audioPermission.launch(Manifest.permission.RECORD_AUDIO)
        } else {
            requestProjection()
        }
    }

    private fun requestProjection() {
        val mpm = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        projectionLauncher.launch(mpm.createScreenCaptureIntent())
    }
}
