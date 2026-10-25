package com.localrecord

import android.app.Application
import android.net.Uri
import android.util.Log
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.localrecord.audio.RecordingService
import com.localrecord.audio.RecordingState
import com.localrecord.data.Recording
import com.localrecord.data.RecordingRepository
import com.localrecord.data.importRecordings
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
import uniffi.whisper_engine.WhisperEngineException
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
    val isPaused = RecordingState.isPaused
    val elapsedMs = RecordingState.elapsedMs
    val liveSegments = RecordingState.liveSegments
    val tentativeSegments = RecordingState.tentativeSegments
    val finalizing = RecordingState.finalizing
    val finalizeErrors = RecordingState.finalizeErrors
    val transcriptionActive = RecordingState.transcriptionActive
    val transcriptionError = RecordingState.transcriptionError
    val modelState = ModelDownloader.state

    private val _recordings = MutableStateFlow<List<Recording>>(emptyList())
    val recordings: StateFlow<List<Recording>> = _recordings.asStateFlow()

    private val _playback = MutableStateFlow<Playback?>(null)
    val playback: StateFlow<Playback?> = _playback.asStateFlow()

    /** True while picked files are being copied in. */
    private val _importing = MutableStateFlow(false)
    val importing: StateFlow<Boolean> = _importing.asStateFlow()

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
        // A finished final pass rewrote a transcript: reload what shows it.
        viewModelScope.launch {
            var previous = emptySet<String>()
            finalizing.collect { current ->
                val done = previous - current
                previous = current
                if (done.isEmpty()) return@collect
                refreshRecordings()
                val open = _playback.value ?: return@collect
                if (open.recording.audioFile.name in done) {
                    val transcript = withContext(Dispatchers.IO) {
                        RecordingRepository.readTranscript(open.recording.audioFile)
                    }
                    _playback.value = _playback.value?.takeIf { it.player === open.player }
                        ?.copy(transcript = transcript)
                }
            }
        }
        // Load the models already on disk, and again once a download
        // completes, so the first recording doesn't wait on them.
        viewModelScope.launch {
            withContext(Dispatchers.IO) { EngineManager.preload(getApplication()) }
            modelState.first { it is ModelDownloader.State.Ready }
            withContext(Dispatchers.IO) { EngineManager.preload(getApplication()) }
        }
    }

    fun startRecording() = RecordingService.start(getApplication())

    fun pauseRecording() = RecordingService.pause(getApplication())

    fun resumeRecording() = RecordingService.resume(getApplication())

    fun stopRecording() = RecordingService.stop(getApplication())

    fun downloadModel() {
        viewModelScope.launch { ModelDownloader.download(getApplication()) }
    }

    fun openPlayback(recording: Recording) {
        viewModelScope.launch {
            closePlayback()
            val transcript = withContext(Dispatchers.IO) {
                RecordingRepository.readTranscript(recording.audioFile)
            }
            val player = try {
                Player.open(recording.audioFile, viewModelScope)
            } catch (e: WhisperEngineException) {
                Log.e(TAG, "can't play ${recording.audioFile.name}", e)
                _message.value = "Can't play ${recording.name}: ${e.message}"
                return@launch
            }
            _playback.value = Playback(recording, transcript, player)
        }
    }

    /** Re-run the accurate whole-recording transcription; replaces the transcript. */
    fun transcribeAgain(recording: Recording) =
        RecordingService.finalize(getApplication(), recording.audioFile)

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

    /** Copy recordings and transcripts picked from Drive etc. into the app. */
    fun importFiles(uris: List<Uri>) {
        if (uris.isEmpty() || _importing.value) return
        _importing.value = true
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) { importRecordings(getApplication(), uris) }
            _importing.value = false
            refreshRecordings()
            _message.value = buildList {
                if (result.imported.isNotEmpty()) {
                    add("Imported ${result.imported.size} file${if (result.imported.size == 1) "" else "s"}.")
                }
                result.skipped.forEach { (name, reason) -> add("Skipped $name: $reason.") }
            }.joinToString("\n")
        }
    }

    fun deleteRecording(recording: Recording) {
        viewModelScope.launch(Dispatchers.IO) {
            RecordingRepository.delete(recording)
            refreshRecordings()
        }
    }

    /** Renames a recording and its transcript; not while its final pass is queued or running. */
    fun renameRecording(recording: Recording, newName: String) {
        if (recording.audioFile.name in finalizing.value) {
            _message.value = "Can't rename ${recording.name} while its transcript is being refined."
            return
        }
        viewModelScope.launch {
            try {
                withContext(Dispatchers.IO) { RecordingRepository.rename(recording, newName) }
            } catch (e: IOException) {
                _message.value = "Can't rename ${recording.name}: ${e.message}"
            }
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
