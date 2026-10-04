package com.localrecord.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Share
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.localrecord.Playback
import uniffi.whisper_engine.Segment

/** Where playback is in the transcript: a word, or a whole segment when it has no word timings. */
private data class Cursor(val segment: Int, val word: Int?)

/** A segment's display text and each word's character range in it. */
private class SegmentText(val text: String, val wordRanges: List<IntRange>)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PlaybackScreen(
    playback: Playback,
    finalizing: Boolean,
    onTranscribeAgain: () -> Unit,
    onBack: () -> Unit,
) {
    val player = playback.player
    val positionMs by player.positionMs.collectAsStateWithLifecycle()
    val isPlaying by player.isPlaying.collectAsStateWithLifecycle()
    val context = LocalContext.current
    BackHandler(onBack = onBack)

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(playback.recording.name) },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                    }
                },
                actions = {
                    if (finalizing) {
                        CircularProgressIndicator(Modifier.padding(12.dp).size(24.dp), strokeWidth = 2.dp)
                    } else {
                        IconButton(onClick = onTranscribeAgain) {
                            Icon(Icons.Default.Refresh, contentDescription = "Transcribe again")
                        }
                    }
                    IconButton(onClick = { shareRecording(context, playback.recording) }) {
                        Icon(Icons.Default.Share, contentDescription = "Share")
                    }
                },
            )
        },
        bottomBar = {
            PlayerControls(
                positionMs = positionMs,
                durationMs = player.durationMs,
                isPlaying = isPlaying,
                onTogglePlay = player::togglePlay,
                onSeek = player::seekTo,
            )
        },
    ) { padding ->
        Box(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 16.dp)
        ) {
            val transcript = playback.transcript
            when {
                transcript == null -> Text("No transcript for this recording")
                transcript.isEmpty() -> Text("No speech was transcribed")
                else -> Transcript(transcript, positionMs, onSeek = player::seekTo)
            }
        }
    }
}

@Composable
private fun Transcript(segments: List<Segment>, positionMs: Long, onSeek: (Long) -> Unit) {
    val cursor = cursorAt(segments, positionMs)
    val listState = rememberLazyListState()
    LaunchedEffect(cursor?.segment) {
        val index = cursor?.segment ?: return@LaunchedEffect
        if (!listState.isScrollInProgress && !listState.isFullyVisible(index)) {
            listState.animateScrollToItem(index)
        }
    }
    LazyColumn(state = listState, verticalArrangement = Arrangement.spacedBy(12.dp)) {
        itemsIndexed(segments) { i, segment ->
            Column {
                if (speakerChanged(segments, i)) SpeakerLabel(segment.speaker)
                SegmentRow(
                    segment = segment,
                    highlightedWord = cursor?.takeIf { it.segment == i }?.let { it.word ?: -1 },
                    onSeek = onSeek,
                )
            }
        }
    }
}

/**
 * @param highlightedWord index of the word to highlight, -1 for the whole
 *   segment, null for none
 */
@Composable
private fun SegmentRow(segment: Segment, highlightedWord: Int?, onSeek: (Long) -> Unit) {
    val segmentText = remember(segment) { segmentText(segment) }
    val highlight = SpanStyle(
        background = MaterialTheme.colorScheme.primaryContainer,
        color = MaterialTheme.colorScheme.onPrimaryContainer,
    )
    val text = remember(segmentText, highlightedWord, highlight) {
        highlighted(segmentText, highlightedWord, highlight)
    }
    var layout by remember { mutableStateOf<TextLayoutResult?>(null) }

    Row {
        Text(
            formatElapsed(segment.startMs),
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier
                .clickable { onSeek(segment.words.firstOrNull()?.startMs ?: segment.startMs) }
                .padding(top = 3.dp),
        )
        Spacer(Modifier.width(12.dp))
        Text(
            text,
            style = MaterialTheme.typography.bodyLarge,
            onTextLayout = { layout = it },
            modifier = Modifier.pointerInput(segment) {
                detectTapGestures { tap ->
                    val offset = layout?.getOffsetForPosition(tap) ?: return@detectTapGestures
                    val word = segmentText.wordRanges.indexOfLast { it.first <= offset }
                    onSeek(segment.words.getOrNull(word)?.startMs ?: segment.startMs)
                }
            },
        )
    }
}

@Composable
private fun PlayerControls(
    positionMs: Long,
    durationMs: Long,
    isPlaying: Boolean,
    onTogglePlay: () -> Unit,
    onSeek: (Long) -> Unit,
) {
    // While dragging, the thumb follows the finger; seek once on release.
    var dragMs by remember { mutableStateOf<Float?>(null) }
    Column(
        Modifier
            .fillMaxWidth()
            .navigationBarsPadding()
            .padding(horizontal = 16.dp, vertical = 8.dp)
    ) {
        Slider(
            value = dragMs ?: positionMs.toFloat(),
            onValueChange = { dragMs = it },
            onValueChangeFinished = {
                dragMs?.let { onSeek(it.toLong()) }
                dragMs = null
            },
            valueRange = 0f..durationMs.coerceAtLeast(1).toFloat(),
        )
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                "${formatElapsed(dragMs?.toLong() ?: positionMs)} / ${formatElapsed(durationMs)}",
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.weight(1f),
            )
            FilledIconButton(onClick = onTogglePlay) {
                Icon(
                    if (isPlaying) Icons.Default.Pause else Icons.Default.PlayArrow,
                    contentDescription = if (isPlaying) "Pause" else "Play",
                )
            }
        }
    }
}

/**
 * The word being spoken at [positionMs], or the last one before it (in a
 * pause, possibly the previous segment's last word; before any word, the
 * first). Segments without word timings are highlighted whole.
 */
private fun cursorAt(segments: List<Segment>, positionMs: Long): Cursor? {
    val segment = segments.indexOfLast { it.startMs <= positionMs }
    if (segment < 0) return null
    val words = segments[segment].words
    if (words.isEmpty()) return Cursor(segment, null)
    val word = words.indexOfLast { it.startMs <= positionMs }
    if (word >= 0) return Cursor(segment, word)
    val previous = segments.getOrNull(segment - 1)
    return if (previous != null && previous.words.isNotEmpty()) {
        Cursor(segment - 1, previous.words.lastIndex)
    } else {
        Cursor(segment, 0)
    }
}

private fun segmentText(segment: Segment): SegmentText {
    if (segment.words.isEmpty()) return SegmentText(segment.text, emptyList())
    val ranges = mutableListOf<IntRange>()
    val text = buildString {
        segment.words.forEachIndexed { i, word ->
            if (i > 0) append(' ')
            ranges += length until length + word.text.length
            append(word.text)
        }
    }
    return SegmentText(text, ranges)
}

private fun highlighted(segmentText: SegmentText, word: Int?, style: SpanStyle): AnnotatedString =
    buildAnnotatedString {
        val range = when (word) {
            null -> null
            -1 -> segmentText.text.indices
            else -> segmentText.wordRanges.getOrNull(word)
        }
        if (range == null || range.isEmpty()) {
            append(segmentText.text)
            return@buildAnnotatedString
        }
        append(segmentText.text.substring(0, range.first))
        withStyle(style) { append(segmentText.text.substring(range)) }
        append(segmentText.text.substring(range.last + 1))
    }

private fun LazyListState.isFullyVisible(index: Int): Boolean {
    val item = layoutInfo.visibleItemsInfo.firstOrNull { it.index == index } ?: return false
    return item.offset >= layoutInfo.viewportStartOffset &&
        item.offset + item.size <= layoutInfo.viewportEndOffset
}
