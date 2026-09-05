package com.camrtsp

import android.app.*
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.*
import android.util.Size
import androidx.core.app.NotificationCompat
import org.json.JSONObject
import java.net.Inet4Address
import java.net.NetworkInterface
import java.util.concurrent.atomic.AtomicBoolean

class StreamService : Service() {
    private val workerThread = HandlerThread("camrtsp-service").apply { start() }
    private val worker = Handler(workerThread.looper)
    private val destroyed = AtomicBoolean(false)
    private var handle = 0L
    private var encoder: CameraEncoder? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var wanted = false
    private var generation = 0
    private var retries = 0
    private var options: Intent? = null
    private var startedAt = 0L
    private var negotiated = "Waiting for camera"
    private var previousFrames = 0L
    private var previousPoll = 0L
    private var measuredFps = 0.0
    private var lastNotification = ""

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == STOP) { stopSelf(); return START_NOT_STICKY }
        createChannel()
        startForeground(NOTIFICATION_ID, notification("Starting camera"))
        worker.post {
            if (!wanted) {
                wanted = true
                isStreaming = true
                retries = 0
                options = intent ?: Intent()
                startAttempt()
            }
        }
        return START_NOT_STICKY
    }

    private fun startAttempt() {
        if (!wanted || destroyed.get()) return
        val token = ++generation
        val intent = options ?: return
        try {
            val port = intent.getIntExtra(PORT, 8554)
            val path = intent.getStringExtra(PATH) ?: "/camera"
            val dimensions = (intent.getStringExtra(RESOLUTION) ?: "1280x720").split('x')
            require(dimensions.size == 2) { "Expected WIDTHxHEIGHT" }
            val size = Size(dimensions[0].toInt(), dimensions[1].toInt())
            acquireLocks()
            handle = NativeBridge.nativeCreateServer(port, path, intent.getStringExtra(USERNAME).orEmpty(),
                intent.getStringExtra(PASSWORD).orEmpty(), intent.getIntExtra(TRANSPORT, 0))
            check(handle != 0L) { "Native RTSP server did not start" }
            startedAt = SystemClock.elapsedRealtime()
            previousPoll = startedAt
            previousFrames = 0
            measuredFps = 0.0
            negotiated = "Waiting for camera"
            encoder = CameraEncoder(this, intent.getStringExtra(CAMERA).orEmpty(), size,
                intent.getIntExtra(FPS, 30), intent.getIntExtra(BITRATE, 3_000_000), handle,
                { info -> worker.post { if (token == generation && wanted) negotiated = info } },
                { error -> worker.post { if (token == generation && wanted) recover(error) } })
            encoder?.start()
            statusMessage = "Starting ${streamUrls(port, path)}"
            worker.post(poll)
        } catch (error: Exception) { recover(error.message ?: "Unable to start streaming") }
          catch (error: LinkageError) { finishWithError("Native library unavailable: ${error.message}") }
    }

    private val poll = object : Runnable {
        override fun run() {
            if (!wanted || destroyed.get() || handle == 0L) return
            try {
                val stats = JSONObject(NativeBridge.nativeGetStats(handle))
                if (!stats.optBoolean("active")) { recover("RTSP server stopped"); return }
                val now = SystemClock.elapsedRealtime()
                if ((!stats.optBoolean("ready") && now - startedAt > 15_000) || stats.optLong("last_frame_age_ms", 0) > 15_000) {
                    recover("Camera stopped delivering encoded frames"); return
                }
                if (NativeBridge.nativeTakeKeyframeRequest(handle) != 0) encoder?.requestKeyframe()
                val frames = stats.optLong("frames")
                if (now - previousPoll >= 2000) {
                    measuredFps = (frames - previousFrames) * 1000.0 / (now - previousPoll)
                    previousFrames = frames; previousPoll = now
                }
                if (stats.optBoolean("ready")) {
                    val port = options?.getIntExtra(PORT, 8554) ?: 8554
                    val path = options?.getStringExtra(PATH) ?: "/camera"
                    statusMessage = "Streaming ${streamUrls(port, path)}\n$negotiated\n${stats.optInt("viewers")} viewers · %.1f measured FPS".format(measuredFps)
                    // A sustained healthy run earns a fresh recovery budget.
                    if (now - startedAt > 60_000) retries = 0
                }
                val notificationText = if (stats.optBoolean("ready")) "Streaming · ${stats.optInt("viewers")} viewers" else "Starting camera"
                if (notificationText != lastNotification) {
                    getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(notificationText))
                    lastNotification = notificationText
                }
                worker.postDelayed(this, 500)
            } catch (error: Exception) { recover(error.message ?: "Unable to read stream status") }
        }
    }

    private fun recover(error: String) {
        if (!wanted || destroyed.get()) return
        releaseStream()
        if (retries >= 3) { finishWithError(error); return }
        val delay = 500L shl retries
        retries++
        statusMessage = "Reconnecting ($retries/3): $error"
        worker.postDelayed({ if (wanted) startAttempt() }, delay)
    }

    private fun finishWithError(error: String) {
        wanted = false
        releaseStream()
        isStreaming = false
        statusMessage = "Stopped: $error"
        stopSelf()
    }

    private fun releaseStream() {
        generation++
        worker.removeCallbacks(poll)
        encoder?.stop(); encoder = null
        if (handle != 0L) runCatching { NativeBridge.nativeStopServer(handle) }
        handle = 0L
        wakeLock?.let { if (it.isHeld) it.release() }; wakeLock = null
        wifiLock?.let { if (it.isHeld) it.release() }; wifiLock = null
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        destroyed.set(true)
        worker.post {
            wanted = false
            worker.removeCallbacksAndMessages(null)
            releaseStream()
            isStreaming = false
            if (!statusMessage.startsWith("Stopped:")) statusMessage = "Idle · ready for your next stream"
            workerThread.quitSafely()
        }
        stopForeground(STOP_FOREGROUND_REMOVE)
        super.onDestroy()
    }

    private fun streamUrls(port: Int, path: String): String {
        val addresses = runCatching {
            NetworkInterface.getNetworkInterfaces().toList().filter { it.isUp && !it.isLoopback }
                .flatMap { it.inetAddresses.toList() }.filterIsInstance<Inet4Address>()
                .filter { !it.isLoopbackAddress && !it.isLinkLocalAddress }.mapNotNull { it.hostAddress }.distinct()
        }.getOrDefault(emptyList())
        return if (addresses.isEmpty()) "rtsp://127.0.0.1:$port$path (no LAN address)"
            else addresses.joinToString("\n") { "rtsp://$it:$port$path" }
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
        const val CAMERA = "camera"; const val RESOLUTION = "resolution"; const val FPS = "fps"; const val BITRATE = "bitrate"; const val PORT = "port"; const val PATH = "path"; const val USERNAME = "username"; const val PASSWORD = "password"; const val TRANSPORT = "transport"
        private const val CHANNEL = "streaming"; private const val NOTIFICATION_ID = 1; private const val STOP = "com.camrtsp.STOP"
    }
}
