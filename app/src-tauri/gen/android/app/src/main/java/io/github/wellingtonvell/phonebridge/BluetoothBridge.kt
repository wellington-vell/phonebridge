package io.github.wellingtonvell.phonebridge

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothServerSocket
import android.util.Log
import java.io.Closeable
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.net.Socket
import java.util.UUID
import kotlin.concurrent.thread

/**
 * RFCOMM server that relays each Bluetooth client to the Rust server on
 * 127.0.0.1:port, so the protocol code is the same for every transport.
 * The phonebridge Linux client finds the channel through SDP using [SERVICE_UUID].
 */
class BluetoothBridge(private val adapter: BluetoothAdapter, private val port: Int) {
    companion object {
        val SERVICE_UUID: UUID = UUID.fromString("aabe3790-b3b9-400f-8a50-e9b0f38ac6b0")
        private const val TAG = "phonebridge"
    }

    @Volatile private var running = true
    private var server: BluetoothServerSocket? = null
    private val open = mutableSetOf<Closeable>()

    // BLUETOOTH_CONNECT is checked by the caller.
    @SuppressLint("MissingPermission")
    fun start() {
        // Secure (authenticated, encrypted) socket: only paired devices.
        val s = adapter.listenUsingRfcommWithServiceRecord("Phonebridge", SERVICE_UUID)
        server = s
        thread(name = "phonebridge-bt-accept") {
            while (running) {
                val bt = try {
                    s.accept()
                } catch (e: IOException) {
                    break
                }
                Log.i(TAG, "Bluetooth client ${bt.remoteDevice.address}")
                val tcp = try {
                    Socket("127.0.0.1", port).apply { tcpNoDelay = true }
                } catch (e: IOException) {
                    Log.w(TAG, "local server unreachable: $e")
                    bt.close()
                    continue
                }
                val pair = listOf<Closeable>(bt, tcp)
                synchronized(open) { open.addAll(pair) }
                val close: () -> Unit = {
                    pair.forEach { runCatching { it.close() } }
                    synchronized(open) { open.removeAll(pair.toSet()) }
                }
                pump(bt.inputStream, tcp.getOutputStream(), close)
                pump(tcp.getInputStream(), bt.outputStream, close)
            }
        }
    }

    private fun pump(input: InputStream, output: OutputStream, close: () -> Unit) =
        thread(name = "phonebridge-bt-relay") {
            val buf = ByteArray(4096)
            try {
                while (true) {
                    val n = input.read(buf)
                    if (n < 0) break
                    output.write(buf, 0, n)
                    output.flush()
                }
            } catch (_: IOException) {
            } finally {
                close()
            }
        }

    fun stop() {
        running = false
        runCatching { server?.close() }
        synchronized(open) {
            open.forEach { runCatching { it.close() } }
            open.clear()
        }
    }
}
