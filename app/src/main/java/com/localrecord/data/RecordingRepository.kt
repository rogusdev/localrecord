package com.localrecord.data

import android.content.Context
import android.util.Log
import uniffi.whisper_engine.Segment
import uniffi.whisper_engine.WhisperEngineException
import uniffi.whisper_engine.transcriptFromVtt
import uniffi.whisper_engine.transcriptToVtt
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
 * removed on uninstall): one .wav plus an optional WebVTT (.vtt) transcript
 * with speakers and word timestamps (format in the Rust engine's vtt.rs).
 */
object RecordingRepository {

    private const val TAG = "RecordingRepository"
    private const val WAV_HEADER_BYTES = 44L
    private const val BYTES_PER_MS = 32L // 16 kHz mono s16

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
        transcriptFileFor(wavFile).writeText(transcriptToVtt(segments))
    }

    /** The saved transcript; null if there is none or it can't be read. Blocking file IO. */
    fun readTranscript(wavFile: File): List<Segment>? {
        val vtt = transcriptFileFor(wavFile)
        if (!vtt.exists()) return null
        return try {
            transcriptFromVtt(vtt.readText())
        } catch (e: WhisperEngineException) {
            Log.w(TAG, "unreadable ${vtt.name}", e)
            null
        }
    }

    fun delete(recording: Recording) {
        recording.wavFile.delete()
        recording.transcriptFile?.delete()
    }
}
