package com.localrecord.data

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Log
import uniffi.whisper_engine.WhisperEngineException
import uniffi.whisper_engine.transcriptFromVtt
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.IOException

private const val TAG = "RecordingImport"
/** Larger "transcripts" are refused unread; an hour of speech is ~200 KB. */
private const val MAX_TRANSCRIPT_BYTES = 10L * 1024 * 1024

/** Picked files by display name: copied in, or skipped with the reason. */
data class ImportResult(
    val imported: List<String>,
    val skipped: List<Pair<String, String>>,
)

/**
 * Copies .wav recordings and their .vtt transcripts picked through the
 * system file picker (Drive, Downloads, ...) into the recordings dir.
 * Existing files are never overwritten; a .vtt must parse and needs a .wav
 * of the same name, picked with it or already here. Blocking IO.
 */
fun importRecordings(context: Context, uris: List<Uri>): ImportResult {
    val dir = RecordingRepository.recordingsDir(context)
    val imported = mutableListOf<String>()
    val skipped = mutableListOf<Pair<String, String>>()
    val picked = uris.map { it to displayName(context, it) }
    val (named, unnamed) = picked.partition { (_, name) -> name.baseName().isNotBlank() }
    unnamed.forEach { (_, name) -> skipped += name to "no file name" }
    val (wavs, rest) = named.partition { (_, name) -> name.lowercaseExtension() == "wav" }
    val (vtts, other) = rest.partition { (_, name) -> name.lowercaseExtension() == "vtt" }
    other.forEach { (_, name) -> skipped += name to "not a .wav or .vtt" }

    // .wav first, so a transcript picked with its recording finds it.
    for ((uri, name) in wavs) {
        val target = File(dir, "${name.baseName()}.wav")
        if (target.exists()) {
            skipped += name to "a recording with that name is already here"
            continue
        }
        try {
            copy(context, uri, target)
            imported += name
        } catch (e: IOException) {
            Log.w(TAG, "can't import $name", e)
            skipped += name to (e.message ?: "can't read it")
        } catch (e: SecurityException) {
            Log.w(TAG, "can't import $name", e)
            skipped += name to "no access"
        }
    }

    for ((uri, name) in vtts) {
        val wav = File(dir, "${name.baseName()}.wav")
        val target = RecordingRepository.transcriptFileFor(wav)
        when {
            !wav.exists() -> skipped += name to "no ${wav.name} to go with it"
            target.exists() -> skipped += name to "that recording already has a transcript"
            else -> try {
                val text = readTranscriptText(context, uri)
                transcriptFromVtt(text)
                target.writeText(text)
                imported += name
            } catch (e: WhisperEngineException) {
                skipped += name to (e.message ?: "not a transcript")
            } catch (e: IOException) {
                Log.w(TAG, "can't import $name", e)
                skipped += name to (e.message ?: "can't read it")
            } catch (e: SecurityException) {
                Log.w(TAG, "can't import $name", e)
                skipped += name to "no access"
            }
        }
    }
    return ImportResult(imported, skipped)
}

/** The provider's file name, without any path; falls back to the URI's last segment. */
private fun displayName(context: Context, uri: Uri): String {
    val name = try {
        context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
            ?.use { cursor -> if (cursor.moveToFirst()) cursor.getString(0) else null }
    } catch (e: SecurityException) {
        Log.w(TAG, "no name for $uri", e)
        null
    }
    return (name ?: uri.lastPathSegment ?: "unnamed").substringAfterLast('/')
}

private fun String.lowercaseExtension(): String = substringAfterLast('.', "").lowercase()

private fun String.baseName(): String = substringBeforeLast('.')

/** Streams [uri] into a hidden temp file, then renames it, so a failed copy never lists. */
private fun copy(context: Context, uri: Uri, target: File) {
    val temp = File(target.parentFile, ".${target.name}.part")
    try {
        val input = context.contentResolver.openInputStream(uri)
            ?: throw IOException("provider returned no data")
        input.use { source -> temp.outputStream().use { source.copyTo(it) } }
        if (!temp.renameTo(target)) throw IOException("can't save ${target.name}")
    } finally {
        temp.delete()
    }
}

private fun readTranscriptText(context: Context, uri: Uri): String {
    val input = context.contentResolver.openInputStream(uri)
        ?: throw IOException("provider returned no data")
    val bytes = ByteArrayOutputStream()
    input.use { source ->
        val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
        while (true) {
            val n = source.read(buffer)
            if (n < 0) break
            bytes.write(buffer, 0, n)
            if (bytes.size() > MAX_TRANSCRIPT_BYTES) throw IOException("too large for a transcript")
        }
    }
    return bytes.toByteArray().decodeToString()
}
