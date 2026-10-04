package com.localrecord.data

import android.content.Context
import uniffi.whisper_engine.Segment
import uniffi.whisper_engine.Word
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

data class Recording(
    val wavFile: File,
    val transcriptFile: File?,
    val durationApproxMs: Long,
) {
    val name: String get() = wavFile.nameWithoutExtension
}

/**
 * Recordings live in app-external storage (user-visible via file managers,
 * removed on uninstall): one .wav plus an optional WebVTT (.vtt) transcript.
 * Each cue is a segment; its words carry inline start timestamps, which
 * playback reads back.
 */
object RecordingRepository {

    private const val WAV_HEADER_BYTES = 44L
    private const val BYTES_PER_MS = 32L // 16 kHz mono s16
    /** Cue timing line: "hh:mm:ss.mmm --> hh:mm:ss.mmm", optional cue settings after. */
    private val VTT_TIMING = Regex("""^(\S+) --> (\S+)""")
    /** Leading voice span naming the speaker, 1-based. */
    private val VTT_VOICE = Regex("""^<v Speaker (\d+)>""")
    /** Inline word start timestamp. */
    private val VTT_WORD_START = Regex("""<(\d+:\d{2}:\d{2}\.\d{3})>""")
    private val VTT_TIMESTAMP = Regex("""(\d+):(\d{2}):(\d{2})\.(\d{3})""")
    /** Blank line(s) between cues. */
    private val VTT_CUE_SEPARATOR = Regex("""\n\s*\n""")
    /** Line breaks inside segment text; a cue's text can't hold blank lines. */
    private val LINE_BREAK = Regex("""\s*\n\s*""")

    fun recordingsDir(context: Context): File {
        val dir = context.getExternalFilesDir("recordings")
            ?: File(context.filesDir, "recordings")
        dir.mkdirs()
        return dir
    }

    /** Fresh WAV path named by start time; suffixed if that second is taken. */
    fun newRecordingFile(context: Context): File {
        val dir = recordingsDir(context)
        val stamp = SimpleDateFormat("yyyyMMdd_HHmmss", Locale.US).format(Date())
        return generateSequence(1) { it + 1 }
            .map { n -> File(dir, if (n == 1) "rec_$stamp.wav" else "rec_${stamp}_$n.wav") }
            .first { !it.exists() }
    }

    fun list(context: Context): List<Recording> =
        recordingsDir(context)
            .listFiles { f -> f.extension == "wav" }
            .orEmpty()
            .sortedByDescending { it.name }
            .map { wav ->
                Recording(
                    wavFile = wav,
                    transcriptFile = transcriptFileFor(wav).takeIf { it.exists() },
                    durationApproxMs = (wav.length() - WAV_HEADER_BYTES).coerceAtLeast(0) / BYTES_PER_MS,
                )
            }

    fun transcriptFileFor(wavFile: File): File =
        File(wavFile.parentFile, "${wavFile.nameWithoutExtension}.vtt")

    fun writeTranscript(wavFile: File, segments: List<Segment>) {
        transcriptFileFor(wavFile).writeText(toVtt(segments))
    }

    /** The saved transcript; null if there is none. Blocking file IO. */
    fun readTranscript(wavFile: File): List<Segment>? {
        val vtt = transcriptFileFor(wavFile)
        if (!vtt.exists()) return null
        return fromVtt(vtt.readText())
    }

    fun delete(recording: Recording) {
        recording.wavFile.delete()
        recording.transcriptFile?.delete()
    }

    /**
     * One cue per segment, speaker as a voice span, every word preceded by
     * its start time: `<v Speaker 1><00:00:01.240>Hello <00:00:01.900>there.`
     * Word end times aren't stored; a word ends where the next starts.
     */
    private fun toVtt(segments: List<Segment>): String = buildString {
        append("WEBVTT\n")
        for (seg in segments) {
            val voice = seg.speaker?.let { "<v Speaker ${it + 1u}>" }.orEmpty()
            val text = if (seg.words.isEmpty()) {
                escapeVtt(seg.text.trim().replace(LINE_BREAK, " "))
            } else {
                seg.words.joinToString(" ") { "<${vttTimestamp(it.startMs)}>${escapeVtt(it.text)}" }
            }
            append("\n${vttTimestamp(seg.startMs)} --> ${vttTimestamp(seg.endMs)}\n$voice$text\n")
        }
    }

    /** Parses what [toVtt] writes; cues it can't read are skipped. */
    private fun fromVtt(vtt: String): List<Segment> =
        vtt.replace("\r\n", "\n").split(VTT_CUE_SEPARATOR).mapNotNull { block ->
            val lines = block.lines()
            val timing = lines.indexOfFirst { "-->" in it }
            if (timing < 0) return@mapNotNull null
            val (start, end) = VTT_TIMING.find(lines[timing])?.destructured ?: return@mapNotNull null
            val startMs = parseVttTimestamp(start) ?: return@mapNotNull null
            val endMs = parseVttTimestamp(end) ?: return@mapNotNull null
            var cue = lines.drop(timing + 1).joinToString(" ").trim().removeSuffix("</v>")
            val voice = VTT_VOICE.find(cue)
            if (voice != null) cue = cue.substring(voice.range.last + 1)
            val starts = VTT_WORD_START.findAll(cue).toList()
            val words = starts.mapIndexed { i, match ->
                val textEnd = starts.getOrNull(i + 1)?.range?.first ?: cue.length
                Word(
                    startMs = parseVttTimestamp(match.groupValues[1]) ?: startMs,
                    endMs = starts.getOrNull(i + 1)?.let { parseVttTimestamp(it.groupValues[1]) } ?: endMs,
                    text = unescapeVtt(cue.substring(match.range.last + 1, textEnd).trim()),
                )
            }
            Segment(
                startMs = startMs,
                endMs = endMs,
                text = if (words.isEmpty()) unescapeVtt(cue) else words.joinToString(" ") { it.text },
                words = words,
                speaker = voice?.groupValues?.get(1)?.toUIntOrNull()?.takeIf { it > 0u }?.minus(1u),
            )
        }

    /** hh:mm:ss.mmm */
    private fun vttTimestamp(ms: Long): String =
        "%02d:%02d:%02d.%03d".format(ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)

    private fun parseVttTimestamp(text: String): Long? {
        val (h, m, s, ms) = VTT_TIMESTAMP.matchEntire(text)?.destructured ?: return null
        return ((h.toLong() * 60 + m.toLong()) * 60 + s.toLong()) * 1000 + ms.toLong()
    }

    /** Cue text can't hold a raw "-->", "<" or "&". */
    private fun escapeVtt(text: String): String =
        text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")

    private fun unescapeVtt(text: String): String =
        text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}
