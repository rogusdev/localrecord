package com.localrecord.data

import android.content.Context
import android.util.Log
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject
import uniffi.whisper_engine.Segment
import uniffi.whisper_engine.Word
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

data class Recording(
    val wavFile: File,
    val transcriptFile: File?,
    /** Word/speaker timings for playback; absent for older recordings. */
    val timingsFile: File?,
    val durationApproxMs: Long,
) {
    val name: String get() = wavFile.nameWithoutExtension
}

/**
 * Recordings live in app-external storage (user-visible via file managers,
 * removed on uninstall): one .wav plus optional siblings — a readable .txt
 * transcript and a .json copy with segment/word timings for playback.
 */
object RecordingRepository {

    private const val TAG = "RecordingRepository"
    private const val WAV_HEADER_BYTES = 44L
    private const val BYTES_PER_MS = 32L // 16 kHz mono s16
    /** A .txt transcript line: "[mm:ss → mm:ss] Speaker n: text", speaker optional. */
    private val TXT_LINE = Regex("""^\[(\d+):(\d{2}) → (\d+):(\d{2})] (?:Speaker (\d+): )?(.*)$""")

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
                    timingsFile = timingsFileFor(wav).takeIf { it.exists() },
                    durationApproxMs = (wav.length() - WAV_HEADER_BYTES).coerceAtLeast(0) / BYTES_PER_MS,
                )
            }

    fun transcriptFileFor(wavFile: File): File =
        File(wavFile.parentFile, "${wavFile.nameWithoutExtension}.txt")

    private fun timingsFileFor(wavFile: File): File =
        File(wavFile.parentFile, "${wavFile.nameWithoutExtension}.json")

    fun writeTranscript(wavFile: File, segments: List<Segment>) {
        val text = segments.joinToString("\n") { seg ->
            val speaker = seg.speaker?.let { "Speaker ${it + 1u}: " }.orEmpty()
            "[${formatMs(seg.startMs)} → ${formatMs(seg.endMs)}] $speaker${seg.text}"
        }
        transcriptFileFor(wavFile).writeText(text)
        timingsFileFor(wavFile).writeText(toJson(segments).toString())
    }

    /**
     * The saved transcript: word timings from the .json, else whole-second
     * segment times from the .txt (recordings made before word timings).
     * Null if there is neither. Blocking file IO.
     */
    fun readTranscript(wavFile: File): List<Segment>? {
        val json = timingsFileFor(wavFile)
        if (json.exists()) {
            try {
                return fromJson(JSONObject(json.readText()))
            } catch (e: JSONException) {
                Log.w(TAG, "unreadable ${json.name}; falling back to .txt", e)
            }
        }
        val txt = transcriptFileFor(wavFile)
        if (!txt.exists()) return null
        return txt.readLines().mapNotNull { line ->
            TXT_LINE.matchEntire(line)?.destructured?.let { (m0, s0, m1, s1, speaker, text) ->
                Segment(
                    startMs = (m0.toLong() * 60 + s0.toLong()) * 1000,
                    endMs = (m1.toLong() * 60 + s1.toLong()) * 1000,
                    text = text,
                    words = emptyList(),
                    speaker = speaker.toUIntOrNull()?.takeIf { it > 0u }?.minus(1u),
                )
            }
        }
    }

    fun delete(recording: Recording) {
        recording.wavFile.delete()
        recording.transcriptFile?.delete()
        timingsFileFor(recording.wavFile).delete()
    }

    private fun toJson(segments: List<Segment>): JSONObject {
        fun span(startMs: Long, endMs: Long, text: String) = JSONObject()
            .put("start_ms", startMs)
            .put("end_ms", endMs)
            .put("text", text)
        val array = JSONArray()
        for (seg in segments) {
            val words = JSONArray()
            seg.words.forEach { words.put(span(it.startMs, it.endMs, it.text)) }
            val json = span(seg.startMs, seg.endMs, seg.text).put("words", words)
            seg.speaker?.let { json.put("speaker", it.toLong()) }
            array.put(json)
        }
        return JSONObject().put("segments", array)
    }

    private fun fromJson(json: JSONObject): List<Segment> {
        val segments = json.getJSONArray("segments")
        return (0 until segments.length()).map { i ->
            val seg = segments.getJSONObject(i)
            val words = seg.getJSONArray("words")
            Segment(
                startMs = seg.getLong("start_ms"),
                endMs = seg.getLong("end_ms"),
                text = seg.getString("text"),
                words = (0 until words.length()).map { j ->
                    val word = words.getJSONObject(j)
                    Word(word.getLong("start_ms"), word.getLong("end_ms"), word.getString("text"))
                },
                speaker = if (seg.has("speaker")) seg.getLong("speaker").toUInt() else null,
            )
        }
    }

    private fun formatMs(ms: Long): String {
        val totalSeconds = ms / 1000
        return "%02d:%02d".format(totalSeconds / 60, totalSeconds % 60)
    }
}
