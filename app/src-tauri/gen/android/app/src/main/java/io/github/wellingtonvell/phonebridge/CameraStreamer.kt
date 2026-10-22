package io.github.wellingtonvell.phonebridge

import android.annotation.SuppressLint
import android.content.Context
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.params.OutputConfiguration
import android.hardware.camera2.params.SessionConfiguration
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.util.Log
import android.util.Range
import android.util.Size
import android.view.Surface
import java.io.BufferedOutputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executor
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread
import kotlin.math.abs

/**
 * Shares a phone camera as an H.264 stream on its own TCP port (control
 * port + 1), next to the Rust audio server. Camera2 renders straight into
 * the encoder's input Surface, so frames never touch the CPU.
 *
 *   PC -> "PBV1"
 *   phone -> "PBV1" status u8 | codec u8 | width u16 | height u16 | rotation u16 | fps u8
 *            (status != 0: then [msg_len u16][UTF-8 message] and close)
 *   phone -> repeated [len u32][pts_us u64][flags u8][H.264 Annex-B, len bytes]
 *            flags: bit0 keyframe, bit1 codec config (SPS/PPS)
 *
 * All big-endian. One PC at a time; a new one replaces the old. The camera
 * is open only while a PC is connected, for battery and privacy.
 */
class CameraStreamer(private val context: Context, private val config: Config) {
    data class Config(
        val port: Int,
        /** USB: only reachable through `adb forward`. */
        val localOnly: Boolean,
        val front: Boolean,
        val width: Int,
        val height: Int,
        val fps: Int,
        val portrait: Boolean,
    )

    companion object {
        private const val TAG = "phonebridge"
        private val MAGIC = "PBV1".toByteArray()
        private const val STATUS_OK = 0
        private const val STATUS_UNAVAILABLE = 2
        private const val CODEC_H264 = 1
        private const val FLAG_KEY = 1
        private const val FLAG_CONFIG = 2
        /** ~2 s of frames; past that the link can't keep up, so skip to a keyframe. */
        private const val MAX_QUEUED = 60
        /** A write stuck this long means the PC is gone without closing. */
        private const val WRITE_TIMEOUT_MS = 5000L
        private val START_CODE = byteArrayOf(0, 0, 0, 1)
    }

    private class Frame(val data: ByteArray, val ptsUs: Long, val flags: Int)

    /** A connected PC, fed by the encoder through a queue so a slow link never stalls it. */
    private inner class Client(val sock: Socket) {
        val peer: String = sock.inetAddress.hostAddress ?: "?"
        val queue = LinkedBlockingQueue<Frame>()
        @Volatile var attached = false
        @Volatile var writingSince = 0L
        var sentConfig = false
        var waitKeyframe = true

        fun close() = runCatching { sock.close() }
    }

    private val lock = Object()
    private val openLock = Object()
    @Volatile private var running = false
    private var server: ServerSocket? = null
    private var client: Client? = null
    private var session: Session? = null
    @Volatile private var lastError: String? = null

    fun start() {
        val addr = if (config.localOnly) InetAddress.getByName("127.0.0.1") else null
        val s = ServerSocket()
        s.reuseAddress = true
        s.bind(InetSocketAddress(addr, config.port))
        server = s
        running = true
        thread(name = "phonebridge-cam-accept") {
            while (running) {
                val sock = try {
                    s.accept()
                } catch (e: IOException) {
                    break
                }
                thread(name = "phonebridge-cam-client") { serve(sock) }
            }
        }
    }

    fun stop() {
        running = false
        runCatching { server?.close() }
        val s = synchronized(lock) {
            client?.close()
            client = null
            session.also { session = null }
        }
        s?.close()
    }

    /** "off" | "listening" | "streaming", for the UI. */
    fun status(): Map<String, Any?> = synchronized(lock) {
        val c = client
        val s = session
        if (c != null && c.attached && s != null) {
            mapOf(
                "state" to "streaming",
                "peer" to c.peer,
                "width" to s.size.width,
                "height" to s.size.height,
            )
        } else {
            mapOf("state" to if (running) "listening" else "off", "error" to lastError)
        }
    }

