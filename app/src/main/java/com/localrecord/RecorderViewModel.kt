package com.localrecord

import android.app.Application
import android.util.Log
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.localrecord.audio.RecordingService
import com.localrecord.audio.RecordingState
import com.localrecord.data.Recording
import com.localrecord.data.RecordingRepository
import com.localrecord.engine.EngineManager
import com.localrecord.model.ModelDownloader
import com.localrecord.playback.Player
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.whisper_engine.Segment
import java.io.IOException

/** An open recording: its player and saved transcript (null if none). */
data class Playback(
    val recording: Recording,
    val transcript: List<Segment>?,
    val player: Player,
)

class RecorderViewModel(app: Application) : AndroidViewModel(app) {

    private companion object {
        const val TAG = "RecorderViewModel"
    }

    val isRecording = RecordingState.isRecording
    val elapsedMs = RecordingState.elapsedMs
    val liveSegments = RecordingState.liveSegments
    val transcriptionActive = RecordingState.transcriptionActive
    val transcriptionError = RecordingState.transcriptionError
    val modelState = ModelDownloader.state

    private val _recordings = MutableStateFlow<List<Recording>>(emptyList())
    val recordings: StateFlow<List<Recording>> = _recordings.asStateFlow()

    private val _playback = MutableStateFlow<Playback?>(null)
    val playback: StateFlow<Playback?> = _playback.asStateFlow()

    /** One-shot message for the snackbar; cleared via [messageShown]. */
    private val _message = MutableStateFlow<String?>(null)
    val message: StateFlow<String?> = _message.asStateFlow()

    init {
        refreshRecordings()
        viewModelScope.launch {
            isRecording.collect { recording ->
                if (!recording) refreshRecordings()
            }
        }
        // Load the model as soon as it's on disk so the first recording
        // doesn't wait on it.
        viewModelScope.launch {
            modelState.first { it is ModelDownloader.State.Ready }
            withContext(Dispatchers.IO) { EngineManager.preload(getApplication()) }
        }
    }

    fun startRecording() = RecordingService.start(getApplication())

    fun stopRecording() = RecordingService.stop(getApplication())

    fun downloadModel() {
        viewModelScope.launch { ModelDownloader.download(getApplication()) }
    }

    fun openPlayback(recording: Recording) {
        viewModelScope.launch {
            closePlayback()
            val transcript = withContext(Dispatchers.IO) {
                RecordingRepository.readTranscript(recording.wavFile)
            }
            val player = try {
                Player.open(recording.wavFile, viewModelScope)
            } catch (e: IOException) {
                Log.e(TAG, "can't play ${recording.wavFile.name}", e)
                _message.value = "Can't play ${recording.name}: ${e.message}"
                return@launch
            }
            _playback.value = Playback(recording, transcript, player)
        }
    }

    fun closePlayback() {
        _playback.value?.player?.close()
        _playback.value = null
    }

    fun messageShown() {
        _message.value = null
    }

    override fun onCleared() {
        closePlayback()
    }

    fun deleteRecording(recording: Recording) {
        viewModelScope.launch(Dispatchers.IO) {
            RecordingRepository.delete(recording)
            refreshRecordings()
        }
    }

    private fun refreshRecordings() {
        viewModelScope.launch {
            _recordings.value = withContext(Dispatchers.IO) {
                RecordingRepository.list(getApplication())
            }
        }
    }
}
