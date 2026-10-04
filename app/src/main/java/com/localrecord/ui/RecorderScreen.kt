package com.localrecord.ui

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
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
import androidx.compose.material.icons.filled.FileOpen
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.Share
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.localrecord.R
import com.localrecord.RecorderViewModel
import com.localrecord.data.Recording
import com.localrecord.model.ModelDownloader
import uniffi.whisper_engine.Segment

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RecorderScreen(
    viewModel: RecorderViewModel,
    onRecordClick: () -> Unit,
) {
    val isRecording by viewModel.isRecording.collectAsStateWithLifecycle()
    val elapsedMs by viewModel.elapsedMs.collectAsStateWithLifecycle()
    val segments by viewModel.liveSegments.collectAsStateWithLifecycle()
    val tentative by viewModel.tentativeSegments.collectAsStateWithLifecycle()
    val transcribing by viewModel.transcriptionActive.collectAsStateWithLifecycle()
    val transcriptionError by viewModel.transcriptionError.collectAsStateWithLifecycle()
    val modelState by viewModel.modelState.collectAsStateWithLifecycle()
    val recordings by viewModel.recordings.collectAsStateWithLifecycle()
    val finalizing by viewModel.finalizing.collectAsStateWithLifecycle()
    val finalizeErrors by viewModel.finalizeErrors.collectAsStateWithLifecycle()
    val message by viewModel.message.collectAsStateWithLifecycle()
    val importing by viewModel.importing.collectAsStateWithLifecycle()
    // Any type: providers label .vtt inconsistently; the import checks extensions.
    val pickFiles = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenMultipleDocuments(),
        viewModel::importFiles,
    )

    val snackbarHostState = remember { SnackbarHostState() }
    LaunchedEffect(message) {
        message?.let {
            snackbarHostState.showSnackbar(it, withDismissAction = true, duration = SnackbarDuration.Long)
            viewModel.messageShown()
        }
    }

    var pendingDelete by remember { mutableStateOf<Recording?>(null) }
    pendingDelete?.let { recording ->
        ConfirmDeleteDialog(
            recording,
            onConfirm = {
                viewModel.deleteRecording(recording)
                pendingDelete = null
            },
            onDismiss = { pendingDelete = null },
        )
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.app_name)) },
                actions = {
                    IconButton(onClick = { pickFiles.launch(arrayOf("*/*")) }, enabled = !importing) {
                        Icon(Icons.Default.FileOpen, contentDescription = "Import recordings")
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
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
                        if (!transcribing) "  (recording only)" else "",
                    style = MaterialTheme.typography.headlineMedium,
                )
                if (!transcribing) {
                    Text(
                        transcriptionError?.let { "Transcription failed: $it" }
                            ?: "No speech model downloaded",
                        color = if (transcriptionError != null) MaterialTheme.colorScheme.error
                        else Color.Unspecified,
                        style = MaterialTheme.typography.bodyMedium,
                    )
                }
                LiveTranscript(segments, tentative, modifier = Modifier.weight(1f))
            } else {
                Text("Recordings", style = MaterialTheme.typography.titleMedium)
                if (importing) {
                    Text("Importing…", style = MaterialTheme.typography.bodySmall)
                    LinearProgressIndicator(Modifier.fillMaxWidth())
                }
                LazyColumn(modifier = Modifier.weight(1f)) {
                    items(recordings, key = { it.wavFile.name }) { rec ->
                        RecordingRow(
                            rec,
                            finalizing = rec.wavFile.name in finalizing,
                            finalizeFailed = rec.wavFile.name in finalizeErrors,
                            onOpen = { viewModel.openPlayback(rec) },
                            onDelete = { pendingDelete = rec },
                        )
                        HorizontalDivider()
                    }
                }
            }
        }
    }
}

@Composable
private fun ConfirmDeleteDialog(recording: Recording, onConfirm: () -> Unit, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Delete recording?") },
        text = {
            Text("${recording.name} (${formatElapsed(recording.durationApproxMs)}) and its transcript will be deleted from this phone. Copies you shared elsewhere are not affected.")
        },
        confirmButton = {
            TextButton(onClick = onConfirm) {
                Text("Delete", color = MaterialTheme.colorScheme.error)
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun ModelCard(state: ModelDownloader.State, onDownload: () -> Unit) {
    Card {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            when (state) {
                is ModelDownloader.State.NotDownloaded -> {
                    Text("Speech models not downloaded (~${state.megabytes} MB, one time). Recording works without them; live transcription, speaker labels and the accurate final transcript need them.")
                    TextButton(onClick = onDownload) { Text("Download models") }
                }
                is ModelDownloader.State.Downloading -> {
                    Text("Downloading models… ${state.progressPercent}%")
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

/** Final segments, then the latest pass's not-yet-final text in grey. */
@Composable
private fun LiveTranscript(
    segments: List<Segment>,
    tentative: List<Segment>,
    modifier: Modifier = Modifier,
) {
    val listState = rememberLazyListState()
    val pending = tentative.joinToString(" ") { it.text }
    LaunchedEffect(segments.size, pending) {
        val last = segments.size - if (pending.isEmpty()) 1 else 0
        if (last >= 0) listState.animateScrollToItem(last)
    }
    LazyColumn(state = listState, modifier = modifier) {
        items(segments.size) { i ->
            if (speakerChanged(segments, i)) SpeakerLabel(segments[i].speaker)
            Text(segments[i].text, style = MaterialTheme.typography.bodyLarge)
            Spacer(Modifier.height(4.dp))
        }
        if (pending.isNotEmpty()) {
            item {
                Text(
                    pending,
                    style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.6f),
                )
            }
        }
    }
}

/** Whether segment [i] starts a new speaker's turn (always for a labelled first segment). */
internal fun speakerChanged(segments: List<Segment>, i: Int): Boolean =
    segments[i].speaker != null && segments[i].speaker != segments.getOrNull(i - 1)?.speaker

@Composable
internal fun SpeakerLabel(speaker: UInt?, modifier: Modifier = Modifier) {
    if (speaker == null) return
    Text(
        speakerName(speaker),
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.primary,
        modifier = modifier.padding(top = 8.dp, bottom = 2.dp),
    )
}

/** Display name for a 0-based speaker index. */
internal fun speakerName(speaker: UInt): String = "Speaker ${speaker + 1u}"

@Composable
private fun RecordingRow(
    recording: Recording,
    finalizing: Boolean,
    finalizeFailed: Boolean,
    onOpen: () -> Unit,
    onDelete: () -> Unit,
) {
    val context = LocalContext.current
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onOpen)
            .padding(vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(recording.name, style = MaterialTheme.typography.bodyLarge)
            Text(
                formatElapsed(recording.durationApproxMs) + when {
                    finalizing -> " · finalizing transcript…"
                    finalizeFailed -> " · transcription failed"
                    recording.transcriptFile != null -> " · transcribed"
                    else -> ""
                },
                style = MaterialTheme.typography.bodySmall,
            )
        }
        IconButton(onClick = { shareRecording(context, recording) }) {
            Icon(Icons.Default.Share, contentDescription = "Share", Modifier.size(20.dp))
        }
        IconButton(onClick = onDelete) {
            Icon(Icons.Default.Delete, contentDescription = "Delete", Modifier.size(20.dp))
        }
    }
}

internal fun formatElapsed(ms: Long): String {
    val totalSeconds = ms / 1000
    return "%02d:%02d".format(totalSeconds / 60, totalSeconds % 60)
}
