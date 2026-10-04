package com.localrecord.audio

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import uniffi.whisper_engine.Segment

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

    /** Not-yet-final text after [liveSegments]; replaced on every pass. */
    private val _tentativeSegments = MutableStateFlow<List<Segment>>(emptyList())
    val tentativeSegments: StateFlow<List<Segment>> = _tentativeSegments.asStateFlow()

    /** True while live transcription is running for the current recording. */
    private val _transcriptionActive = MutableStateFlow(false)
    val transcriptionActive: StateFlow<Boolean> = _transcriptionActive.asStateFlow()

    /** Why transcription is off although the model is present (load or backend failure). */
    private val _transcriptionError = MutableStateFlow<String?>(null)
    val transcriptionError: StateFlow<String?> = _transcriptionError.asStateFlow()

    internal fun onRecordingStarted(transcribing: Boolean, error: String?) {
        _liveSegments.value = emptyList()
        _tentativeSegments.value = emptyList()
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

    internal fun setTentative(segments: List<Segment>) {
        _tentativeSegments.value = segments
    }

    internal fun appendSegments(segments: List<Segment>) {
        if (segments.isEmpty()) return
        _liveSegments.update { it + segments }
    }

    internal fun onRecordingStopped() {
        _tentativeSegments.value = emptyList()
        _isRecording.value = false
        _transcriptionActive.value = false
    }
}
