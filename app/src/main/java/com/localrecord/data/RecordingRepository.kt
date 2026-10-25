package com.localrecord.data

import android.content.Context
import android.util.Log
import uniffi.whisper_engine.Segment
import uniffi.whisper_engine.WhisperEngineException
import uniffi.whisper_engine.recordingDurationMs
import uniffi.whisper_engine.transcriptFromVtt
import uniffi.whisper_engine.transcriptToVtt
import java.io.File
import java.io.IOException
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

data class Recording(
    val audioFile: File,
    val transcriptFile: File?,
    /** 0 if the file can't be read. */
    val durationMs: Long,
) {
    val name: String get() = audioFile.nameWithoutExtension
}

/**
 * Recordings live in app-external storage (user-visible via file managers,
 * removed on uninstall): one Ogg Opus .ogg (written and read by the Rust
 * engine's opus_file.rs) plus an optional WebVTT (.vtt) transcript with
 * speakers and word timestamps (format in the Rust engine's vtt.rs).
 */
object RecordingRepository {

    private const val TAG = "RecordingRepository"
    const val AUDIO_EXTENSION = "ogg"
    /** ext4 limits a file name to 255 bytes; leaves room for ".ogg"/".vtt". */
    private const val MAX_NAME_BYTES = 250

    fun recordingsDir(context: Context): File {
        val dir = context.getExternalFilesDir("recordings")
            ?: File(context.filesDir, "recordings")
        dir.mkdirs()
        return dir
    }

    /** Fresh audio file path named by start time; suffixed if that second is taken. */
    fun newRecordingFile(context: Context): File {
        val dir = recordingsDir(context)
        val stamp = SimpleDateFormat("yyyyMMdd_HHmmss", Locale.US).format(Date())
        return generateSequence(1) { it + 1 }
            .map { n -> File(dir, if (n == 1) "rec_$stamp.$AUDIO_EXTENSION" else "rec_${stamp}_$n.$AUDIO_EXTENSION") }
            .first { !it.exists() }
    }

    /** Blocking file IO. */
    fun list(context: Context): List<Recording> =
        recordingsDir(context)
            .listFiles { f -> f.extension == AUDIO_EXTENSION }
            .orEmpty()
            // By modification time, not name, so renamed recordings keep their place.
            .sortedByDescending { it.lastModified() }
            .map { audio ->
                Recording(
                    audioFile = audio,
                    transcriptFile = transcriptFileFor(audio).takeIf { it.exists() },
                    durationMs = durationMs(audio),
                )
            }

    private fun durationMs(audio: File): Long =
        try {
            recordingDurationMs(audio.path)
        } catch (e: WhisperEngineException) {
            Log.w(TAG, "unreadable ${audio.name}", e)
            0
        }

    fun transcriptFileFor(audioFile: File): File =
        File(audioFile.parentFile, "${audioFile.nameWithoutExtension}.vtt")

    fun writeTranscript(audioFile: File, segments: List<Segment>) {
        transcriptFileFor(audioFile).writeText(transcriptToVtt(segments))
    }

    /** The saved transcript; null if there is none or it can't be read. Blocking file IO. */
    fun readTranscript(audioFile: File): List<Segment>? {
        val vtt = transcriptFileFor(audioFile)
        if (!vtt.exists()) return null
        return try {
            transcriptFromVtt(vtt.readText())
        } catch (e: WhisperEngineException) {
            Log.w(TAG, "unreadable ${vtt.name}", e)
            null
        }
    }

    /**
     * Renames the audio file and its .vtt to [newName] (no extension). Returns the
     * renamed recording, or throws [IOException] with a message for the user.
     * Blocking file IO.
     */
    fun rename(recording: Recording, newName: String): Recording {
        val name = newName.trim()
        when {
            name.isEmpty() -> throw IOException("the name is empty")
            name.startsWith(".") -> throw IOException("the name can't start with a dot")
            name.any { it == '/' || it == '\u0000' } -> throw IOException("the name can't contain /")
            name.toByteArray().size > MAX_NAME_BYTES -> throw IOException("the name is too long")
        }
        val audio = recording.audioFile
        if (name == audio.nameWithoutExtension) return recording
        val newAudio = File(audio.parentFile, "$name.$AUDIO_EXTENSION")
        val oldVtt = transcriptFileFor(audio)
        val newVtt = transcriptFileFor(newAudio)
        if (newAudio.exists() || newVtt.exists()) throw IOException("a recording named $name is already here")
        if (!audio.renameTo(newAudio)) throw IOException("can't rename ${audio.name}")
        if (oldVtt.exists() && !oldVtt.renameTo(newVtt)) {
            newAudio.renameTo(audio)
            throw IOException("can't rename ${oldVtt.name}")
        }
        return recording.copy(
            audioFile = newAudio,
            transcriptFile = newVtt.takeIf { it.exists() },
        )
    }

    /** Deletes the audio file and its .vtt, including one written since [list] ran. */
    fun delete(recording: Recording) {
        recording.audioFile.delete()
        transcriptFileFor(recording.audioFile).delete()
    }
}
