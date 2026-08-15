package com.localrecord.audio

import java.io.File
import java.io.RandomAccessFile
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Streams s16le PCM into a WAV file, patching the RIFF header sizes on
 * close. Header is written up front with placeholder sizes so the file is
 * playable even after a crash (most players tolerate the wrong size).
 */
class WavWriter(
    file: File,
    private val sampleRateHz: Int,
    private val channels: Int = 1,
) : AutoCloseable {

    private val raf = RandomAccessFile(file, "rw")
    private var dataBytes = 0L

    init {
        raf.setLength(0)
        raf.write(buildHeader(0))
    }

    fun write(pcm: ByteArray, length: Int = pcm.size) {
        raf.write(pcm, 0, length)
        dataBytes += length
    }

    override fun close() {
        raf.seek(0)
        raf.write(buildHeader(dataBytes))
        raf.close()
    }

    private fun buildHeader(dataSize: Long): ByteArray {
        val bitsPerSample = 16
        val byteRate = sampleRateHz * channels * bitsPerSample / 8
        val blockAlign = channels * bitsPerSample / 8
        return ByteBuffer.allocate(44).order(ByteOrder.LITTLE_ENDIAN).apply {
            put("RIFF".toByteArray(Charsets.US_ASCII))
            putInt((36 + dataSize).toInt())
            put("WAVE".toByteArray(Charsets.US_ASCII))
            put("fmt ".toByteArray(Charsets.US_ASCII))
            putInt(16)                    // PCM fmt chunk size
            putShort(1)                   // PCM format
            putShort(channels.toShort())
            putInt(sampleRateHz)
            putInt(byteRate)
            putShort(blockAlign.toShort())
            putShort(bitsPerSample.toShort())
            put("data".toByteArray(Charsets.US_ASCII))
            putInt(dataSize.toInt())
        }.array()
    }
}
