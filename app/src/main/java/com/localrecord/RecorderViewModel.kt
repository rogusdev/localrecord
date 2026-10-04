package com.localrecord

import android.app.Application
import android.util.Log
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.google.android.gms.common.api.ApiException
import com.google.android.gms.common.api.CommonStatusCodes
import com.localrecord.audio.RecordingService
import com.localrecord.audio.RecordingState
import com.localrecord.data.Recording
import com.localrecord.data.RecordingRepository
import com.localrecord.drive.DriveAuth
import com.localrecord.drive.DriveUploadWorker
import com.localrecord.engine.EngineManager
import com.localrecord.model.ModelDownloader
import com.localrecord.playback.Player
import com.localrecord.settings.Settings
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

    private val _driveBackupEnabled =
        MutableStateFlow(Settings.driveBackupEnabled(app))
    val driveBackupEnabled: StateFlow<Boolean> = _driveBackupEnabled.asStateFlow()

    private val _wifiOnlyUpload = MutableStateFlow(Settings.wifiOnlyUpload(app))
    val wifiOnlyUpload: StateFlow<Boolean> = _wifiOnlyUpload.asStateFlow()

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

    fun setWifiOnlyUpload(wifiOnly: Boolean) {
        Settings.setWifiOnlyUpload(getApplication(), wifiOnly)
        _wifiOnlyUpload.value = wifiOnly
        // Queued work keeps the network constraint it was enqueued with.
        if (_driveBackupEnabled.value) DriveUploadWorker.reschedule(getApplication())
    }

    /**
     * Toggle Drive backup. Enabling may require user consent: the result's
     * pendingIntent is surfaced through [onAuthorizationNeeded] for the
     * Activity to launch.
     */
    fun setDriveBackupEnabled(
        enabled: Boolean,
        onAuthorizationNeeded: (android.app.PendingIntent) -> Unit,
    ) {
        val app = getApplication<Application>()
        if (!enabled) {
            Settings.setDriveBackupEnabled(app, false)
            _driveBackupEnabled.value = false
            return
        }
        viewModelScope.launch {
            val result = try {
                DriveAuth.authorize(app)
            } catch (e: ApiException) {
                // DEVELOPER_ERROR: no OAuth client for this package + signing
                // key in Google Cloud (see README)
                Log.e(TAG, "Drive authorization failed", e)
                val status = CommonStatusCodes.getStatusCodeString(e.statusCode)
                _message.value = "Drive authorization failed ($status). See README: Google Drive backup."
                return@launch
            }
            if (result.hasResolution()) {
                result.pendingIntent?.let(onAuthorizationNeeded)
            } else {
                confirmDriveEnabled()
            }
        }
    }

    /** The consent UI was dismissed or failed. */
    fun onDriveConsentDenied() {
        _message.value = "Drive backup not enabled: authorization was not granted"
    }

    /** Called after the consent UI completes successfully. */
    fun confirmDriveEnabled() {
        val app = getApplication<Application>()
        Settings.setDriveBackupEnabled(app, true)
        _driveBackupEnabled.value = true
        DriveUploadWorker.enqueue(app)
    }

    private fun refreshRecordings() {
        viewModelScope.launch {
            _recordings.value = withContext(Dispatchers.IO) {
                RecordingRepository.list(getApplication())
            }
        }
    }
}
