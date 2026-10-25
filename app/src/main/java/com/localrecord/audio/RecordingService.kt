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
import android.util.Log
import android.widget.Toast
import com.localrecord.MainActivity
import com.localrecord.R
import com.localrecord.data.RecordingRepository
import com.localrecord.engine.EngineManager
import com.localrecord.model.ModelDownloader
import uniffi.whisper_engine.LiveSession
import uniffi.whisper_engine.RecordingWriter
import uniffi.whisper_engine.Segment
import uniffi.whisper_engine.WhisperEngineException
import java.io.File
import java.io.IOException
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.thread
import kotlin.concurrent.withLock

/**
 * Foreground service that captures mic audio, encodes it to an Ogg Opus file
 * (Rust's [RecordingWriter]), and feeds the same buffers to the Rust
 * live-transcription session. Kotlin only marshals buffers — all encoding and
 * transcription logic is on the Rust side.
 *
 * Capture format is 16 kHz mono PCM16: exactly what the speech model
 * consumes, so no resampling and one shared buffer for both the file and the
 * engine.
 *
 * Pausing releases the mic; the audio on either side of a pause is joined in
 * one file and one live session, so transcript times match the file.
 *
 * After a recording stops, its live transcript is saved as a draft. The
 * whole-recording pass (more accurate, but minutes of CPU work) runs only on
 * request: [finalize] queues it, and it replaces the draft when done. The
 * service stays in the foreground until capture and the queue are both done.
 */
class RecordingService : Service() {

