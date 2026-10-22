package io.github.wellingtonvell.phonebridge

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log

/**
 * Foreground service that keeps microphone (and camera) capture allowed
 * while the screen is off or another app is in front. Capture itself runs in
 * Rust (audio) and CameraStreamer (video).
 */
class CaptureService : Service() {
    companion object {
        /** Boolean extra: camera sharing is on. */
        const val EXTRA_CAMERA = "camera"
        private const val CHANNEL = "phonebridge"
        private const val ID = 1
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Called again whenever camera sharing is turned on or off, to update
        // the service types. Android 14 throws if we claim the camera type
        // without the permission.
        val camera = intent?.getBooleanExtra(EXTRA_CAMERA, false) == true &&
            checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, "Microphone", NotificationManager.IMPORTANCE_LOW)
        )
        val open = PendingIntent.getActivity(
            this,
            0,
            packageManager.getLaunchIntentForPackage(packageName),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val text = if (camera) {
            "Microphone and camera available to your PC"
        } else {
            "Microphone available to your PC"
        }
        val notification = Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.ic_btn_speak_now)
            .setContentTitle("Phonebridge")
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val mic = ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            try {
                startForeground(ID, notification, if (camera) mic or ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA else mic)
            } catch (e: SecurityException) {
                // Camera type refused (e.g. started from the background on
                // Android 14+): keep the microphone going at least.
                if (!camera) throw e
                Log.w("phonebridge", "foreground service without the camera type", e)
                startForeground(ID, notification, mic)
            }
        } else {
            startForeground(ID, notification)
        }
        return START_NOT_STICKY
    }
}
