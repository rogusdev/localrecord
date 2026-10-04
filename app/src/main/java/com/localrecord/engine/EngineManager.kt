package com.localrecord.engine

import android.content.Context
import android.util.Log
import com.localrecord.model.ModelDownloader
import uniffi.whisper_engine.EngineConfig
import uniffi.whisper_engine.SpeakerEncoder
import uniffi.whisper_engine.WhisperEngine
import uniffi.whisper_engine.WhisperEngineException
import uniffi.whisper_engine.initLogging

/**
 * Owns the single loaded Whisper model. Loading is expensive (model read +
 * Vulkan pipeline setup), so the engine is created once and reused across
 * recordings. All transcription logic lives in Rust; this class only manages
 * lifecycle.
 */
object EngineManager {

    private const val TAG = "EngineManager"

    @Volatile
    private var engine: WhisperEngine? = null

    @Volatile
    private var speakers: SpeakerEncoder? = null

    @Volatile
    private var finalEngine: WhisperEngine? = null

    /**
     * Load the engine if the model file is present. Returns null when the
     * model hasn't been downloaded yet; throws [WhisperEngineException] if it
     * fails to load. Blocking — call off the main thread.
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

    /**
     * The engine for final whole-recording passes (small.en), loaded on first
     * use; null when its model isn't downloaded, in which case the live
     * transcript stands. Throws [WhisperEngineException] if it fails to load.
     * Blocking.
     */
    @Synchronized
    fun finalEngineOrLoad(context: Context): WhisperEngine? {
        finalEngine?.let { return it }
        val modelFile = ModelDownloader.finalModelFile(context)
        if (!modelFile.exists()) return null
        initLogging()
        val config = EngineConfig(useGpu = true, numThreads = 4u, language = "en")
        return WhisperEngine(modelFile.absolutePath, config).also { finalEngine = it }
    }

    /**
     * The speaker voiceprint model, or null if it isn't downloaded or fails to
     * load (logged): speaker labels are optional. Blocking.
     */
    @Synchronized
    fun speakersOrLoad(context: Context): SpeakerEncoder? {
        speakers?.let { return it }
        val modelFile = ModelDownloader.speakerModelFile(context)
        if (!modelFile.exists()) return null
        initLogging()
        return try {
            SpeakerEncoder(modelFile.absolutePath).also { speakers = it }
        } catch (e: WhisperEngineException) {
            Log.e(TAG, "speaker model failed to load; no speaker labels", e)
            null
        }
    }

    /** Load whatever models are on disk so the first recording doesn't wait. Blocking. */
    fun preload(context: Context) {
        try {
            getOrLoad(context)
        } catch (e: WhisperEngineException) {
            Log.e(TAG, "model preload failed", e)
        }
        speakersOrLoad(context)
    }
}
