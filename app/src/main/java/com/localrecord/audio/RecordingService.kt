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
import android.os.IBinder
import android.os.SystemClock
import android.util.Log
import com.localrecord.MainActivity
import com.localrecord.R
import com.localrecord.data.RecordingRepository
import com.localrecord.engine.EngineManager
import uniffi.whisper_engine.LiveSession
import uniffi.whisper_engine.Segment
import uniffi.whisper_engine.WhisperEngineException
import java.io.File
import kotlin.concurrent.thread

/**
 * Foreground service that captures mic audio, writes it to a WAV file, and
 * feeds the same buffers to the Rust live-transcription session. Kotlin only
 * marshals buffers — all transcription logic is on the Rust side.
 *
 * Capture format is 16 kHz mono PCM16: exactly what Whisper consumes, so no
 * resampling and one shared buffer for both the file and the engine.
 */
class RecordingService : Service() {

    companion object {
        const val ACTION_START = "com.localrecord.action.START_RECORDING"
        const val ACTION_STOP = "com.localrecord.action.STOP_RECORDING"

        const val SAMPLE_RATE_HZ = 16_000
        private const val TAG = "RecordingService"
        private const val CHANNEL_ID = "recording"
        private const val NOTIFICATION_ID = 1
        /** 100 ms of 16 kHz mono s16 audio per read. */
        private const val READ_BUFFER_BYTES = SAMPLE_RATE_HZ / 10 * 2
        /** Pull newly stable segments every 5 reads (500 ms). */
        private const val DRAIN_EVERY_READS = 5

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
    }

    @Volatile
    private var capturing = false
    /** Alive from start until the file and transcript are finalized. Main thread only. */
    private var captureThread: Thread? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val active = captureThread?.isAlive == true
        when (intent?.action) {
            ACTION_START -> if (!active) startRecording()
            // The capture thread finalizes and stops the service itself.
            ACTION_STOP -> if (active) capturing = false else stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun startRecording() {
        startForeground(
            NOTIFICATION_ID,
            buildNotification(),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE,
        )
        val wavFile = RecordingRepository.newRecordingFile(this)
        capturing = true
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
        stopSelf()
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
        super.onDestroy()
    }

    private fun buildNotification(): Notification {
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
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mic)
            .setContentTitle("Recording")
            .setContentText("Transcribing on-device")
            .setContentIntent(tapIntent)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, "Stop", stopIntent).build())
            .build()
    }
}
