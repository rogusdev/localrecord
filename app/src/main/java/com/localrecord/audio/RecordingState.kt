package com.localrecord.audio

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import uniffi.whisper_engine.Segment
import java.io.File

/**
 * Process-wide bridge between RecordingService and the UI. The service is
 * the only writer; Compose collects the flows.
 */
object RecordingState {
    private val _isRecording = MutableStateFlow(false)
    val isRecording: StateFlow<Boolean> = _isRecording.asStateFlow()

    private val _elapsedMs = MutableStateFlow(0L)
    val elapsedMs: StateFlow<Long> = _elapsedMs.asStateFlow()

    private val _liveSegments = MutableStateFlow<List<Segment>>(emptyList())
    val liveSegments: StateFlow<List<Segment>> = _liveSegments.asStateFlow()

    /** True while live transcription is running for the current recording. */
    private val _transcriptionActive = MutableStateFlow(false)
    val transcriptionActive: StateFlow<Boolean> = _transcriptionActive.asStateFlow()

    /** Why transcription is off although the model is present (load or backend failure). */
    private val _transcriptionError = MutableStateFlow<String?>(null)
    val transcriptionError: StateFlow<String?> = _transcriptionError.asStateFlow()

    /** WAV file being written right now; Drive upload must skip it. */
    @Volatile
    var activeFile: File? = null
        private set

    internal fun onRecordingStarted(file: File, transcribing: Boolean, error: String?) {
        activeFile = file
        _liveSegments.value = emptyList()
        _elapsedMs.value = 0L
        _transcriptionActive.value = transcribing
        _transcriptionError.value = error
        _isRecording.value = true
    }

    internal fun onElapsed(ms: Long) {
        _elapsedMs.value = ms
    }

    /** Transcription stopped mid-recording; recording carries on. Idempotent. */
    internal fun onTranscriptionFailed(error: String) {
        _transcriptionActive.value = false
        _transcriptionError.value = error
    }

    internal fun appendSegments(segments: List<Segment>) {
        if (segments.isEmpty()) return
        _liveSegments.update { it + segments }
    }

    internal fun onRecordingStopped() {
        activeFile = null
        _isRecording.value = false
        _transcriptionActive.value = false
    }
}
