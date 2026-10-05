package com.localrecord.playback

import java.io.File
import java.io.IOException
import java.io.RandomAccessFile
import java.nio.ByteBuffer
import java.nio.ByteOrder

private const val FORMAT_PCM = 1
private const val FORMAT_EXTENSIBLE = 0xFFFE
private const val BITS_PER_SAMPLE = 16
private const val FMT_BYTES = 16

/**
 * Where a PCM16 WAV's samples are and how they're laid out.
 * [frames] is the data chunk's size, or the rest of the file when that size
 * is unset (0) or more than the file holds (a truncated copy).
 */
class WavFormat(
    val sampleRateHz: Int,
    val channels: Int,
    val dataOffset: Long,
    val frames: Long,
) {
    val frameBytes: Int get() = channels * BITS_PER_SAMPLE / 8

    fun framesToMs(frames: Long): Long = frames * 1000 / sampleRateHz

    fun msToFrames(ms: Long): Long = ms * sampleRateHz / 1000

    companion object {
        /** @throws IOException if [file] isn't a 16-bit PCM WAV (mono or stereo). */
        fun read(file: File): WavFormat = RandomAccessFile(file, "r").use { raf ->
            val riff = ByteArray(12).also { raf.readFully(it) }
            if (String(riff, 0, 4, Charsets.US_ASCII) != "RIFF" ||
                String(riff, 8, 4, Charsets.US_ASCII) != "WAVE"
            ) {
                throw IOException("not a WAV file")
            }
            var fmt: ByteBuffer? = null
            val chunk = ByteArray(8)
            while (raf.filePointer + 8 <= raf.length()) {
                raf.readFully(chunk)
                val id = String(chunk, 0, 4, Charsets.US_ASCII)
                val size = ByteBuffer.wrap(chunk, 4, 4).order(ByteOrder.LITTLE_ENDIAN).int.toLong() and 0xFFFFFFFFL
                val body = raf.filePointer
                when (id) {
                    "fmt " -> {
                        if (size < FMT_BYTES) throw IOException("truncated fmt chunk")
                        val bytes = ByteArray(FMT_BYTES).also { raf.readFully(it) }
                        fmt = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
                    }
                    "data" -> return parse(fmt ?: throw IOException("data before fmt chunk"), body, size, raf.length())
                }
                // chunks are padded to an even size
                raf.seek(body + size + (size and 1))
            }
            throw IOException("no audio data")
        }

        private fun parse(fmt: ByteBuffer, dataOffset: Long, dataBytes: Long, fileBytes: Long): WavFormat {
            val formatTag = fmt.getShort(0).toInt() and 0xFFFF
            val channels = fmt.getShort(2).toInt()
            val rate = fmt.getInt(4)
            val bits = fmt.getShort(14).toInt()
            if ((formatTag != FORMAT_PCM && formatTag != FORMAT_EXTENSIBLE) || bits != BITS_PER_SAMPLE) {
                throw IOException("unsupported WAV encoding (only 16-bit PCM plays)")
            }
            if (channels !in 1..2 || rate <= 0) throw IOException("unsupported WAV layout")
            val available = fileBytes - dataOffset
            val bytes = if (dataBytes == 0L || dataBytes > available) available else dataBytes
            val frameBytes = channels * BITS_PER_SAMPLE / 8
            return WavFormat(rate, channels, dataOffset, bytes / frameBytes)
        }
    }
}
