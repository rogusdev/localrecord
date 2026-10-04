package com.localrecord

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.runtime.getValue
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.localrecord.ui.PlaybackScreen
import com.localrecord.ui.RecorderScreen

class MainActivity : ComponentActivity() {

    private val viewModel: RecorderViewModel by viewModels()

    private val micPermission = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions()
    ) {
        // Only the mic is required. The result map holds just what was asked
        // for, so check directly: a denied POST_NOTIFICATIONS must not block.
        if (hasPermission(Manifest.permission.RECORD_AUDIO)) {
            viewModel.startRecording()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme(
                colorScheme = if (isSystemInDarkTheme()) darkColorScheme()
                else lightColorScheme()
            ) {
                val playback by viewModel.playback.collectAsStateWithLifecycle()
                val finalizing by viewModel.finalizing.collectAsStateWithLifecycle()
                val finalizeErrors by viewModel.finalizeErrors.collectAsStateWithLifecycle()
                playback?.let {
                    PlaybackScreen(
                        it,
                        finalizing = it.recording.wavFile.name in finalizing,
                        finalizeError = finalizeErrors[it.recording.wavFile.name],
                        onTranscribeAgain = { viewModel.transcribeAgain(it.recording) },
                        onBack = viewModel::closePlayback,
                    )
                }
                    ?: RecorderScreen(
                        viewModel = viewModel,
                        onRecordClick = ::onRecordClick,
                    )
            }
        }
    }

    private fun onRecordClick() {
        if (viewModel.isRecording.value) {
            viewModel.stopRecording()
            return
        }
        val needed = buildList {
            add(Manifest.permission.RECORD_AUDIO)
            add(Manifest.permission.POST_NOTIFICATIONS)
        }.filterNot(::hasPermission)
        if (needed.isEmpty()) {
            viewModel.startRecording()
        } else {
            micPermission.launch(needed.toTypedArray())
        }
    }

    private fun hasPermission(permission: String): Boolean =
        ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED
}