    companion object {
        const val ACTION_START = "com.localrecord.action.START_RECORDING"
        const val ACTION_STOP = "com.localrecord.action.STOP_RECORDING"
        const val ACTION_PAUSE = "com.localrecord.action.PAUSE_RECORDING"
        const val ACTION_RESUME = "com.localrecord.action.RESUME_RECORDING"
        const val ACTION_FINALIZE = "com.localrecord.action.FINALIZE_TRANSCRIPT"
        private const val EXTRA_AUDIO_PATH = "audio_path"

        const val SAMPLE_RATE_HZ = 16_000
        private const val TAG = "RecordingService"
        private const val CHANNEL_ID = "recording"
        private const val NOTIFICATION_ID = 1
        /** 100 ms of 16 kHz mono s16 audio per read. */
        private const val READ_BUFFER_BYTES = SAMPLE_RATE_HZ / 10 * 2
        /** Pull newly stable segments every 5 reads (500 ms). */
        private const val DRAIN_EVERY_READS = 5
        /** How often a paused capture thread still pulls segments a running pass commits. */
        private const val PAUSED_DRAIN_MS = 500L
        /** 16 kHz mono s16. */
        private const val BYTES_PER_SECOND = SAMPLE_RATE_HZ * 2
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

        fun stop(context: Context) = send(context, ACTION_STOP)

        fun pause(context: Context) = send(context, ACTION_PAUSE)

        fun resume(context: Context) = send(context, ACTION_RESUME)

        private fun send(context: Context, action: String) {
            context.startService(Intent(context, RecordingService::class.java).setAction(action))
        }

        /** Queue a final transcription pass for an existing recording. */
        fun finalize(context: Context, audioFile: File) {
            context.startForegroundService(
                Intent(context, RecordingService::class.java)
                    .setAction(ACTION_FINALIZE)
                    .putExtra(EXTRA_AUDIO_PATH, audioFile.path)
            )
        }

        /** Foreground type for the final pass: no mic, just CPU work. */
        private val PROCESSING_TYPE =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.VANILLA_ICE_CREAM) {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROCESSING
            } else {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
            }
    }

    @Volatile
    private var capturing = false
    @Volatile
    private var paused = false
    /** Held while [capturing] or [paused] change, so a paused capture thread wakes at once. */
    private val captureLock = ReentrantLock()
    private val captureChanged = captureLock.newCondition()
    /** Alive from start until the file and live transcript are saved. Main thread only. */
    private var captureThread: Thread? = null

    /**
     * Final passes run one at a time, off the capture thread, at background
     * priority (nice 10) so the phone stays usable; the engine decodes on the
     * calling thread, so the whole pass runs at it.
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
            ACTION_STOP -> if (active) signalCapture { capturing = false } else stopIfIdle()
            ACTION_PAUSE, ACTION_RESUME -> if (active && capturing) {
                signalCapture { paused = intent.action == ACTION_PAUSE }
                updateForeground()
            } else {
                stopIfIdle()
            }
            ACTION_FINALIZE -> {
                val path = intent.getStringExtra(EXTRA_AUDIO_PATH)
                if (path != null) enqueueFinal(File(path)) else stopIfIdle()
            }
        }
        return START_NOT_STICKY
    }

    private fun startRecording() {
        capturing = true
        paused = false
        updateForeground()
        val audioFile = RecordingRepository.newRecordingFile(this)
        // Model load (first use) and transcription flush block; keep them off
        // the main thread.
        captureThread = thread(name = "audio-capture") { captureLoop(audioFile) }
    }

    private fun captureLoop(audioFile: File) {
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
        captureAudio(audioFile, session, publish)
        // Capture also ends on its own after a mic or storage error.
        signalCapture { capturing = false }

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
            RecordingRepository.writeTranscript(audioFile, transcript)
        }
        RecordingState.onRecordingStopped()
        val thisThread = Thread.currentThread()
        mainHandler.post {
            // A new recording may already have started its own capture thread.
            if (captureThread === thisThread) captureThread = null
            if (ModelDownloader.speechModelReady(this)) {
                Toast.makeText(this, REFINE_HINT, Toast.LENGTH_LONG).show()
            }
            stopIfIdle()
        }
    }

    /** Main thread. */
    private fun enqueueFinal(audioFile: File) {
        pendingFinals++
        RecordingState.onFinalizing(audioFile, true)
        updateForeground()
        finalizer.execute {
            // A foreground service doesn't keep the CPU awake; without this
            // the pass stalls whenever the screen is off.
            val wakeLock = getSystemService(PowerManager::class.java)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "localrecord:final-pass")
            wakeLock.acquire(FINAL_PASS_WAKE_LOCK_MS)
            try {
                finalizeTranscript(audioFile)
            } finally {
                if (wakeLock.isHeld) wakeLock.release()
            }
            mainHandler.post {
                pendingFinals--
                RecordingState.onFinalizing(audioFile, false)
                updateForeground()
                stopIfIdle()
            }
        }
    }

    /** Replace the live draft with a whole-recording pass. Blocking; finalizer thread. */
    private fun finalizeTranscript(audioFile: File) {
        try {
            val engine = EngineManager.getOrLoad(this)
            if (engine == null) {
                RecordingState.onFinalizeFailed(audioFile, "the speech model isn't downloaded")
                return
            }
            val segments = engine.transcribeRecording(audioFile.path, EngineManager.speakersOrLoad(this))
            // An empty final pass over audio the draft found speech in is
            // more likely a failure than the truth; keep the draft then.
            if (segments.isNotEmpty() || !RecordingRepository.transcriptFileFor(audioFile).exists()) {
                RecordingRepository.writeTranscript(audioFile, segments)
            }
        } catch (e: WhisperEngineException) {
            Log.e(TAG, "final pass failed for ${audioFile.name}; keeping the live transcript", e)
            RecordingState.onFinalizeFailed(audioFile, e.message ?: "transcription failed")
        } catch (e: IOException) {
            Log.e(TAG, "can't save the transcript of ${audioFile.name}", e)
            RecordingState.onFinalizeFailed(audioFile, "can't save the transcript: ${e.message}")
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

    /** Changes [capturing] / [paused] and wakes a paused capture thread to act on it. */
    private fun signalCapture(change: () -> Unit) = captureLock.withLock {
        change()
        captureChanged.signalAll()
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
        audioFile: File,
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
        val drain = { live: LiveSession ->
            publish(live.drainSegments())
            RecordingState.setTentative(live.tentative())
        }
        try {
            RecordingWriter(audioFile.path).use { writer ->
                val buffer = ByteArray(READ_BUFFER_BYTES)
                var reads = 0
                var recordedBytes = 0L
                while (capturing) {
                    if (paused) {
                        if (recorder.recordingState == AudioRecord.RECORDSTATE_RECORDING) recorder.stop()
                        RecordingState.onPaused(true)
                        captureLock.withLock {
                            if (paused && capturing) {
                                captureChanged.await(PAUSED_DRAIN_MS, TimeUnit.MILLISECONDS)
                            }
                        }
                        session?.let(drain)
                        continue
                    }
                    if (recorder.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
                        recorder.startRecording()
                        RecordingState.onPaused(false)
                    }
                    val read = recorder.read(buffer, 0, buffer.size)
                    if (read < 0) {
                        // Error codes (e.g. ERROR_DEAD_OBJECT) don't recover.
                        Log.e(TAG, "AudioRecord.read returned $read; stopping")
                        break
                    }
                    if (read == 0) continue
                    val pcm = if (read == buffer.size) buffer else buffer.copyOf(read)
                    writer.writePcm16(pcm)
                    session?.feedPcm16(pcm)
                    // Audio time, so pauses don't count.
                    recordedBytes += read
                    RecordingState.onElapsed(recordedBytes * 1000 / BYTES_PER_SECOND)
                    if (session != null && ++reads % DRAIN_EVERY_READS == 0) drain(session)
                }
                writer.finish()
            }
        } catch (e: WhisperEngineException) {
            // Storage full or gone: the file keeps what was written before.
            Log.e(TAG, "can't write ${audioFile.name}; stopping", e)
        } finally {
            runCatching { recorder.stop() }
            recorder.release()
        }
    }

    override fun onDestroy() {
        signalCapture { capturing = false }
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
        val pauseAction = if (paused) ACTION_RESUME else ACTION_PAUSE
        val pauseIntent = PendingIntent.getService(
            this, 2,
            Intent(this, RecordingService::class.java).setAction(pauseAction),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mic)
            .setContentIntent(tapIntent)
            .setOngoing(true)
        return if (recording) {
            builder.setContentTitle(if (paused) "Recording paused" else "Recording")
                .setContentText(if (paused) "Mic off" else "Transcribing on-device")
                .addAction(
                    Notification.Action.Builder(null, if (paused) "Resume" else "Pause", pauseIntent).build()
                )
                .addAction(Notification.Action.Builder(null, "Stop", stopIntent).build())
                .build()
        } else {
            builder.setContentTitle("Finalizing transcript")
                .setContentText("Re-transcribing the whole recording on-device")
                .build()
        }
    }
}
