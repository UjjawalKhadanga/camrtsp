package com.camrtsp

import android.Manifest
import android.content.pm.PackageManager
import android.content.Context
import android.graphics.ImageFormat
import android.hardware.camera2.*
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Handler
import android.os.HandlerThread
import android.util.Range
import android.util.Size
import android.view.Surface
import java.nio.ByteBuffer

/** Camera2 -> MediaCodec surface path. The encoder buffer is copied by JNI before release. */
class CameraEncoder(
    private val context: Context,
    private val requestedCamera: String,
    private val requestedSize: Size,
    private val requestedFps: Int,
    private val bitrate: Int,
    private val handle: Long,
    private val onError: (String) -> Unit,
) {
    private val thread = HandlerThread("camrtsp-codec").apply { start() }
    private val handler = Handler(thread.looper)
    private var codec: MediaCodec? = null
    private var camera: CameraDevice? = null
    private var session: CameraCaptureSession? = null

    fun start() {
        if (context.checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
            throw SecurityException("Camera permission is required to stream")
        }
        val manager = context.getSystemService(CameraManager::class.java)
        val id = selectCamera(manager)
        val choice = chooseSizeAndFps(manager.getCameraCharacteristics(id))
        val encoder = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, choice.first.width, choice.first.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, choice.second)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
            if (android.os.Build.VERSION.SDK_INT >= 23) setInteger(MediaFormat.KEY_PRIORITY, 0)
        }
        encoder.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) = Unit
            override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                try {
                    if (info.size > 0) {
                        val buffer = codec.getOutputBuffer(index)
                        if (buffer != null) NativeBridge.nativePushAccessUnit(handle, buffer, info.offset, info.size, info.presentationTimeUs, info.flags)
                    }
                } catch (error: Throwable) { onError("Encoder output error: ${error.message}") }
                finally { codec.releaseOutputBuffer(index, false) }
            }
            override fun onOutputFormatChanged(codec: MediaCodec, outputFormat: MediaFormat) {
                val sps = outputFormat.getByteBuffer("csd-0")?.copyBytes()?.withoutStartCode()
                val pps = outputFormat.getByteBuffer("csd-1")?.copyBytes()?.withoutStartCode()
                if (sps != null && pps != null) {
                    try { NativeBridge.nativeSetCodecConfig(handle, sps, pps) }
                    catch (error: Throwable) { onError("Codec configuration error: ${error.message}") }
                } else onError("H.264 encoder did not publish SPS/PPS")
            }
            override fun onError(codec: MediaCodec, error: MediaCodec.CodecException) { onError("MediaCodec: ${error.diagnosticInfo}") }
        }, handler)
        encoder.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val surface = encoder.createInputSurface()
        encoder.start()
        codec = encoder
        manager.openCamera(id, object : CameraDevice.StateCallback() {
            override fun onOpened(device: CameraDevice) {
                camera = device
                device.createCaptureSession(listOf(surface), object : CameraCaptureSession.StateCallback() {
                    override fun onConfigured(captureSession: CameraCaptureSession) {
                        session = captureSession
                        val request = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                            addTarget(surface)
                            set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, Range(choice.second, choice.second))
                            set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO)
                        }
                        captureSession.setRepeatingRequest(request.build(), null, handler)
                    }
                    override fun onConfigureFailed(captureSession: CameraCaptureSession) { onError("Camera capture session configuration failed") }
                }, handler)
            }
            override fun onDisconnected(device: CameraDevice) { device.close(); onError("Camera disconnected") }
            override fun onError(device: CameraDevice, error: Int) { device.close(); onError("Camera error $error") }
        }, handler)
    }

    fun stop() {
        runCatching { session?.stopRepeating() }; session?.close(); session = null
        camera?.close(); camera = null
        codec?.stop(); codec?.release(); codec = null
        thread.quitSafely()
    }

    private fun selectCamera(manager: CameraManager): String {
        if (requestedCamera.isNotBlank() && manager.cameraIdList.contains(requestedCamera)) return requestedCamera
        return manager.cameraIdList.firstOrNull { manager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_BACK }
            ?: manager.cameraIdList.firstOrNull() ?: error("No camera is available")
    }

    private fun chooseSizeAndFps(characteristics: CameraCharacteristics): Pair<Size, Int> {
        val map = characteristics.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP) ?: error("Camera has no stream configuration")
        val sizes = map.getOutputSizes(ImageFormat.PRIVATE)?.toList().orEmpty()
        val size = sizes.minByOrNull { kotlin.math.abs(it.width - requestedSize.width) + kotlin.math.abs(it.height - requestedSize.height) }
            ?: error("Camera has no private surface output")
        val fps = characteristics.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES).orEmpty()
            .filter { it.lower <= requestedFps && it.upper >= requestedFps }.minByOrNull { it.upper - it.lower }?.let { requestedFps }
            ?: requestedFps
        return size to fps
    }

    private fun ByteBuffer.copyBytes(): ByteArray {
        val copy = duplicate(); return ByteArray(copy.remaining()).also { copy.get(it) }
    }
    private fun ByteArray.withoutStartCode(): ByteArray = when {
        size >= 4 && this[0] == 0.toByte() && this[1] == 0.toByte() && this[2] == 0.toByte() && this[3] == 1.toByte() -> copyOfRange(4, size)
        size >= 3 && this[0] == 0.toByte() && this[1] == 0.toByte() && this[2] == 1.toByte() -> copyOfRange(3, size)
        else -> this
    }
}
