package com.camrtsp

import android.app.*
import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.IBinder
import android.os.PowerManager
import android.util.Size
import androidx.core.app.NotificationCompat

class StreamService : Service() {
    private var handle = 0L
    private var encoder: CameraEncoder? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == STOP) { stopStreaming(); stopSelf(); return START_NOT_STICKY }
        if (handle != 0L) return START_NOT_STICKY
        createChannel()
        val port = intent?.getIntExtra(PORT, 8554) ?: 8554
        val path = intent?.getStringExtra(PATH) ?: "/camera"
        startForeground(NOTIFICATION_ID, notification("Starting rtsp://<LAN-IP>:$port$path"))
        try {
            handle = NativeBridge.nativeCreateServer(port, path, intent?.getStringExtra(USERNAME).orEmpty(), intent?.getStringExtra(PASSWORD).orEmpty(), 0)
            check(handle != 0L) { "Native RTSP server did not start" }
            val requested = (intent?.getStringExtra(RESOLUTION) ?: "1280x720").split('x').let { Size(it.getOrNull(0)?.toIntOrNull() ?: 1280, it.getOrNull(1)?.toIntOrNull() ?: 720) }
            acquireLocks()
            encoder = CameraEncoder(this, intent?.getStringExtra(CAMERA).orEmpty(), requested, intent?.getIntExtra(FPS, 30) ?: 30, intent?.getIntExtra(BITRATE, 3_000_000) ?: 3_000_000, handle) { error -> stopWithError(error) }.also { it.start() }
            statusMessage = "Streaming rtsp://<LAN-IP>:$port$path"
            isStreaming = true
            startForeground(NOTIFICATION_ID, notification(statusMessage))
        } catch (error: Throwable) { stopWithError(error.message ?: "Unable to start streaming") }
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() { stopStreaming(); super.onDestroy() }

    private fun stopWithError(error: String) { statusMessage = "Stopped: $error"; startForeground(NOTIFICATION_ID, notification("Stopped: $error")); stopStreaming(); stopSelf() }
    private fun stopStreaming() {
        isStreaming = false
        if (!statusMessage.startsWith("Stopped:")) statusMessage = "Idle · ready for your next stream"
        encoder?.stop(); encoder = null
        if (handle != 0L) runCatching { NativeBridge.nativeStopServer(handle) }; handle = 0L
        wakeLock?.let { if (it.isHeld) it.release() }; wakeLock = null
        wifiLock?.let { if (it.isHeld) it.release() }; wifiLock = null
        stopForeground(STOP_FOREGROUND_REMOVE)
    }
    private fun acquireLocks() {
        wakeLock = getSystemService(PowerManager::class.java).newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "camrtsp:stream").also { it.acquire() }
        wifiLock = getSystemService(WifiManager::class.java).createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "camrtsp:stream").also { it.acquire() }
    }
    private fun createChannel() {
        getSystemService(NotificationManager::class.java).createNotificationChannel(NotificationChannel(CHANNEL, "camrtsp streaming", NotificationManager.IMPORTANCE_LOW))
    }
    private fun notification(text: String): Notification = NotificationCompat.Builder(this, CHANNEL)
        .setSmallIcon(android.R.drawable.presence_video_online).setContentTitle("camrtsp").setContentText(text).setOngoing(true)
        .addAction(android.R.drawable.ic_menu_close_clear_cancel, "Stop", PendingIntent.getService(this, 1, Intent(this, StreamService::class.java).setAction(STOP), PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)).build()

    companion object {
        @Volatile var statusMessage = "Idle · ready for your next stream"
        @Volatile var isStreaming = false
        const val CAMERA = "camera"; const val RESOLUTION = "resolution"; const val FPS = "fps"; const val BITRATE = "bitrate"; const val PORT = "port"; const val PATH = "path"; const val USERNAME = "username"; const val PASSWORD = "password"
        private const val CHANNEL = "streaming"; private const val NOTIFICATION_ID = 1; private const val STOP = "com.camrtsp.STOP"
    }
}
