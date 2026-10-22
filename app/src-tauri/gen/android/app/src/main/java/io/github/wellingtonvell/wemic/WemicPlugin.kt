package io.github.wellingtonvell.wemic

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothManager
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.view.WindowManager
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

@InvokeArg
class KeepScreenOnArgs {
    var on: Boolean = false
}

@InvokeArg
class BluetoothArgs {
    var port: Int = 8125
}

/**
 * Android side of the app, called from Rust (src/platform.rs): runtime
 * permissions, keep-screen-on, the microphone foreground service and the
 * Bluetooth RFCOMM bridge.
 */
@TauriPlugin(
    permissions = [
        Permission(strings = [Manifest.permission.RECORD_AUDIO], alias = "microphone"),
        Permission(strings = [Manifest.permission.BLUETOOTH_CONNECT], alias = "bluetooth"),
        Permission(strings = [Manifest.permission.POST_NOTIFICATIONS], alias = "notifications"),
    ]
)
class WemicPlugin(private val activity: Activity) : Plugin(activity) {
    private var bridge: BluetoothBridge? = null

    @Command
    fun keepScreenOn(invoke: Invoke) {
        val args = invoke.parseArgs(KeepScreenOnArgs::class.java)
        activity.runOnUiThread {
            val flag = WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON
            if (args.on) activity.window.addFlags(flag) else activity.window.clearFlags(flag)
            invoke.resolve()
        }
    }

    @Command
    fun startService(invoke: Invoke) {
        try {
            activity.startForegroundService(Intent(activity, MicService::class.java))
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject("cannot start the microphone service: ${e.message}")
        }
    }

    @Command
    fun stopService(invoke: Invoke) {
        activity.stopService(Intent(activity, MicService::class.java))
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
}
