package com.localrecord.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.localrecord.RecorderViewModel
import com.localrecord.data.Recording
import com.localrecord.model.ModelDownloader

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RecorderScreen(
    viewModel: RecorderViewModel,
    onRecordClick: () -> Unit,
    onDriveToggle: (Boolean) -> Unit,
) {
    val isRecording by viewModel.isRecording.collectAsStateWithLifecycle()
    val elapsedMs by viewModel.elapsedMs.collectAsStateWithLifecycle()
    val segments by viewModel.liveSegments.collectAsStateWithLifecycle()
    val transcribing by viewModel.transcriptionActive.collectAsStateWithLifecycle()
    val modelState by viewModel.modelState.collectAsStateWithLifecycle()
    val recordings by viewModel.recordings.collectAsStateWithLifecycle()
    val driveEnabled by viewModel.driveBackupEnabled.collectAsStateWithLifecycle()
    val wifiOnly by viewModel.wifiOnlyUpload.collectAsStateWithLifecycle()

    Scaffold(
        topBar = { TopAppBar(title = { Text("LocalRecord") }) },
        floatingActionButton = {
            FloatingActionButton(onClick = onRecordClick) {
                Icon(
                    if (isRecording) Icons.Default.Stop else Icons.Default.Mic,
                    contentDescription = if (isRecording) "Stop" else "Record",
                )
            }
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            if (modelState !is ModelDownloader.State.Ready) {
                ModelCard(modelState, onDownload = viewModel::downloadModel)
            }

            if (isRecording) {
                Text(
                    text = formatElapsed(elapsedMs) +
                        if (!transcribing) "  (recording only — no model)" else "",
                    style = MaterialTheme.typography.headlineMedium,
                )
                LiveTranscript(
                    segments = segments.map { it.text },
                    modifier = Modifier.weight(1f),
                )
            } else {
                Text("Recordings", style = MaterialTheme.typography.titleMedium)
                LazyColumn(modifier = Modifier.weight(1f)) {
                    items(recordings, key = { it.wavFile.name }) { rec ->
                        RecordingRow(rec, onDelete = { viewModel.deleteRecording(rec) })
                        HorizontalDivider()
                    }
                }
                DriveSettingsRow(
                    driveEnabled = driveEnabled,
                    wifiOnly = wifiOnly,
                    onDriveToggle = onDriveToggle,
                    onWifiOnlyToggle = viewModel::setWifiOnlyUpload,
                )
            }
        }
    }
}

@Composable
private fun ModelCard(state: ModelDownloader.State, onDownload: () -> Unit) {
    Card {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            when (state) {
                is ModelDownloader.State.NotDownloaded -> {
                    Text("Speech model not downloaded (~60 MB, one time). Recording works without it, but live transcription needs it.")
                    TextButton(onClick = onDownload) { Text("Download model") }
                }
                is ModelDownloader.State.Downloading -> {
                    Text("Downloading model… ${state.progressPercent}%")
                    LinearProgressIndicator(
                        progress = { state.progressPercent / 100f },
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
                is ModelDownloader.State.Failed -> {
                    Text("Model download failed: ${state.message}")
                    TextButton(onClick = onDownload) { Text("Retry") }
                }
                ModelDownloader.State.Ready -> Unit
            }
        }
    }
}

@Composable
private fun LiveTranscript(segments: List<String>, modifier: Modifier = Modifier) {
    val listState = rememberLazyListState()
    LaunchedEffect(segments.size) {
        if (segments.isNotEmpty()) listState.animateScrollToItem(segments.size - 1)
    }
    LazyColumn(state = listState, modifier = modifier) {
        items(segments.size) { i ->
            Text(segments[i], style = MaterialTheme.typography.bodyLarge)
            Spacer(Modifier.height(4.dp))
        }
    }
}

@Composable
private fun RecordingRow(recording: Recording, onDelete: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(recording.name, style = MaterialTheme.typography.bodyLarge)
            Text(
                formatElapsed(recording.durationApproxMs) +
                    if (recording.transcriptFile != null) " · transcribed" else "",
                style = MaterialTheme.typography.bodySmall,
            )
        }
        IconButton(onClick = onDelete) {
            Icon(Icons.Default.Delete, contentDescription = "Delete", Modifier.size(20.dp))
        }
    }
}

@Composable
private fun DriveSettingsRow(
    driveEnabled: Boolean,
    wifiOnly: Boolean,
    onDriveToggle: (Boolean) -> Unit,
    onWifiOnlyToggle: (Boolean) -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Google Drive backup")
            Switch(checked = driveEnabled, onCheckedChange = onDriveToggle)
        }
        if (driveEnabled) {
            Row(
                Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("Wi-Fi only", style = MaterialTheme.typography.bodyMedium)
                Switch(checked = wifiOnly, onCheckedChange = onWifiOnlyToggle)
            }
        }
    }
}

private fun formatElapsed(ms: Long): String {
    val totalSeconds = ms / 1000
    return "%02d:%02d".format(totalSeconds / 60, totalSeconds % 60)
}