    private fun serve(sock: Socket) {
        val c = Client(sock)
        try {
            sock.tcpNoDelay = true
            sock.soTimeout = 5000
            val hello = ByteArray(4)
            DataInputStream(sock.getInputStream()).readFully(hello)
            if (!hello.contentEquals(MAGIC)) {
                c.close()
                return
            }
            Log.i(TAG, "camera client ${c.peer}")
            val out = DataOutputStream(BufferedOutputStream(sock.getOutputStream(), 64 * 1024))
            val old = synchronized(lock) {
                if (!running) return
                client.also { client = c }
            }
            old?.close()
            // Opening takes a moment; do it outside `lock` so status() stays quick.
            val s = try {
                synchronized(openLock) {
                    synchronized(lock) { session } ?: Session().also { new ->
                        new.open()
                        synchronized(lock) {
                            if (!running || client !== c) {
                                new.close()
                                return
                            }
                            session = new
                        }
                    }
                }
            } catch (e: Exception) {
                val msg = e.message ?: e.javaClass.simpleName
                lastError = "camera: $msg"
                Log.w(TAG, "camera unavailable", e)
                sendError(out, STATUS_UNAVAILABLE, msg)
                return
            }
            lastError = null
            out.write(MAGIC)
            out.writeByte(STATUS_OK)
            out.writeByte(CODEC_H264)
            out.writeShort(s.size.width)
            out.writeShort(s.size.height)
            out.writeShort(s.rotation)
            out.writeByte(s.fps)
            out.flush()
            c.attached = true
            s.requestKeyframe()
            while (!sock.isClosed) {
                val f = c.queue.poll(1, TimeUnit.SECONDS) ?: continue
                c.writingSince = SystemClock.elapsedRealtime()
                out.writeInt(f.data.size)
                out.writeLong(f.ptsUs)
                out.writeByte(f.flags)
                out.write(f.data)
                if (c.queue.isEmpty()) out.flush()
                c.writingSince = 0
            }
        } catch (_: IOException) {
        } catch (_: InterruptedException) {
        } finally {
            c.close()
            drop(c)
        }
    }

    private fun sendError(out: DataOutputStream, status: Int, msg: String) {
        runCatching {
            val bytes = msg.toByteArray(Charsets.UTF_8)
            out.write(MAGIC)
            out.writeByte(status)
            out.write(ByteArray(8))
            out.writeShort(bytes.size)
            out.write(bytes)
            out.flush()
        }
    }

    /** Close the camera once its PC is gone (unless another one took over). */
    private fun drop(c: Client): Unit = synchronized(openLock) {
        val s = synchronized(lock) {
            if (client !== c) return
            Log.i(TAG, "camera client ${c.peer} gone")
            client = null
            session.also { session = null }
        }
        // Outside `lock`, as closing the camera takes a while and status()
        // runs on the main thread; under `openLock`, so the next PC waits
        // for the camera to be free.
        s?.close()
    }

    /** Called on the encoder thread for every output buffer. */
    private fun deliver(s: Session, f: Frame) {
        val c = synchronized(lock) { client } ?: return
        if (!c.attached) return
        val since = c.writingSince
        if (since != 0L && SystemClock.elapsedRealtime() - since > WRITE_TIMEOUT_MS) {
            Log.w(TAG, "camera client ${c.peer} stalled")
            c.close()
            return
        }
        if (!c.sentConfig) {
            val cfg = s.codecConfig ?: return
            c.queue.add(cfg)
            c.sentConfig = true
        }
        if (f.flags and FLAG_CONFIG != 0) return // cached and sent above
        if (c.queue.size > MAX_QUEUED) {
            c.queue.clear()
            c.sentConfig = false
            c.waitKeyframe = true
            s.requestKeyframe()
            return
        }
        if (c.waitKeyframe) {
            if (f.flags and FLAG_KEY == 0) return
            c.waitKeyframe = false
        }
        c.queue.add(f)
    }

