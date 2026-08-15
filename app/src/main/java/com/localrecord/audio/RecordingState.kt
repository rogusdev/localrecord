package com.localrecord.audio

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
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

    /** True while transcription is disabled because no model is loaded. */
    private val _transcriptionActive = MutableStateFlow(false)
    val transcriptionActive: StateFlow<Boolean> = _transcriptionActive.asStateFlow()

    internal fun onRecordingStarted(transcribing: Boolean) {
        _liveSegments.value = emptyList()
        _elapsedMs.value = 0L
        _transcriptionActive.value = transcribing
        _isRecording.value = true
    }

    internal fun onElapsed(ms: Long) {
        _elapsedMs.value = ms
    }

    internal fun appendSegments(segments: List<Segment>) {
        if (segments.isEmpty()) return
        _liveSegments.value = _liveSegments.value + segments
    }

    internal fun onRecordingStopped() {
        _isRecording.value = false
        _transcriptionActive.value = false
    }
}
