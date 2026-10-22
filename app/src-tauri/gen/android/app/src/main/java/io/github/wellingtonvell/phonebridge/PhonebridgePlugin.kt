package io.github.wellingtonvell.phonebridge

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothManager
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.view.WindowManager
import androidx.appcompat.app.AppCompatActivity
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class KeepScreenOnArgs {
    /** A microphone client is connected. */
    var mic: Boolean = false
    /** The keep-screen-on setting. */
    var enabled: Boolean = false
}

@InvokeArg
class BluetoothArgs {
    var port: Int = 8125
}

@InvokeArg
class ServiceArgs {
    var camera: Boolean = false
}

@InvokeArg
class CameraArgs {
    var port: Int = 8126
    var localOnly: Boolean = true
    var lens: String = "back"
    var width: Int = 1280
    var height: Int = 720
    var fps: Int = 30
    var orientation: String = "landscape"
}

@InvokeArg
class PreviewRectArgs {
    var x: Int = 0
    var y: Int = 0
    var width: Int = 0
    var height: Int = 0
    var radius: Float = 0f
    var visible: Boolean = false
}

/**
 * Android side of the app, called from Rust (src/platform.rs): runtime
 * permissions, keep-screen-on, the capture foreground service, the
 * Bluetooth RFCOMM bridge, camera sharing and its on-screen preview.
 *
 * Commands run on the main thread (Tauri dispatches them there).
 */
@TauriPlugin(
    permissions = [
        Permission(strings = [Manifest.permission.RECORD_AUDIO], alias = "microphone"),
        Permission(strings = [Manifest.permission.CAMERA], alias = "camera"),
        Permission(strings = [Manifest.permission.BLUETOOTH_CONNECT], alias = "bluetooth"),
        Permission(strings = [Manifest.permission.POST_NOTIFICATIONS], alias = "notifications"),
    ]
)
class PhonebridgePlugin(private val activity: Activity) : Plugin(activity) {
    private var bridge: BluetoothBridge? = null
    private var camera: CameraStreamer? = null
    private val preview = CameraPreview(activity)

    // Keep the screen on while a PC uses the phone (main thread).
    private var keepScreenEnabled = false
    private var micConnected = false
    private var cameraStreaming = false
    private var screenKept = false

    private fun applyKeepScreen() {
        val on = keepScreenEnabled && (micConnected || cameraStreaming)
        if (on == screenKept) return
        screenKept = on
        val flag = WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON
        if (on) activity.window.addFlags(flag) else activity.window.clearFlags(flag)
    }

    private fun setCameraStreaming(on: Boolean) {
        cameraStreaming = on
        applyKeepScreen()
    }

    // Sharing goes on without the activity (foreground service); only the
    // preview goes away with it.
    override fun onDestroy(activity: AppCompatActivity) {
        if (activity === this.activity) preview.destroy()
    }

    @Command
    fun keepScreenOn(invoke: Invoke) {
        val args = invoke.parseArgs(KeepScreenOnArgs::class.java)
        activity.runOnUiThread {
            keepScreenEnabled = args.enabled
            micConnected = args.mic
            applyKeepScreen()
            invoke.resolve()
        }
    }

    @Command
    fun startService(invoke: Invoke) {
        val args = invoke.parseArgs(ServiceArgs::class.java)
        try {
            val intent = Intent(activity, CaptureService::class.java)
                .putExtra(CaptureService.EXTRA_CAMERA, args.camera)
            activity.startForegroundService(intent)
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("cannot start the capture service: ${e.message}")
        }
    }

    @Command
    fun stopService(invoke: Invoke) {
        activity.stopService(Intent(activity, CaptureService::class.java))
        invoke.resolve()
    }

    @Command
    fun startBluetooth(invoke: Invoke) {
        val args = invoke.parseArgs(BluetoothArgs::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
            activity.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            invoke.reject("Bluetooth permission denied")
            return
        }
        val adapter = activity.getSystemService(BluetoothManager::class.java)?.adapter
        if (adapter == null) {
            invoke.reject("this phone has no Bluetooth")
            return
        }
        if (!adapter.isEnabled) {
            invoke.reject("Bluetooth is turned off")
            return
        }
        bridge?.stop()
        try {
            bridge = BluetoothBridge(adapter, args.port).also { it.start() }
            invoke.resolve()
        } catch (e: Exception) {
            bridge = null
            invoke.reject("cannot start Bluetooth: ${e.message}")
        }
    }

    @Command
    fun stopBluetooth(invoke: Invoke) {
        bridge?.stop()
        bridge = null
        invoke.resolve()
    }

    @Command
    fun startCamera(invoke: Invoke) {
        val args = invoke.parseArgs(CameraArgs::class.java)
        if (activity.checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
            invoke.reject("camera permission denied")
            return
        }
        camera?.stop()
        camera = null
        setCameraStreaming(false)
        val config = CameraStreamer.Config(
            port = args.port,
            localOnly = args.localOnly,
            front = args.lens == "front",
            width = args.width,
            height = args.height,
            fps = args.fps,
            portrait = args.orientation == "portrait",
        )
        try {
            var streamer: CameraStreamer? = null
            // From the camera's client threads; a replaced streamer is ignored.
            val onStreaming = { on: Boolean ->
                activity.runOnUiThread { if (camera != null && camera === streamer) setCameraStreaming(on) }
            }
            streamer = CameraStreamer(activity.applicationContext, config, preview, onStreaming)
            camera = streamer
            streamer.start()
            invoke.resolve()
        } catch (e: Exception) {
            camera = null
            invoke.reject("cannot share the camera on port ${args.port}: ${e.message}")
        }
    }

    @Command
    fun stopCamera(invoke: Invoke) {
        camera?.stop()
        camera = null
        setCameraStreaming(false)
        invoke.resolve()
    }

    @Command
    fun cameraStatus(invoke: Invoke) {
        val status = camera?.status() ?: mapOf("state" to "off")
        val ret = JSObject()
        status.forEach { (k, v) -> if (v != null) ret.put(k, v) }
        ret.put(
            "permission",
            activity.checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED,
        )
        invoke.resolve(ret)
    }

    /** Where the page's preview placeholder is, in window pixels. */
    @Command
    fun setPreviewRect(invoke: Invoke) {
        val a = invoke.parseArgs(PreviewRectArgs::class.java)
        preview.setRect(a.x, a.y, a.width, a.height, a.radius, a.visible)
        invoke.resolve()
    }

    /** This app's page in the system settings, to grant a refused permission. */
    @Command
    fun openAppSettings(invoke: Invoke) {
        try {
            activity.startActivity(
                Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", activity.packageName, null))
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            )
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("cannot open the settings: ${e.message}")
        }
    }
}
