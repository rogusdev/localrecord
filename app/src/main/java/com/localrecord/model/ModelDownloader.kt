package com.localrecord.model

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import okhttp3.Request
import java.io.File
import java.io.IOException

/**
 * One-time download of the Whisper model file. After this completes the app
 * never needs the network for transcription again (Drive backup is separate
 * and optional).
 */
object ModelDownloader {

    // Quantized base.en: ~60 MB, good latency/accuracy starting point for
    // on-device English. Swap for small.en-q5_1 if accuracy disappoints.
    private const val MODEL_NAME = "ggml-base.en-q5_1.bin"
    private const val MODEL_URL =
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$MODEL_NAME"

    sealed interface State {
        data object NotDownloaded : State
        data class Downloading(val progressPercent: Int) : State
        data object Ready : State
        data class Failed(val message: String) : State
    }

    private val _state = MutableStateFlow<State>(State.NotDownloaded)
    val state: StateFlow<State> = _state.asStateFlow()

    fun modelFile(context: Context): File =
        File(File(context.filesDir, "models").apply { mkdirs() }, MODEL_NAME)

    fun refreshState(context: Context) {
        if (modelFile(context).exists()) _state.value = State.Ready
    }

    suspend fun download(context: Context) = withContext(Dispatchers.IO) {
        val target = modelFile(context)
        if (target.exists()) {
            _state.value = State.Ready
            return@withContext
        }
        val tmp = File(target.parentFile, "${target.name}.part")
        _state.value = State.Downloading(0)
        try {
            val client = OkHttpClient()
            val response = client.newCall(Request.Builder().url(MODEL_URL).build()).execute()
            response.use { resp ->
                if (!resp.isSuccessful) throw IOException("HTTP ${resp.code}")
                val body = resp.body ?: throw IOException("empty body")
                val total = body.contentLength()
                body.byteStream().use { input ->
                    tmp.outputStream().use { output ->
                        val buffer = ByteArray(256 * 1024)
                        var copied = 0L
                        while (true) {
                            val read = input.read(buffer)
                            if (read < 0) break
                            output.write(buffer, 0, read)
                            copied += read
                            if (total > 0) {
                                _state.value = State.Downloading((copied * 100 / total).toInt())
                            }
                        }
                    }
                }
            }
            if (!tmp.renameTo(target)) throw IOException("rename failed")
            _state.value = State.Ready
        } catch (e: IOException) {
            tmp.delete()
            _state.value = State.Failed(e.message ?: "download failed")
        }
    }
}