    /** The open camera feeding the encoder. */
    private inner class Session {
        private val manager = context.getSystemService(CameraManager::class.java)
        private val cameraId: String
        val size: Size
        val fps: Int
        private val fpsRange: Range<Int>
        val rotation: Int
        private var codec: MediaCodec? = null
        private var surface: Surface? = null
        private var camera: CameraDevice? = null
        private var capture: CameraCaptureSession? = null
        @Volatile var codecConfig: Frame? = null
        @Volatile private var closed = false

        init {
            val facing = if (config.front) {
                CameraCharacteristics.LENS_FACING_FRONT
            } else {
                CameraCharacteristics.LENS_FACING_BACK
            }
            val ids = manager.cameraIdList
            cameraId = ids.firstOrNull {
                manager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == facing
            } ?: ids.firstOrNull() ?: throw IOException("this phone has no camera")
            val chars = manager.getCameraCharacteristics(cameraId)
            val map = chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
            size = pickSize(map?.getOutputSizes(MediaCodec::class.java) ?: emptyArray())
            fpsRange = pickFps(chars.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES))
            fps = fpsRange.upper
            // Same math as JPEG_ORIENTATION, with "landscape" being the usual
            // one (phone turned counter-clockwise, device orientation 270).
            val sensor = chars.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 90
            val device = if (config.portrait) 0 else 270
            val front = chars.get(CameraCharacteristics.LENS_FACING) ==
                CameraCharacteristics.LENS_FACING_FRONT
            rotation = if (front) (sensor - device + 360) % 360 else (sensor + device) % 360
        }

        // Declared after init so a failed camera lookup leaves no thread behind.
        private val handlerThread = HandlerThread("phonebridge-camera").apply { start() }
        private val handler = Handler(handlerThread.looper)
        private val executor = Executor { handler.post(it) }

        private fun pickSize(sizes: Array<Size>): Size {
            if (sizes.isEmpty()) return Size(config.width, config.height)
            val aspect = config.width.toDouble() / config.height
            val sameAspect = sizes.filter { abs(it.width.toDouble() / it.height - aspect) < 0.05 }
            val target = config.width * config.height
            return (sameAspect.ifEmpty { sizes.toList() })
                .minBy { abs(it.width * it.height - target) }
        }

        /** Prefer a fixed [fps, fps] range for even frame pacing. */
        private fun pickFps(ranges: Array<Range<Int>>?): Range<Int> {
            if (ranges.isNullOrEmpty()) return Range(config.fps, config.fps)
            return ranges.minBy { abs(it.upper - config.fps) * 1000 + (it.upper - it.lower) }
        }

        private fun bitrate() = when {
            size.height <= 480 -> 2_000_000
            size.height <= 720 -> 4_000_000
            else -> 8_000_000
        }

