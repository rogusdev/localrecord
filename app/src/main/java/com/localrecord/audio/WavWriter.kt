package com.localrecord.audio

import java.io.File
import java.io.RandomAccessFile
import java.nio.ByteBuffer
import java.nio.ByteOrder

private const val HEADER_BYTES = 44L
private const val BITS_PER_SAMPLE = 16

/**
 * Streams s16le PCM into a WAV file. The RIFF header sizes are re-patched
 * after every ~1 s of audio and on close, so a recording cut short by a crash
 * is still a valid WAV missing at most its last second.
 */
class WavWriter(
    file: File,
    private val sampleRateHz: Int,
    private val channels: Int = 1,
) : AutoCloseable {

    private val raf = RandomAccessFile(file, "rw")
    private val byteRate = sampleRateHz * channels * BITS_PER_SAMPLE / 8
    private var dataBytes = 0L
    private var headerDataBytes = 0L

    init {
        raf.setLength(0)
        writeHeader()
    }

    fun write(pcm: ByteArray, length: Int = pcm.size) {
        raf.write(pcm, 0, length)
        dataBytes += length
        if (dataBytes - headerDataBytes >= byteRate) {
            writeHeader()
            raf.seek(HEADER_BYTES + dataBytes)
        }
    }

    override fun close() {
        raf.use { writeHeader() }
    }

    private fun writeHeader() {
        raf.seek(0)
        raf.write(buildHeader(dataBytes))
        headerDataBytes = dataBytes
    }

    private fun buildHeader(dataSize: Long): ByteArray {
        val blockAlign = channels * BITS_PER_SAMPLE / 8
        return ByteBuffer.allocate(HEADER_BYTES.toInt()).order(ByteOrder.LITTLE_ENDIAN).apply {
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
            putShort(BITS_PER_SAMPLE.toShort())
            put("data".toByteArray(Charsets.US_ASCII))
            putInt(dataSize.toInt())
        }.array()
    }
}
