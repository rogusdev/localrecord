package com.localrecord.audio

import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.os.Process
import android.os.SystemClock
import android.util.Log
import android.widget.Toast
import com.localrecord.MainActivity
import com.localrecord.R
import com.localrecord.data.RecordingRepository
import com.localrecord.engine.EngineManager
import com.localrecord.model.ModelDownloader
import uniffi.whisper_engine.LiveSession
import uniffi.whisper_engine.Segment
import uniffi.whisper_engine.WhisperEngineException
import java.io.File
import java.io.IOException
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import kotlin.concurrent.thread

/**
 * Foreground service that captures mic audio, writes it to a WAV file, and
 * feeds the same buffers to the Rust live-transcription session. Kotlin only
 * marshals buffers — all transcription logic is on the Rust side.
 *
 * Capture format is 16 kHz mono PCM16: exactly what Whisper consumes, so no
 * resampling and one shared buffer for both the file and the engine.
 *
 * After a recording stops, its live transcript is saved as a draft. The
 * whole-recording pass (more accurate, but heavy on the GPU/CPU) runs only on
 * request: [finalize] queues it, and it replaces the draft when done. The
 * service stays in the foreground until capture and the queue are both done.
 */
class RecordingService : Service() {

    companion object {
        const val ACTION_START = "com.localrecord.action.START_RECORDING"
        const val ACTION_STOP = "com.localrecord.action.STOP_RECORDING"
        const val ACTION_FINALIZE = "com.localrecord.action.FINALIZE_TRANSCRIPT"
        private const val EXTRA_WAV_PATH = "wav_path"

        const val SAMPLE_RATE_HZ = 16_000
        private const val TAG = "RecordingService"
        private const val CHANNEL_ID = "recording"
        private const val NOTIFICATION_ID = 1
        /** 100 ms of 16 kHz mono s16 audio per read. */
        private const val READ_BUFFER_BYTES = SAMPLE_RATE_HZ / 10 * 2
        /** Pull newly stable segments every 5 reads (500 ms). */
        private const val DRAIN_EVERY_READS = 5
        /** Shown when a recording stops; the final pass is started by hand. */
        private const val REFINE_HINT =
            "Recording saved. To refine its transcript, open it and tap Refine (↻). " +
                "That uses a lot of the phone's processing while it runs."
        /** Upper bound on one final pass holding the CPU awake (6 h). */
        private const val FINAL_PASS_WAKE_LOCK_MS = 6 * 60 * 60 * 1000L

        fun start(context: Context) {
            context.startForegroundService(
                Intent(context, RecordingService::class.java).setAction(ACTION_START)
            )
        }

        fun stop(context: Context) {
            context.startService(
                Intent(context, RecordingService::class.java).setAction(ACTION_STOP)
            )
        }

        /** Queue a final transcription pass for an existing recording. */
        fun finalize(context: Context, wavFile: File) {
            context.startForegroundService(
                Intent(context, RecordingService::class.java)
                    .setAction(ACTION_FINALIZE)
                    .putExtra(EXTRA_WAV_PATH, wavFile.path)
            )
        }

        /** Foreground type for the final pass: no mic, just CPU/GPU work. */
        private val PROCESSING_TYPE =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.VANILLA_ICE_CREAM) {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROCESSING
            } else {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
            }
    }

    @Volatile
    private var capturing = false
    /** Alive from start until the file and live transcript are saved. Main thread only. */
    private var captureThread: Thread? = null

    /**
     * Final passes run one at a time, off the capture thread, at background
     * priority (nice 10) so the phone stays usable; whisper.cpp's worker
     * threads inherit it.
     */
    private val finalizer: ExecutorService = Executors.newSingleThreadExecutor { task ->
        Thread({
            Process.setThreadPriority(Process.THREAD_PRIORITY_BACKGROUND)
            task.run()
        }, "final-pass")
    }
    /** Queued or running final passes. Main thread only. */
    private var pendingFinals = 0
    private val mainHandler = Handler(Looper.getMainLooper())

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val active = captureThread?.isAlive == true
        when (intent?.action) {
            ACTION_START -> if (!active) startRecording()
            // The capture thread saves the recording and the live draft.
            ACTION_STOP -> if (active) capturing = false else stopIfIdle()
            ACTION_FINALIZE -> {
                val path = intent.getStringExtra(EXTRA_WAV_PATH)
                if (path != null) enqueueFinal(File(path)) else stopIfIdle()
            }
        }
        return START_NOT_STICKY
    }

    private fun startRecording() {
        capturing = true
        updateForeground()
        val wavFile = RecordingRepository.newRecordingFile(this)
        // Model load (first use) and transcription flush block; keep them off
        // the main thread.
        captureThread = thread(name = "audio-capture") { captureLoop(wavFile) }
    }

    private fun captureLoop(wavFile: File) {
        // Engine is null when the model isn't downloaded yet: record anyway,
        // just without live transcription. A load failure is shown in the UI.
        var loadError: String? = null
        val session = try {
            EngineManager.getOrLoad(this)?.createLiveSession(EngineManager.speakersOrLoad(this))
        } catch (e: WhisperEngineException) {
            Log.e(TAG, "live transcription unavailable", e)
            loadError = e.message
            null
        }
        RecordingState.onRecordingStarted(transcribing = session != null, error = loadError)

        val transcript = mutableListOf<Segment>()
        val publish = { segments: List<Segment> ->
            transcript += segments
            RecordingState.appendSegments(segments)
        }
        captureAudio(wavFile, session, publish)

        // Flush the transcription tail and persist the transcript.
        if (session != null) {
            try {
                publish(session.finish())
            } catch (e: WhisperEngineException) {
                Log.e(TAG, "session.finish failed", e)
            }
            session.close()
        }
        if (transcript.isNotEmpty()) {
            RecordingRepository.writeTranscript(wavFile, transcript)
        }
        RecordingState.onRecordingStopped()
        val thisThread = Thread.currentThread()
        mainHandler.post {
            // A new recording may already have started its own capture thread.
            if (captureThread === thisThread) captureThread = null
            if (ModelDownloader.finalModelFile(this).exists()) {
                Toast.makeText(this, REFINE_HINT, Toast.LENGTH_LONG).show()
            }
            stopIfIdle()
        }
    }

    /** Main thread. */
    private fun enqueueFinal(wavFile: File) {
        pendingFinals++
        RecordingState.onFinalizing(wavFile, true)
        updateForeground()
        finalizer.execute {
            // A foreground service doesn't keep the CPU awake; without this
            // the pass stalls whenever the screen is off.
            val wakeLock = getSystemService(PowerManager::class.java)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "localrecord:final-pass")
            wakeLock.acquire(FINAL_PASS_WAKE_LOCK_MS)
            try {
                finalizeTranscript(wavFile)
            } finally {
                if (wakeLock.isHeld) wakeLock.release()
            }
            mainHandler.post {
                pendingFinals--
                RecordingState.onFinalizing(wavFile, false)
                updateForeground()
                stopIfIdle()
            }
        }
    }

    /** Replace the live draft with a whole-recording pass. Blocking; finalizer thread. */
    private fun finalizeTranscript(wavFile: File) {
        try {
            val engine = EngineManager.finalEngineOrLoad(this)
            if (engine == null) {
                RecordingState.onFinalizeFailed(wavFile, "speech models aren't downloaded")
                return
            }
            val segments = engine.transcribeWav(wavFile.path, EngineManager.speakersOrLoad(this))
            // An empty final pass over audio the draft found speech in is
            // more likely a failure than the truth; keep the draft then.
            if (segments.isNotEmpty() || !RecordingRepository.transcriptFileFor(wavFile).exists()) {
                RecordingRepository.writeTranscript(wavFile, segments)
            }
        } catch (e: WhisperEngineException) {
            Log.e(TAG, "final pass failed for ${wavFile.name}; keeping the live transcript", e)
            RecordingState.onFinalizeFailed(wavFile, e.message ?: "transcription failed")
        } catch (e: IOException) {
            Log.e(TAG, "can't save the transcript of ${wavFile.name}", e)
            RecordingState.onFinalizeFailed(wavFile, "can't save the transcript: ${e.message}")
        }
    }

    /** Main thread. Foreground types and notification text for the current work. */
    private fun updateForeground() {
        val recording = captureThread?.isAlive == true || capturing
        if (!recording && pendingFinals == 0) return
        var types = 0
        if (recording) types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
        if (pendingFinals > 0) types = types or PROCESSING_TYPE
        startForeground(NOTIFICATION_ID, buildNotification(recording), types)
    }

    /** Main thread. */
    private fun stopIfIdle() {
        if (captureThread?.isAlive != true && !capturing && pendingFinals == 0) {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
    }

    @SuppressLint("MissingPermission") // UI checks RECORD_AUDIO before starting
    private fun captureAudio(
        wavFile: File,
        session: LiveSession?,
        publish: (List<Segment>) -> Unit,
    ) {
        val minBuffer = AudioRecord.getMinBufferSize(
            SAMPLE_RATE_HZ, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT
        )
        val recorder = AudioRecord(
            MediaRecorder.AudioSource.MIC,
            SAMPLE_RATE_HZ,
            AudioFormat.CHANNEL_IN_MONO,
            AudioFormat.ENCODING_PCM_16BIT,
            maxOf(minBuffer, READ_BUFFER_BYTES * 4),
        )
        if (recorder.state != AudioRecord.STATE_INITIALIZED) {
            Log.e(TAG, "AudioRecord failed to initialize")
            recorder.release()
            return
        }
        val startedAt = SystemClock.elapsedRealtime()
        try {
            WavWriter(wavFile, SAMPLE_RATE_HZ).use { wav ->
                recorder.startRecording()
                val buffer = ByteArray(READ_BUFFER_BYTES)
                var reads = 0
                while (capturing) {
                    val read = recorder.read(buffer, 0, buffer.size)
                    if (read < 0) {
                        // Error codes (e.g. ERROR_DEAD_OBJECT) don't recover.
                        Log.e(TAG, "AudioRecord.read returned $read; stopping")
                        break
                    }
                    if (read == 0) continue
                    wav.write(buffer, read)
                    session?.feedPcm16(if (read == buffer.size) buffer else buffer.copyOf(read))
                    RecordingState.onElapsed(SystemClock.elapsedRealtime() - startedAt)
                    if (session != null && ++reads % DRAIN_EVERY_READS == 0) {
                        publish(session.drainSegments())
                        RecordingState.setTentative(session.tentative())
                        session.failure()?.let(RecordingState::onTranscriptionFailed)
                    }
                }
            }
        } finally {
            runCatching { recorder.stop() }
            recorder.release()
        }
    }

    override fun onDestroy() {
        capturing = false
        finalizer.shutdown()
        super.onDestroy()
    }

    private fun buildNotification(recording: Boolean): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Recording", NotificationManager.IMPORTANCE_LOW)
        )
        val tapIntent = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val stopIntent = PendingIntent.getService(
            this, 1,
            Intent(this, RecordingService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mic)
            .setContentIntent(tapIntent)
            .setOngoing(true)
        return if (recording) {
            builder.setContentTitle("Recording")
                .setContentText("Transcribing on-device")
                .addAction(Notification.Action.Builder(null, "Stop", stopIntent).build())
                .build()
        } else {
            builder.setContentTitle("Finalizing transcript")
                .setContentText("Re-transcribing the whole recording on-device")
                .build()
        }
    }
}
