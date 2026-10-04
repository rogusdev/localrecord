package com.localrecord.playback

import android.media.AudioAttributes
import android.media.MediaPlayer
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

/**
 * Plays one recording and publishes its position for the transcript
 * highlight. Use from the main thread; [open] prepares off it.
 */
class Player private constructor(
    private val mediaPlayer: MediaPlayer,
    private val scope: CoroutineScope,
) : AutoCloseable {

    companion object {
        /** Position refresh while playing; well under one spoken word. */
        private const val POSITION_TICK_MS = 50L

        /** @throws java.io.IOException if the file can't be read or decoded. */
        suspend fun open(file: File, scope: CoroutineScope): Player {
            val mediaPlayer = withContext(Dispatchers.IO) {
                MediaPlayer().apply {
                    setAudioAttributes(
                        AudioAttributes.Builder()
                            .setUsage(AudioAttributes.USAGE_MEDIA)
                            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                            .build()
                    )
                    setDataSource(file.path)
                    prepare()
                }
            }
            return Player(mediaPlayer, scope)
        }
    }

    val durationMs: Long = mediaPlayer.duration.toLong()

    private val _positionMs = MutableStateFlow(0L)
    val positionMs: StateFlow<Long> = _positionMs.asStateFlow()

    private val _isPlaying = MutableStateFlow(false)
    val isPlaying: StateFlow<Boolean> = _isPlaying.asStateFlow()

    private var ticker: Job? = null

    init {
        mediaPlayer.setOnCompletionListener {
            stopTicking()
            _positionMs.value = durationMs
        }
    }

    fun togglePlay() = if (mediaPlayer.isPlaying) pause() else play()

    fun play() {
        if (_positionMs.value >= durationMs) seekTo(0)
        mediaPlayer.start()
        _isPlaying.value = true
        ticker?.cancel()
        ticker = scope.launch {
            while (isActive) {
                _positionMs.value = mediaPlayer.currentPosition.toLong()
                delay(POSITION_TICK_MS)
            }
        }
    }

    fun pause() {
        mediaPlayer.pause()
        stopTicking()
        _positionMs.value = mediaPlayer.currentPosition.toLong()
    }

    fun seekTo(ms: Long) {
        val target = ms.coerceIn(0, durationMs)
        mediaPlayer.seekTo(target, MediaPlayer.SEEK_CLOSEST)
        _positionMs.value = target
    }

    override fun close() {
        stopTicking()
        mediaPlayer.release()
    }

    private fun stopTicking() {
        ticker?.cancel()
        ticker = null
        _isPlaying.value = false
    }
}
