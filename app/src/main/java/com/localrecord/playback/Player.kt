package com.localrecord.playback

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTimestamp
import android.media.AudioTrack
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.io.IOException
import java.io.RandomAccessFile
import java.util.concurrent.CountDownLatch

private const val TAG = "Player"

/** Position refresh while playing; well under one spoken word. */
private const val POSITION_TICK_MS = 50L

/** Bytes per AudioTrack write; a multiple of every frame size (2 or 4). */
private const val WRITE_BYTES = 16 * 1024

/**
 * Plays one recording (16-bit PCM WAV) through an AudioTrack and publishes
 * its position for the transcript highlight. The position is the frame the
 * speaker is playing now, from [AudioTrack.getTimestamp]; MediaPlayer's
 * position ran ~0.5 s ahead of the sound on the OnePlus 15.
 * Use from the main thread; [open] reads the header off it.
 */
class Player private constructor(
    private val file: File,
    private val format: WavFormat,
    private val scope: CoroutineScope,
) : AutoCloseable {

    companion object {
        /** @throws IOException if the file can't be read or isn't 16-bit PCM WAV. */
        suspend fun open(file: File, scope: CoroutineScope): Player {
            val format = withContext(Dispatchers.IO) { WavFormat.read(file) }
            return Player(file, format, scope)
        }
    }

    val durationMs: Long = format.framesToMs(format.frames)

    private val _positionMs = MutableStateFlow(0L)
    val positionMs: StateFlow<Long> = _positionMs.asStateFlow()

    private val _isPlaying = MutableStateFlow(false)
    val isPlaying: StateFlow<Boolean> = _isPlaying.asStateFlow()

    /** The track playing now; null while paused or stopped. */
    private var run: Run? = null

    fun togglePlay() = if (run != null) pause() else play()

    fun play() {
        if (run != null) return
        if (_positionMs.value >= durationMs) _positionMs.value = 0
        run = Run(format.msToFrames(_positionMs.value).coerceIn(0, format.frames))
        _isPlaying.value = true
    }

    fun pause() {
        val current = run ?: return
        _positionMs.value = current.positionMs()
        stop()
    }

    fun seekTo(ms: Long) {
        val playing = run != null
        stop()
        _positionMs.value = ms.coerceIn(0, durationMs)
        if (playing) play()
    }

    override fun close() = stop()

    private fun stop() {
        run?.stop()
        run = null
        _isPlaying.value = false
    }

    /**
     * One AudioTrack from [startFrame] to the end or [stop]. A writer thread
     * feeds it from the file and releases it once stopped, so the main thread
     * never touches a released track.
     */
    private inner class Run(private val startFrame: Long) {
        private val track = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build()
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(format.sampleRateHz)
                    .setChannelMask(
                        if (format.channels == 1) AudioFormat.CHANNEL_OUT_MONO
                        else AudioFormat.CHANNEL_OUT_STEREO
                    )
                    .build()
            )
            .setTransferMode(AudioTrack.MODE_STREAM)
            .build()
        private val timestamp = AudioTimestamp()
        /** Frames played as of the last position, kept if a timestamp is missing. */
        private var played = 0L

        @Volatile
        private var stopped = false
        private val stoppedSignal = CountDownLatch(1)
        private val ticker: Job

        init {
            track.play()
            Thread(::feed, "player-writer").start()
            ticker = scope.launch {
                while (isActive) {
                    val ms = positionMs()
                    _positionMs.value = ms
                    if (ms >= durationMs) this@Player.stop()
                    delay(POSITION_TICK_MS)
                }
            }
        }

        /**
         * Recording time at the speaker now. Holds at the start until the
         * track reports its first timestamp (output latency).
         */
        fun positionMs(): Long {
            if (track.getTimestamp(timestamp)) {
                val sinceNs = System.nanoTime() - timestamp.nanoTime
                val now = timestamp.framePosition + sinceNs * format.sampleRateHz / 1_000_000_000
                // can't be past what the mixer has consumed
                played = now.coerceIn(0, track.playbackHeadPosition.toLong() and 0xFFFFFFFFL)
            }
            return format.framesToMs((startFrame + played).coerceAtMost(format.frames))
        }

        fun stop() {
            ticker.cancel()
            stopped = true
            // interrupts a blocking write
            track.pause()
            stoppedSignal.countDown()
        }

        private fun feed() {
            try {
                RandomAccessFile(file, "r").use { raf ->
                    raf.seek(format.dataOffset + startFrame * format.frameBytes)
                    var remaining = (format.frames - startFrame) * format.frameBytes
                    val buffer = ByteArray(WRITE_BYTES)
                    while (remaining > 0 && !stopped) {
                        val length = minOf(WRITE_BYTES.toLong(), remaining).toInt()
                        raf.readFully(buffer, 0, length)
                        var offset = 0
                        while (offset < length && !stopped) {
                            val written = track.write(buffer, offset, length - offset)
                            if (written < 0) throw IOException("AudioTrack write failed: $written")
                            offset += written
                        }
                        remaining -= length
                    }
                }
                // the queued tail plays out; the ticker ends the run at the last frame
            } catch (e: IOException) {
                Log.e(TAG, "playback of ${file.name} stopped", e)
            }
            stoppedSignal.await()
            track.release()
        }
    }
}
