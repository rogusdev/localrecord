package com.localrecord

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.localrecord.audio.RecordingService
import com.localrecord.audio.RecordingState
import com.localrecord.data.Recording
import com.localrecord.data.RecordingRepository
import com.localrecord.drive.DriveAuth
import com.localrecord.drive.DriveUploadWorker
import com.localrecord.engine.EngineManager
import com.localrecord.model.ModelDownloader
import com.localrecord.settings.Settings
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class RecorderViewModel(app: Application) : AndroidViewModel(app) {

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
            val result = runCatching { DriveAuth.authorize(app) }.getOrNull()
            when {
                result == null -> Unit // Play services error; leave disabled
                result.hasResolution() -> result.pendingIntent?.let(onAuthorizationNeeded)
                else -> confirmDriveEnabled()
            }
        }
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
