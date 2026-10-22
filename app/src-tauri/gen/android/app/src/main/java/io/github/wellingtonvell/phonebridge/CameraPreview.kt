package io.github.wellingtonvell.phonebridge

import android.app.Activity
import android.graphics.Matrix
import android.graphics.Outline
import android.graphics.SurfaceTexture
import android.hardware.display.DisplayManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Size
import android.view.Gravity
import android.view.Surface
import android.view.TextureView
import android.view.View
import android.view.ViewGroup
import android.view.ViewOutlineProvider
import android.widget.FrameLayout
import kotlin.math.min

/**
 * Live camera preview on the phone: a TextureView laid over the WebView at
 * the rect of the page's preview placeholder (reported by ui/main.js through
 * setPreviewRect). CameraStreamer adds its Surface as a second Camera2
 * output next to the encoder, so the preview shows exactly what the PC gets.
 *
 * The camera service already rotates (and, for the front camera, mirrors)
 * preview buffers upright for the display's natural orientation; this class
 * only undoes the view's stretch, turns the image for the current display
 * rotation and letterboxes it.
 *
 * Methods are for the main thread unless they say otherwise.
 */
class CameraPreview(private val activity: Activity) : TextureView.SurfaceTextureListener {
    /** The camera side, i.e. the running CameraStreamer. */
    interface Consumer {
        /** A new surface to draw the preview into. Main thread. */
        fun onPreviewSurface(surface: Surface)

        /**
         * The surface is going away. Call [done] (any thread) once the camera
         * no longer draws into it; only then is it released.
         */
        fun onPreviewSurfaceLost(done: () -> Unit)
    }

    private val main = Handler(Looper.getMainLooper())
    private var view: TextureView? = null
    private var texture: SurfaceTexture? = null

    /** Current preview surface, read by the camera thread when it opens. */
    @Volatile var surface: Surface? = null
        private set

    var consumer: Consumer? = null
        set(value) {
            field = value
            surface?.let { value?.onPreviewSurface(it) }
            if (value == null) setLive(false)
        }

    // Placeholder rect from the page, in window pixels.
    private var rect = IntArray(4)
    private var radius = 0f
    private var wanted = false

    // What the camera renders, set from the camera thread.
    @Volatile private var bufferSize: Size? = null
    @Volatile private var sensorOrientation = 90

