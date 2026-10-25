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
 * One-time download of the model files: Parakeet for transcription (live
 * and the on-demand final pass) and a speaker-embedding model for speaker
 * labels. After this completes the app never needs the network again.
 */
object ModelDownloader {

    /** Directory under the models dir holding the speech model's files. */
    private const val PARAKEET_DIR = "parakeet-tdt-0.6b-v2-int8"
    private const val PARAKEET_URL =
        "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/" +
            "1ab9323565ddb038682214b292f588070a538ce2/"

    /**
     * URLs are pinned to a revision so the SHA-256 stays valid. [name] is a
     * path under the models dir.
     */
    private class ModelFile(val name: String, val url: String, val sha256: String, val bytes: Long)

    private fun parakeetFile(file: String, sha256: String, bytes: Long) =
        ModelFile("$PARAKEET_DIR/$file", PARAKEET_URL + file, sha256, bytes)

    // NVIDIA's Parakeet TDT 0.6B v2 (English, CC-BY-4.0) as sherpa-onnx's
    // int8 export; the four files make one model.
    private val PARAKEET = listOf(
        parakeetFile(
            "encoder.int8.onnx",
            "a32b12d17bbbc309d0686fbbcc2987b5e9b8333a7da83fa6b089f0a2acd651ab",
            652_184_296,
        ),
        parakeetFile(
            "decoder.int8.onnx",
            "b6bb64963457237b900e496ee9994b59294526439fbcc1fecf705b31a15c6b4e",
            7_257_753,
        ),
        parakeetFile(
            "joiner.int8.onnx",
            "7946164367946e7f9f29a122407c3252b680dbae9a51343eb2488d057c3c43d2",
            1_739_080,
        ),
        parakeetFile(
            "tokens.txt",
            "ec182b70dd42113aff6c5372c75cac58c952443eb22322f57bbd7f53977d497d",
            9_384,
        ),
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

    private val ALL = PARAKEET + SPEAKER

    /** Model files earlier builds downloaded; deleted to reclaim space. */
    private val OBSOLETE = listOf(
        "3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx",
        "ggml-base.en-q5_1.bin",
        "ggml-small.en-q5_1.bin",
    )

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

    /** The speech model's directory, for [uniffi.whisper_engine.SpeechEngine]. */
    fun speechModelDir(context: Context): File = File(modelsDir(context), PARAKEET_DIR)

    fun speechModelReady(context: Context): Boolean = PARAKEET.all { fileFor(context, it).exists() }

    fun speakerModelFile(context: Context): File = fileFor(context, SPEAKER)

    private fun modelsDir(context: Context): File = File(context.filesDir, "models")

    private fun fileFor(context: Context, model: ModelFile): File =
        File(modelsDir(context), model.name).apply { parentFile?.mkdirs() }

    private fun missing(context: Context): List<ModelFile> =
        ALL.filterNot { fileFor(context, it).exists() }

    fun refreshState(context: Context) {
        OBSOLETE.forEach { File(modelsDir(context), it).delete() }
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
