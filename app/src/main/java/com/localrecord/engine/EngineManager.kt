package com.localrecord.engine

import android.content.Context
import com.localrecord.model.ModelDownloader
import uniffi.whisper_engine.EngineConfig
import uniffi.whisper_engine.WhisperEngine
import uniffi.whisper_engine.initLogging

/**
 * Owns the single loaded Whisper model. Loading is expensive (model read +
 * Vulkan pipeline setup), so the engine is created once and reused across
 * recordings. All transcription logic lives in Rust; this class only manages
 * lifecycle.
 */
object EngineManager {

    @Volatile
    private var engine: WhisperEngine? = null

    val isLoaded: Boolean
        get() = engine != null

    /**
     * Load the engine if the model file is present. Returns null when the
     * model hasn't been downloaded yet. Blocking — call on Dispatchers.IO.
     */
    @Synchronized
    fun getOrLoad(context: Context): WhisperEngine? {
        engine?.let { return it }
        val modelFile = ModelDownloader.modelFile(context)
        if (!modelFile.exists()) return null
        initLogging()
        val config = EngineConfig(useGpu = true, numThreads = 4u, language = "en")
        return WhisperEngine(modelFile.absolutePath, config).also { engine = it }
    }
}