    // Shown only once the camera draws fresh frames, so a stale frame from
    // the previous session never flashes up.
    private var live = false
    private var awaitingFrame = false

    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) {}
        override fun onDisplayRemoved(displayId: Int) {}
        override fun onDisplayChanged(displayId: Int) = updateTransform()
    }

    init {
        activity.getSystemService(DisplayManager::class.java)
            ?.registerDisplayListener(displayListener, main)
    }

    /** Position (window px, relative to the WebView) and whether to show it. */
    fun setRect(x: Int, y: Int, width: Int, height: Int, cornerRadius: Float, visible: Boolean) {
        rect = intArrayOf(x, y, maxOf(width, 1), maxOf(height, 1))
        radius = cornerRadius
        wanted = visible && width > 0 && height > 0
        val v = ensureView() ?: return
        val lp = (v.layoutParams as FrameLayout.LayoutParams)
        if (lp.leftMargin != rect[0] || lp.topMargin != rect[1] || lp.width != rect[2] || lp.height != rect[3]) {
            lp.leftMargin = rect[0]
            lp.topMargin = rect[1]
            lp.width = rect[2]
            lp.height = rect[3]
            v.layoutParams = lp
        }
        v.invalidateOutline()
        updateVisibility()
    }

    /** The camera is about to render [size] buffers into the surface. Any thread. */
    fun setContent(size: Size, sensor: Int) {
        bufferSize = size
        sensorOrientation = sensor
        texture?.setDefaultBufferSize(size.width, size.height)
        main.post { updateTransform() }
    }

    /** Whether the camera currently feeds the preview. Any thread. */
    fun setLive(on: Boolean) {
        main.post {
            if (on == live) return@post
            live = on
            awaitingFrame = on
            updateVisibility()
            // In case frame updates are not reported while transparent.
            if (on) main.postDelayed({
                if (live && awaitingFrame) {
                    awaitingFrame = false
                    updateVisibility()
                }
            }, 500)
        }
    }

    fun destroy() {
        activity.getSystemService(DisplayManager::class.java)?.unregisterDisplayListener(displayListener)
        view?.let { (it.parent as? ViewGroup)?.removeView(it) }
    }

    /** The view, (re)attached above the WebView in the content frame. */
    private fun ensureView(): TextureView? {
        val content = activity.findViewById<FrameLayout>(android.R.id.content) ?: return null
        val v = view ?: TextureView(activity).also { v ->
            v.surfaceTextureListener = this
            v.isOpaque = false
            // Transparent, not invisible: an invisible TextureView never
            // creates its surface, and the camera needs it before showing.
            v.alpha = 0f
            v.importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
            v.outlineProvider = object : ViewOutlineProvider() {
                override fun getOutline(view: View, outline: Outline) =
                    outline.setRoundRect(0, 0, view.width, view.height, radius)
            }
            v.clipToOutline = true
            v.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> updateTransform() }
            view = v
        }
        if (v.parent !== content) {
            (v.parent as? ViewGroup)?.removeView(v)
            val lp = FrameLayout.LayoutParams(rect[2], rect[3], Gravity.TOP or Gravity.START)
            lp.leftMargin = rect[0]
            lp.topMargin = rect[1]
            content.addView(v, lp) // last child: drawn above the WebView
        }
        return v
    }

    private fun updateVisibility() {
        view?.alpha = if (wanted && live && !awaitingFrame) 1f else 0f
    }

    private fun displayRotation(): Int {
        val r = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            activity.display?.rotation ?: Surface.ROTATION_0
        } else {
            @Suppress("DEPRECATION")
            activity.windowManager.defaultDisplay.rotation
        }
        return when (r) {
            Surface.ROTATION_90 -> 90
            Surface.ROTATION_180 -> 180
            Surface.ROTATION_270 -> 270
            else -> 0
        }
    }

    /** Upright for the current display rotation, aspect kept, letterboxed. */
    private fun updateTransform() {
        val v = view ?: return
        val size = bufferSize ?: return
        val w = v.width.toFloat()
        val h = v.height.toFloat()
        if (w <= 0f || h <= 0f) return
        // Buffer as the camera service hands it over: upright for the
        // display's natural orientation.
        val sideways = sensorOrientation % 180 != 0
        val cw = (if (sideways) size.height else size.width).toFloat()
        val ch = (if (sideways) size.width else size.height).toFloat()
        val rotation = displayRotation()
        val (rw, rh) = if (rotation % 180 != 0) ch to cw else cw to ch
        val cx = w / 2
        val cy = h / 2
        val scale = min(w / rw, h / rh)
        v.setTransform(
            Matrix().apply {
                setScale(cw / w, ch / h, cx, cy) // undo the stretch to the view
                postRotate(-rotation.toFloat(), cx, cy)
                postScale(scale, scale, cx, cy)
            }
        )
    }

    override fun onSurfaceTextureAvailable(st: SurfaceTexture, width: Int, height: Int) {
        texture = st
        bufferSize?.let { st.setDefaultBufferSize(it.width, it.height) }
        val s = Surface(st)
        surface = s
        updateTransform()
        consumer?.onPreviewSurface(s)
    }

    override fun onSurfaceTextureSizeChanged(st: SurfaceTexture, width: Int, height: Int) {
        // TextureView resets the buffer size to the view size; keep the camera's.
        bufferSize?.let { st.setDefaultBufferSize(it.width, it.height) }
        updateTransform()
    }

    override fun onSurfaceTextureDestroyed(st: SurfaceTexture): Boolean {
        val s = surface
        texture = null
        surface = null
        live = false
        awaitingFrame = false
        val release = {
            main.post {
                s?.release()
                st.release()
            }
            Unit
        }
        val c = consumer
        if (c != null) c.onPreviewSurfaceLost(release) else release()
        // Released by us once the camera let go of it.
        return false
    }

    override fun onSurfaceTextureUpdated(st: SurfaceTexture) {
        if (awaitingFrame) {
            awaitingFrame = false
            updateVisibility()
        }
    }
}
