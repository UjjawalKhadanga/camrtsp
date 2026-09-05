package com.camrtsp

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.text.InputType
import android.view.View
import android.widget.*
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat

class MainActivity : AppCompatActivity() {
    private lateinit var camera: EditText
    private lateinit var resolution: EditText
    private lateinit var fps: EditText
    private lateinit var bitrate: EditText
    private lateinit var port: EditText
    private lateinit var path: EditText
    private lateinit var username: EditText
    private lateinit var password: EditText
    private lateinit var status: TextView

    private val ink = Color.rgb(16, 19, 16)
    private val panel = Color.rgb(26, 32, 23)
    private val mint = Color.rgb(183, 255, 91)
    private val paper = Color.rgb(241, 243, 233)
    private val muted = Color.rgb(163, 173, 157)
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var start: Button
    private lateinit var stop: Button
    private val refresh = object : Runnable {
        override fun run() {
            status.text = StreamService.statusMessage
            start.isEnabled = !StreamService.isStreaming
            start.alpha = if (start.isEnabled) 1f else 0.5f
            stop.isEnabled = StreamService.isStreaming
            stop.alpha = if (stop.isEnabled) 1f else 0.5f
            handler.postDelayed(this, 500)
        }
    }
    private fun dp(value: Int) = (value * resources.displayMetrics.density).toInt()
    private fun surface(color: Int, stroke: Int = Color.rgb(52, 63, 45)) = GradientDrawable().apply {
        setColor(color); cornerRadius = dp(14).toFloat(); setStroke(dp(1), stroke)
    }

