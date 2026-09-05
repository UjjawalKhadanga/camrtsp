package com.camrtsp

import java.nio.ByteBuffer

object NativeBridge {
    init { System.loadLibrary("camrtsp_android") }

    external fun nativeCreateServer(port: Int, path: String, username: String, password: String, transport: Int): Long
    external fun nativeSetCodecConfig(handle: Long, sps: ByteArray, pps: ByteArray)
    external fun nativePushAccessUnit(handle: Long, buffer: ByteBuffer, offset: Int, size: Int, ptsUs: Long, flags: Int)
    external fun nativeSetFrameRate(handle: Long, numerator: Int, denominator: Int)
    external fun nativeTakeKeyframeRequest(handle: Long): Int
    external fun nativeGetStats(handle: Long): String
    external fun nativeStopServer(handle: Long)
}
