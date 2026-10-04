package com.localrecord.model

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import okhttp3.Request
import java.io.File
import java.io.IOException
import java.security.MessageDigest

/**
 * One-time download of the model files: Whisper for transcription and a
 * speaker-embedding model for live speaker labels. After this completes the
 * app never needs the network again.
 */
object ModelDownloader {

    /** URLs are pinned to a revision so the SHA-256 stays valid. */
    private class ModelFile(val name: String, val url: String, val sha256: String, val bytes: Long)

    // Quantized base.en: ~60 MB, good latency/accuracy starting point for
    // on-device English. Swap for small.en-q5_1 if accuracy disappoints.
    private val WHISPER = ModelFile(
        name = "ggml-base.en-q5_1.bin",
        url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/" +
            "f281eb45af861ab5e5297d23694b7d46e090c02c/ggml-base.en-q5_1.bin",
        sha256 = "4baf70dd0d7c4247ba2b81fafd9c01005ac77c2f9ef064e00dcf195d0e2fdd2f",
        bytes = 59_721_011,
    )

    // 3D-Speaker CAM++ "zh_en common advanced" voiceprints (3D-Speaker,
    // Apache-2.0), from sherpa-onnx's model mirror. Beat 10 other sherpa
    // speaker models on far-field meeting audio (AMI) at the same size/speed.
    private val SPEAKER = ModelFile(
        name = "3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
        url = "https://huggingface.co/csukuangfj/speaker-embedding-models/resolve/" +
            "0743f301363dec56491a490f6d6cbc9d67f9a3bf/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
        sha256 = "aa3cfc16963a10586a9393f5035d6d6b57e98d358b347f80c2a30bf4f00ceba2",
        bytes = 28_281_164,
    )

    private val ALL = listOf(WHISPER, SPEAKER)

    /** Model files earlier builds downloaded; deleted to reclaim space. */
    private val OBSOLETE = listOf("3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx")

    private const val BYTES_PER_MB = 1_000_000L

    sealed interface State {
        data class NotDownloaded(val megabytes: Long) : State
        data class Downloading(val progressPercent: Int) : State
        data object Ready : State
        data class Failed(val message: String) : State
    }

    private val _state = MutableStateFlow<State>(State.NotDownloaded(0))
    val state: StateFlow<State> = _state.asStateFlow()

    /** Serializes downloads: two writers on the same .part file corrupt the model. */
    private val downloadLock = Mutex()

    fun modelFile(context: Context): File = fileFor(context, WHISPER)

    fun speakerModelFile(context: Context): File = fileFor(context, SPEAKER)

    private fun fileFor(context: Context, model: ModelFile): File =
        File(File(context.filesDir, "models").apply { mkdirs() }, model.name)

    private fun missing(context: Context): List<ModelFile> =
        ALL.filterNot { fileFor(context, it).exists() }

    fun refreshState(context: Context) {
        OBSOLETE.forEach { File(File(context.filesDir, "models"), it).delete() }
        val missing = missing(context)
        _state.value = if (missing.isEmpty()) State.Ready
        else State.NotDownloaded(missing.sumOf { it.bytes } / BYTES_PER_MB)
    }

    suspend fun download(context: Context) = downloadLock.withLock { downloadLocked(context) }

    private suspend fun downloadLocked(context: Context) = withContext(Dispatchers.IO) {
        val missing = missing(context)
        val total = missing.sumOf { it.bytes }
        var done = 0L
        _state.value = State.Downloading(0)
        try {
            for (model in missing) {
                fetch(model, fileFor(context, model)) { copied ->
                    _state.value = State.Downloading(((done + copied) * 100 / total).toInt())
                }
                done += model.bytes
            }
            _state.value = State.Ready
        } catch (e: IOException) {
            _state.value = State.Failed(e.message ?: "download failed")
        }
    }

    /** Download to a .part file, verify, then move into place. */
    private fun fetch(model: ModelFile, target: File, onProgress: (copiedBytes: Long) -> Unit) {
        val tmp = File(target.parentFile, "${target.name}.part")
        try {
            val response = OkHttpClient().newCall(Request.Builder().url(model.url).build()).execute()
            response.use { resp ->
                if (!resp.isSuccessful) throw IOException("${model.name}: HTTP ${resp.code}")
                val body = resp.body ?: throw IOException("${model.name}: empty body")
                val sha256 = MessageDigest.getInstance("SHA-256")
                body.byteStream().use { input ->
                    tmp.outputStream().use { output ->
                        val buffer = ByteArray(256 * 1024)
                        var copied = 0L
                        while (true) {
                            val read = input.read(buffer)
                            if (read < 0) break
                            output.write(buffer, 0, read)
                            sha256.update(buffer, 0, read)
                            copied += read
                            onProgress(copied)
                        }
                    }
                }
                val actual = sha256.digest().joinToString("") { "%02x".format(it) }
                if (actual != model.sha256) throw IOException("${model.name}: checksum mismatch")
            }
            if (!tmp.renameTo(target)) throw IOException("${model.name}: rename failed")
        } finally {
            tmp.delete()
        }
    }
}