    override fun onResume() { super.onResume(); handler.post(refresh) }
    override fun onPause() { handler.removeCallbacks(refresh); super.onPause() }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.statusBarColor = ink
        window.navigationBarColor = ink
        val view = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL; setPadding(dp(24), dp(32), dp(24), dp(32)); setBackgroundColor(ink)
        }
        fun text(parent: LinearLayout, value: String, size: Float, color: Int = paper): TextView {
            return TextView(this).also {
                it.text = value; it.textSize = size; it.setTextColor(color)
                it.setPadding(0, dp(6), 0, dp(10)); parent.addView(it)
            }
        }
        fun card(): LinearLayout = LinearLayout(this).also {
            it.orientation = LinearLayout.VERTICAL; it.setPadding(dp(20), dp(18), dp(20), dp(18))
            it.background = surface(panel)
            view.addView(it, LinearLayout.LayoutParams(-1, -2).apply { topMargin = dp(18) })
        }
        fun field(parent: LinearLayout, label: String, default: String, type: Int = InputType.TYPE_CLASS_TEXT): EditText {
            val caption = text(parent, label, 12f, muted)
            return EditText(this).also {
                it.id = View.generateViewId(); caption.labelFor = it.id
                it.inputType = type; it.setSingleLine(true); it.setText(default); it.textSize = 15f
                it.setTextColor(paper); it.setHintTextColor(muted)
                it.background = surface(ink); it.setPadding(dp(12), dp(12), dp(12), dp(12))
                parent.addView(it, LinearLayout.LayoutParams(-1, -2).apply { bottomMargin = dp(14) })
            }
        }
        val heading = LinearLayout(this).apply { gravity = android.view.Gravity.CENTER_VERTICAL }
        heading.addView(ImageView(this).apply { setImageResource(com.camrtsp.R.drawable.ic_brand); contentDescription = null }, LinearLayout.LayoutParams(dp(42), dp(42)).apply { rightMargin = dp(12) })
        text(heading, "camrtsp", 28f).typeface = Typeface.DEFAULT_BOLD
        view.addView(heading)
        text(view, "YOUR CAMERA. YOUR NETWORK.", 10f, mint).letterSpacing = 0.15f
        text(view, "Open the signal.", 34f).typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
        text(view, "A native camera source for your network.", 14f, muted)
        val live = card()
        text(live, "SIGNAL STATUS", 10f, mint).letterSpacing = 0.15f
        status = text(live, StreamService.statusMessage, 16f)
        status.setTextIsSelectable(true)
        text(live, "Open your phone’s LAN address in VLC, OBS, or another RTSP viewer on the same network.", 12f, muted)
        start = Button(this).apply {
            text = "Start streaming  ↗"; isAllCaps = false; setTextColor(ink); background = surface(mint, mint)
        }
        stop = Button(this).apply {
            text = "Stop streaming"; isAllCaps = false; setTextColor(paper); background = surface(panel)
        }
        live.addView(start, LinearLayout.LayoutParams(-1, dp(52)).apply { topMargin = dp(10) })
        live.addView(stop, LinearLayout.LayoutParams(-1, dp(48)).apply { topMargin = dp(10) })
        val capture = card()
        text(capture, "01 / CAMERA", 11f, mint).letterSpacing = 0.1f
        camera = field(capture, "Camera ID · leave empty for first back camera", "")
        camera.hint = "Automatic"
        resolution = field(capture, "Resolution · width x height", "1280x720")
        fps = field(capture, "Frames per second", "30", InputType.TYPE_CLASS_NUMBER)
        val advancedToggle = Button(this).apply {
            text = "Network & encoding settings  +"; isAllCaps = false; setTextColor(mint); background = surface(ink)
        }
        view.addView(advancedToggle, LinearLayout.LayoutParams(-1, dp(52)).apply { topMargin = dp(18) })
        val advanced = card()
        text(advanced, "02 / NETWORK & ENCODING", 11f, mint)
        bitrate = field(advanced, "Bitrate · bits per second", "3000000", InputType.TYPE_CLASS_NUMBER)
        port = field(advanced, "RTSP port", "8554", InputType.TYPE_CLASS_NUMBER)
        path = field(advanced, "Stream path", "/camera")
        username = field(advanced, "Username · optional", "")
        password = field(advanced, "Password · optional", "", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD)
        advanced.visibility = View.GONE
        advancedToggle.setOnClickListener {
            val opening = advanced.visibility == View.GONE
            advanced.visibility = if (opening) View.VISIBLE else View.GONE
            advancedToggle.text = if (opening) "Network & encoding settings  −" else "Network & encoding settings  +"
        }
        text(view, "H.264  /  RTSP  /  NATIVE RUST CORE", 10f, muted).apply {
            gravity = android.view.Gravity.CENTER; setPadding(0, dp(28), 0, 0)
        }
        setContentView(ScrollView(this).apply { isFillViewport = true; setBackgroundColor(ink); addView(view) })
        start.setOnClickListener { startStreaming() }
        stop.setOnClickListener { stopService(Intent(this, StreamService::class.java)) }
    }

    private fun startStreaming() {
        val missing = requiredPermissions().filter {
            ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
        }
        if (missing.isNotEmpty()) {
            ActivityCompat.requestPermissions(this, missing.toTypedArray(), CAMERA_REQUEST)
            return
        }
        val dimensions = resolution.text.toString().split('x')
        val validDimensions = dimensions.size == 2 && dimensions.all { (it.toIntOrNull() ?: 0) > 0 }
        val validAuth = username.text.isNullOrEmpty() == password.text.isNullOrEmpty()
        if (!validDimensions || (fps.text.toString().toIntOrNull() ?: 0) !in 1..240 ||
            (bitrate.text.toString().toIntOrNull() ?: 0) <= 0 ||
            (port.text.toString().toIntOrNull() ?: 0) !in 1..65535 ||
            !path.text.toString().startsWith("/") || !validAuth) {
            StreamService.statusMessage = "Check resolution, FPS (1–240), bitrate, port (1–65535), and path (/). Supply both auth fields or neither."
            status.text = StreamService.statusMessage
            return
        }
        val intent = Intent(this, StreamService::class.java).apply {
            putExtra(StreamService.CAMERA, camera.text.toString())
            putExtra(StreamService.RESOLUTION, resolution.text.toString())
            putExtra(StreamService.FPS, fps.text.toString().toIntOrNull() ?: 30)
            putExtra(StreamService.BITRATE, bitrate.text.toString().toIntOrNull() ?: 3_000_000)
            putExtra(StreamService.PORT, port.text.toString().toIntOrNull() ?: 8554)
            putExtra(StreamService.PATH, path.text.toString())
            putExtra(StreamService.USERNAME, username.text.toString())
            putExtra(StreamService.PASSWORD, password.text.toString())
        }
        ContextCompat.startForegroundService(this, intent)
        StreamService.statusMessage = "Starting rtsp://<LAN-IP>:${port.text}${path.text}"
        status.text = StreamService.statusMessage
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        if (requestCode != CAMERA_REQUEST) return
        val granted = results.isNotEmpty() && results.all { it == PackageManager.PERMISSION_GRANTED }
        if (granted) startStreaming()
        else { StreamService.statusMessage = "Camera and notification permissions are required"; status.text = StreamService.statusMessage }
    }

    private fun requiredPermissions(): List<String> {
        val permissions = mutableListOf(Manifest.permission.CAMERA)
        if (Build.VERSION.SDK_INT >= 33) {
            permissions.add(Manifest.permission.POST_NOTIFICATIONS)
        }
        return permissions
    }

    private companion object { const val CAMERA_REQUEST = 41 }
}
