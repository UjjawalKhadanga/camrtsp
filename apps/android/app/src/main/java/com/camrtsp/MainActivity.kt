package com.camrtsp

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
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

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val view = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(32, 32, 32, 32) }
        fun field(label: String, default: String): EditText {
            view.addView(TextView(this).apply { text = label })
            return EditText(this).also { it.setText(default); view.addView(it) }
        }
        camera = field("Camera ID (empty = first back camera)", "")
        resolution = field("Resolution", "1280x720")
        fps = field("FPS", "30")
        bitrate = field("Bitrate bps", "3000000")
        port = field("RTSP port", "8554")
        path = field("RTSP path", "/camera")
        username = field("Username (optional)", "")
        password = field("Password (optional)", "")
        val start = Button(this).apply { text = "Start streaming" }
        val stop = Button(this).apply { text = "Stop" }
        status = TextView(this).apply { text = "Idle" }
        view.addView(start); view.addView(stop); view.addView(status)
        setContentView(ScrollView(this).apply { addView(view) })
        start.setOnClickListener { startStreaming() }
        stop.setOnClickListener { stopService(Intent(this, StreamService::class.java)); status.text = "Stopped" }
    }

    private fun startStreaming() {
        val missing = requiredPermissions().filter {
            ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
        }
        if (missing.isNotEmpty()) {
            ActivityCompat.requestPermissions(this, missing.toTypedArray(), CAMERA_REQUEST)
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
        status.text = "Starting rtsp://<LAN-IP>:${port.text}${path.text}"
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        if (requestCode != CAMERA_REQUEST) return
        val granted = results.isNotEmpty() && results.all { it == PackageManager.PERMISSION_GRANTED }
        if (granted) startStreaming()
        else status.text = "Camera and notification permissions are required"
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
