package io.github.wellingtonvell.phonebridge

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.ImageDecoder
import android.net.Uri
import android.os.Build
import android.provider.MediaStore
import android.provider.Settings
import android.util.Base64
import android.view.WindowManager
import androidx.activity.result.ActivityResult
import androidx.appcompat.app.AppCompatActivity
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import kotlin.concurrent.thread
import kotlin.math.min

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
    /** Pairing code, display form (from getToken). */
    var token: String = ""
    /** Also accept clients without the pairing code (plaintext PBV1). */
    var allowUnauthenticated: Boolean = false
    /**
     * Virtual background sent to the PC: "" off, a built-in one ("studio",
     * "office", "daylight", "fireplace") or "custom" (the photo from
     * pickBackground).
     */
    var background: String = ""
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
            token = SecureChannel.canonicalCode(args.token),
            allowUnauthenticated = args.allowUnauthenticated,
            background = backgroundBytes(args.background),
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

    private val prefs get() = activity.getSharedPreferences("phonebridge", Context.MODE_PRIVATE)

    private fun resolveToken(invoke: Invoke, code: String) {
        invoke.resolve(JSObject().apply { put("token", SecureChannel.displayCode(code)) })
    }

    private fun storeNewToken(): String =
        SecureChannel.newCode().also { prefs.edit().putString("token", it).commit() }

    /** The pairing code (docs/secure-channel.md), created on first use. */
    @Command
    fun getToken(invoke: Invoke) {
        val stored = prefs.getString("token", null)
        resolveToken(invoke, stored?.takeIf { SecureChannel.isValidCode(it) } ?: storeNewToken())
    }

    /** A new pairing code; PCs paired with the old one must re-pair. */
    @Command
    fun resetToken(invoke: Invoke) {
        resolveToken(invoke, storeNewToken())
    }

    /** The user's own virtual background picture, a JPEG the PC puts behind the person. */
    private val backgroundFile get() = File(activity.filesDir, "background.jpg")

    /**
     * Built-in backgrounds (res/raw, made by app/backgrounds/generate.py and,
     * the animated GIF, app/backgrounds/fireplace.py).
     */
    private val builtInBackgrounds = mapOf(
        "studio" to R.raw.bg_studio,
        "office" to R.raw.bg_office,
        "daylight" to R.raw.bg_daylight,
        "fireplace" to R.raw.bg_fireplace,
    )

    /** The picture (JPEG or GIF) for a CameraArgs.background name; null when off or missing. */
    private fun backgroundBytes(name: String): ByteArray? = runCatching {
        when (name) {
            "" -> null
            "custom" -> backgroundFile.readBytes()
            else -> builtInBackgrounds[name]?.let { id ->
                activity.resources.openRawResource(id).use { it.readBytes() }
            }
        }
    }.getOrNull()

    /**
     * Whether the user chose a photo of their own, and a small JPEG data URL
     * of it for the picker.
     */
    @Command
    fun backgroundStatus(invoke: Invoke) {
        thread(name = "phonebridge-background") {
            val ret = JSObject()
            ret.put("chosen", backgroundFile.isFile)
            runCatching { thumbnail() }.getOrNull()?.let { ret.put("thumbnail", it) }
            invoke.resolve(ret)
        }
    }

    private fun thumbnail(): String? {
        if (!backgroundFile.isFile) return null
        val opts = BitmapFactory.Options().apply { inSampleSize = 4 }
        val full = BitmapFactory.decodeFile(backgroundFile.path, opts) ?: return null
        val thumb = Bitmap.createScaledBitmap(full, 320, 320 * full.height / maxOf(1, full.width), true)
        val out = java.io.ByteArrayOutputStream()
        thumb.compress(Bitmap.CompressFormat.JPEG, 80, out)
        if (thumb !== full) thumb.recycle()
        full.recycle()
        return "data:image/jpeg;base64," + Base64.encodeToString(out.toByteArray(), Base64.NO_WRAP)
    }

    /** Let the user choose the virtual background picture; resolves `{ picked }` (false: cancelled). */
    @Command
    fun pickBackground(invoke: Invoke) {
        val intent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            Intent(MediaStore.ACTION_PICK_IMAGES).setType("image/*")
        } else {
            Intent(Intent.ACTION_GET_CONTENT).setType("image/*").addCategory(Intent.CATEGORY_OPENABLE)
        }
        try {
            startActivityForResult(invoke, intent, "backgroundPicked")
        } catch (e: Exception) {
            invoke.reject("cannot open the picture picker: ${e.message}")
        }
    }

    @ActivityCallback
    fun backgroundPicked(invoke: Invoke, result: ActivityResult) {
        val uri = result.data?.data
        if (result.resultCode != Activity.RESULT_OK || uri == null) {
            invoke.resolve(JSObject().apply { put("picked", false) })
            return
        }
        thread(name = "phonebridge-background") {
            try {
                saveBackground(uri)
                invoke.resolve(JSObject().apply { put("picked", true) })
            } catch (e: Exception) {
                invoke.reject("cannot use this picture: ${e.message ?: e.javaClass.simpleName}")
            }
        }
    }

    /**
     * Decode [uri] upright (ImageDecoder applies the EXIF orientation), fit it
     * within 1920x1080 and store it as a JPEG the PC accepts (well under 4 MiB).
     */
    private fun saveBackground(uri: Uri) {
        val source = ImageDecoder.createSource(activity.contentResolver, uri)
        val bitmap = ImageDecoder.decodeBitmap(source) { decoder, info, _ ->
            val (w, h) = info.size.width to info.size.height
            val scale = min(1.0, min(1920.0 / maxOf(w, h), 1080.0 / minOf(w, h)))
            decoder.setTargetSize(maxOf(1, (w * scale).toInt()), maxOf(1, (h * scale).toInt()))
            decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
        }
        val tmp = File(activity.filesDir, "background.jpg.tmp")
        tmp.outputStream().use { bitmap.compress(Bitmap.CompressFormat.JPEG, 85, it) }
        bitmap.recycle()
        if (!tmp.renameTo(backgroundFile)) throw java.io.IOException("cannot store the picture")
    }

    @Command
    fun clearBackground(invoke: Invoke) {
        backgroundFile.delete()
        invoke.resolve()
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