        /** [tuned] adds latency hints, which some encoders reject outright. */
        private fun format(tuned: Boolean) =
            MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, size.width, size.height).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, bitrate())
                setInteger(MediaFormat.KEY_FRAME_RATE, fps)
                // Short GOP: a PC that joins or drops packets recovers quickly.
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
                if (!tuned) return@apply
                setInteger(MediaFormat.KEY_PRIORITY, 0) // realtime
                // Baseline: no B-frames, so frames leave in display order.
                setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                    setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
                    setInteger(MediaFormat.KEY_PREPEND_HEADER_TO_SYNC_FRAMES, 1)
                }
                // Not KEY_LOW_LATENCY: it's a decoder feature, and encoders
                // such as OMX.sprd.h264.encoder fail configure() with it.
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    setInteger(MediaFormat.KEY_LATENCY, 1)
                }
            }

        private fun createCodec(): MediaCodec {
            val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
            codec.setCallback(object : MediaCodec.Callback() {
                override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {}

                override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                    if (closed) return
                    try {
                        val buf = codec.getOutputBuffer(index)
                        if (buf != null && info.size > 0) {
                            buf.position(info.offset)
                            buf.limit(info.offset + info.size)
                            val data = ByteArray(info.size).also { buf.get(it) }
                            var flags = 0
                            if (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0) flags = flags or FLAG_KEY
                            if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                                flags = flags or FLAG_CONFIG
                                codecConfig = Frame(annexB(data), info.presentationTimeUs, FLAG_CONFIG)
                            }
                            codec.releaseOutputBuffer(index, false)
                            deliver(this@Session, Frame(data, info.presentationTimeUs, flags))
                        } else {
                            codec.releaseOutputBuffer(index, false)
                        }
                    } catch (e: IllegalStateException) {
                        // Released under us while closing.
                    }
                }

                override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
                    // Some encoders only report SPS/PPS here, without a config buffer.
                    if (codecConfig != null) return
                    val sps = format.getByteBuffer("csd-0") ?: return
                    val pps = format.getByteBuffer("csd-1")
                    val bytes = listOfNotNull(sps, pps).map { b ->
                        annexB(ByteArray(b.remaining()).also { b.duplicate().get(it) })
                    }
                    codecConfig = Frame(bytes.reduce { a, b -> a + b }, 0, FLAG_CONFIG)
                }

                override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                    fatal("encoder: ${e.message}")
                }
            }, handler)
            return codec
        }

        /** Open camera and encoder; blocks until frames are flowing or it failed. */
        fun open() =
            try {
                openOrThrow()
            } catch (e: Exception) {
                close()
                throw e
            }

        @SuppressLint("MissingPermission")
        private fun openOrThrow() {
            var enc = createCodec()
            codec = enc
            try {
                enc.configure(format(true), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            } catch (e: Exception) {
                Log.w(TAG, "encoder rejected the tuned format, using a plain one", e)
                enc.release()
                enc = createCodec()
                codec = enc
                try {
                    enc.configure(format(false), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
                } catch (e: Exception) {
                    throw IOException("the H.264 encoder rejected ${size.width}x${size.height}@$fps", e)
                }
            }
            val input = enc.createInputSurface()
            surface = input
            enc.start()

            val ready = CountDownLatch(1)
            var failure: String? = null
            val fail = { msg: String ->
                if (ready.count > 0) {
                    failure = msg
                    ready.countDown()
                } else {
                    fatal(msg)
                }
            }
            manager.openCamera(cameraId, executor, object : CameraDevice.StateCallback() {
                override fun onOpened(device: CameraDevice) {
                    if (closed) {
                        device.close()
                        return
                    }
                    camera = device
                    val outputs = listOf(OutputConfiguration(input))
                    val cb = object : CameraCaptureSession.StateCallback() {
                        override fun onConfigured(s: CameraCaptureSession) {
                            capture = s
                            try {
                                val req = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                                    addTarget(input)
                                    set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, fpsRange)
                                }
                                s.setRepeatingRequest(req.build(), null, handler)
                                ready.countDown()
                            } catch (e: Exception) {
                                fail("cannot start capture: ${e.message}")
                            }
                        }

                        override fun onConfigureFailed(s: CameraCaptureSession) =
                            fail("camera rejected the ${size.width}x${size.height} stream")
                    }
                    try {
                        device.createCaptureSession(
                            SessionConfiguration(SessionConfiguration.SESSION_REGULAR, outputs, executor, cb)
                        )
                    } catch (e: Exception) {
                        fail("cannot configure camera: ${e.message}")
                    }
                }

                override fun onDisconnected(device: CameraDevice) {
                    device.close()
                    fail("camera taken by another app")
                }

                override fun onError(device: CameraDevice, error: Int) {
                    device.close()
                    fail(
                        when (error) {
                            ERROR_CAMERA_IN_USE, ERROR_MAX_CAMERAS_IN_USE ->
                                "camera in use by another app"
                            ERROR_CAMERA_DISABLED -> "camera disabled by policy"
                            else -> "camera error $error"
                        }
                    )
                }
            })
            if (!ready.await(5, TimeUnit.SECONDS)) failure = failure ?: "camera did not start"
            failure?.let {
                close()
                throw IOException(it)
            }
            Log.i(TAG, "camera $cameraId ${size.width}x${size.height}@$fps rotation $rotation")
        }

        fun requestKeyframe() {
            runCatching {
                codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
            }
        }

        /** The camera died while streaming: tell the UI and drop the PC. */
        private fun fatal(msg: String) {
            Log.w(TAG, msg)
            lastError = msg
            synchronized(lock) {
                if (session === this) client?.close()
            }
        }

        fun close() {
            if (closed) return
            closed = true
            runCatching { capture?.close() }
            runCatching { camera?.close() }
            runCatching { codec?.stop() }
            runCatching { codec?.release() }
            runCatching { surface?.release() }
            handlerThread.quitSafely()
        }
    }

    /** MediaCodec's AVC config is already Annex-B; make sure of it anyway. */
    private fun annexB(data: ByteArray): ByteArray {
        val hasStart = data.size >= 4 && data[0].toInt() == 0 && data[1].toInt() == 0 &&
            (data[2].toInt() == 1 || (data[2].toInt() == 0 && data[3].toInt() == 1))
        return if (hasStart) data else START_CODE + data
    }
}
