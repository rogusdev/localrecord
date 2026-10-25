package com.localrecord.engine

import android.content.Context
import android.util.Log
import com.localrecord.model.ModelDownloader
import uniffi.whisper_engine.SpeakerEncoder
import uniffi.whisper_engine.SpeechEngine
import uniffi.whisper_engine.WhisperEngineException
import uniffi.whisper_engine.initLogging

/**
 * Owns the loaded models. The speech model (Parakeet, ~1.2 GB in memory) is
 * loaded once and shared by live sessions and final passes, which may run
 * at the same time. All transcription logic lives in Rust; this class only
 * manages lifecycle.
 */
object EngineManager {

    private const val TAG = "EngineManager"

    @Volatile
    private var engine: SpeechEngine? = null

    @Volatile
    private var speakers: SpeakerEncoder? = null

    /**
     * Load the speech engine if its model is present. Returns null when the
     * model hasn't been downloaded yet; throws [WhisperEngineException] if it
     * fails to load. Blocking — call off the main thread.
     */
    @Synchronized
    fun getOrLoad(context: Context): SpeechEngine? {
        engine?.let { return it }
        if (!ModelDownloader.speechModelReady(context)) return null
        initLogging()
        val modelDir = ModelDownloader.speechModelDir(context)
        return SpeechEngine(modelDir.absolutePath).also { engine = it }
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
