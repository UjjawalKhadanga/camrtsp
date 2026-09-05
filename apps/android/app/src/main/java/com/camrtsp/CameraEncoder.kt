package com.camrtsp

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.hardware.camera2.*
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.util.Range
import android.util.Size
import android.view.Surface
import java.nio.ByteBuffer
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/** Camera and codec callbacks share one thread; JNI copies each output before release. */
class CameraEncoder(
    private val context: Context,
    private val requestedCamera: String,
    private val requestedSize: Size,
    private val requestedFps: Int,
    private val bitrate: Int,
    private val handle: Long,
    private val onConfigured: (String) -> Unit,
    private val onError: (String) -> Unit,
) {
    private val thread = HandlerThread("camrtsp-codec").apply { start() }
    private val handler = Handler(thread.looper)
    private val stopped = AtomicBoolean(false)
    private var codec: MediaCodec? = null
    private var camera: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var surface: Surface? = null

    fun start() {
        handler.post {
            if (stopped.get()) return@post
            try { configure() } catch (error: Exception) { fail(error.message ?: "Camera startup failed") }
        }
    }

    private fun fail(message: String) { if (!stopped.get()) onError(message) }

    private fun configure() {
        check(context.checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            "Camera permission is required to stream"
        }
        require(requestedSize.width in 1..16384 && requestedSize.height in 1..16384 && requestedFps in 1..240 && bitrate > 0) {
            "Invalid resolution, FPS, or bitrate"
        }
        val manager = context.getSystemService(CameraManager::class.java)
        val id = if (requestedCamera.isNotBlank()) {
            require(manager.cameraIdList.contains(requestedCamera)) { "Camera '$requestedCamera' was not found" }
            requestedCamera
        } else manager.cameraIdList.firstOrNull {
            manager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_BACK
        } ?: manager.cameraIdList.firstOrNull() ?: error("No camera is available")
        val characteristics = manager.getCameraCharacteristics(id)
        val encoder = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec = encoder
        val capabilities = encoder.codecInfo.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC).videoCapabilities
            ?: error("H.264 encoder has no video capabilities")
        val map = characteristics.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
            ?: error("Camera has no stream configuration")
        val size = map.getOutputSizes(MediaCodec::class.java).orEmpty()
            .filter { capabilities.isSizeSupported(it.width, it.height) }
            .minByOrNull { kotlin.math.abs(it.width - requestedSize.width) + kotlin.math.abs(it.height - requestedSize.height) }
            ?: error("Camera and H.264 encoder have no compatible surface size")
        val minDuration = map.getOutputMinFrameDuration(MediaCodec::class.java, size)
        val maxFps = if (minDuration > 0) 1_000_000_000.0 / minDuration else 240.0
        val ranges = characteristics.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES).orEmpty()
        val range = ranges.filter { it.upper <= maxFps + 0.01 && capabilities.areSizeAndRateSupported(size.width, size.height, it.upper.toDouble()) }
            .minWithOrNull(compareBy<Range<Int>>(
                { if (requestedFps < it.lower) it.lower - requestedFps else if (requestedFps > it.upper) requestedFps - it.upper else 0 },
                { kotlin.math.abs(it.upper - requestedFps) }, { it.upper - it.lower }
            )) ?: error("Camera and encoder have no compatible FPS range")
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, size.width, size.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, capabilities.bitrateRange.clamp(bitrate))
            setInteger(MediaFormat.KEY_FRAME_RATE, range.upper)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
            setInteger(MediaFormat.KEY_PRIORITY, 0)
        }
        // A variable AE range must not be advertised as an exact frame rate in SDP.
        NativeBridge.nativeSetFrameRate(handle, if (range.lower == range.upper) range.upper else 0, 1)
        encoder.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) = Unit
            override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                try {
                    if (!stopped.get() && info.size > 0 && info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                        codec.getOutputBuffer(index)?.let {
                            NativeBridge.nativePushAccessUnit(handle, it, info.offset, info.size, info.presentationTimeUs, info.flags)
                        }
                    }
                } catch (error: Exception) { fail("Encoder output: ${error.message}") }
                finally { runCatching { codec.releaseOutputBuffer(index, false) } }
            }
            override fun onOutputFormatChanged(codec: MediaCodec, outputFormat: MediaFormat) {
                if (stopped.get()) return
                try {
                    val sps = outputFormat.getByteBuffer("csd-0")?.copyBytes()?.withoutStartCode()
                    val pps = outputFormat.getByteBuffer("csd-1")?.copyBytes()?.withoutStartCode()
                    check(sps != null && pps != null) { "H.264 encoder did not publish SPS/PPS" }
                    NativeBridge.nativeSetCodecConfig(handle, sps, pps)
                } catch (error: Exception) { fail("Codec configuration: ${error.message}") }
            }
            override fun onError(codec: MediaCodec, error: MediaCodec.CodecException) { fail("MediaCodec: ${error.diagnosticInfo}") }
        }, handler)
        encoder.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val inputSurface = encoder.createInputSurface()
        surface = inputSurface
        encoder.start()
        manager.openCamera(id, object : CameraDevice.StateCallback() {
            override fun onOpened(device: CameraDevice) {
                if (stopped.get()) { device.close(); return }
                camera = device
                try {
                    device.createCaptureSession(listOf(inputSurface), object : CameraCaptureSession.StateCallback() {
                        override fun onConfigured(captureSession: CameraCaptureSession) {
                            if (stopped.get()) { captureSession.close(); return }
                            session = captureSession
                            try {
                                val request = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                                    addTarget(inputSurface)
                                    set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, range)
                                    val modes = (characteristics.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES) ?: intArrayOf())
                                    if (CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO in modes) {
                                        set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO)
                                    }
                                }
                                captureSession.setRepeatingRequest(request.build(), null, handler)
                                val fpsText = if (range.lower == range.upper) "${range.upper}" else "${range.lower}–${range.upper}"
                                onConfigured("${size.width}x${size.height} · $fpsText FPS · ${encoder.name}")
                            } catch (error: Exception) { fail("Camera request: ${error.message}") }
                        }
                        override fun onConfigureFailed(captureSession: CameraCaptureSession) { captureSession.close(); fail("Camera capture configuration failed") }
                    }, handler)
                } catch (error: Exception) { fail("Camera session: ${error.message}") }
            }
            override fun onDisconnected(device: CameraDevice) { device.close(); fail("Camera disconnected") }
            override fun onError(device: CameraDevice, error: Int) { device.close(); fail("Camera error $error") }
        }, handler)
    }

    fun requestKeyframe() {
        handler.post {
            if (!stopped.get()) runCatching {
                codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
            }.onFailure { android.util.Log.w("camrtsp", "Keyframe request unavailable", it) }
        }
    }

    /** Called on the service worker, never on the UI or codec callback thread. */
    fun stop() {
        if (!stopped.compareAndSet(false, true)) return
        val complete = CountDownLatch(1)
        handler.post {
            try {
                runCatching { session?.stopRepeating() }
                runCatching { session?.close() }; session = null
                runCatching { camera?.close() }; camera = null
                runCatching { codec?.stop() }
                runCatching { codec?.release() }; codec = null
                runCatching { surface?.release() }; surface = null
            } finally { complete.countDown(); thread.quitSafely() }
        }
        if (!complete.await(3, TimeUnit.SECONDS)) android.util.Log.w("camrtsp", "Camera cleanup is taking longer than expected")
    }

    private fun ByteBuffer.copyBytes(): ByteArray = duplicate().let { copy -> ByteArray(copy.remaining()).also { copy.get(it) } }
    private fun ByteArray.withoutStartCode(): ByteArray = when {
        size >= 4 && this[0] == 0.toByte() && this[1] == 0.toByte() && this[2] == 0.toByte() && this[3] == 1.toByte() -> copyOfRange(4, size)
        size >= 3 && this[0] == 0.toByte() && this[1] == 0.toByte() && this[2] == 1.toByte() -> copyOfRange(3, size)
        else -> this
    }
}
