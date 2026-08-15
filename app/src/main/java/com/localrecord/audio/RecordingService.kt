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
import com.localrecord.drive.DriveUploadWorker
import com.localrecord.engine.EngineManager
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import uniffi.whisper_engine.LiveSession
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
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
        private const val CHANNEL_ID = "recording"
        private const val NOTIFICATION_ID = 1
        /** 100 ms of 16 kHz mono s16 audio per read. */
        private const val READ_BUFFER_BYTES = SAMPLE_RATE_HZ / 10 * 2
        private const val SEGMENT_POLL_MS = 500L

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

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    @Volatile
    private var capturing = false
    private var captureThread: Thread? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> if (!capturing) startRecording()
            ACTION_STOP -> stopRecording()
        }
        return START_NOT_STICKY
    }

    @SuppressLint("MissingPermission") // UI checks RECORD_AUDIO before starting
    private fun startRecording() {
        startForeground(
            NOTIFICATION_ID,
            buildNotification(),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE,
        )

        val timestamp = SimpleDateFormat("yyyyMMdd_HHmmss", Locale.US).format(Date())
        val wavFile = File(RecordingRepository.recordingsDir(this), "rec_$timestamp.wav")

        // Engine is null when the model isn't downloaded yet: record anyway,
        // just without live transcription.
        val session = EngineManager.getOrLoad(this)?.createLiveSession()
        RecordingState.onRecordingStarted(transcribing = session != null)
        capturing = true

        captureThread = thread(name = "audio-capture") {
            captureLoop(wavFile, session)
        }

        if (session != null) {
            scope.launch {
                while (isActive && capturing) {
                    delay(SEGMENT_POLL_MS)
                    RecordingState.appendSegments(session.drainSegments())
                }
            }
        }
    }

    @SuppressLint("MissingPermission")
    private fun captureLoop(wavFile: File, session: LiveSession?) {
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
        val startedAt = SystemClock.elapsedRealtime()
        try {
            WavWriter(wavFile, SAMPLE_RATE_HZ).use { wav ->
                recorder.startRecording()
                val buffer = ByteArray(READ_BUFFER_BYTES)
                while (capturing) {
                    val read = recorder.read(buffer, 0, buffer.size)
                    if (read <= 0) {
                        Log.w("RecordingService", "AudioRecord.read returned $read")
                        continue
                    }
                    wav.write(buffer, read)
                    session?.feedPcm16(if (read == buffer.size) buffer else buffer.copyOf(read))
                    RecordingState.onElapsed(SystemClock.elapsedRealtime() - startedAt)
                }
            }
        } finally {
            runCatching { recorder.stop() }
            recorder.release()
        }

        // Flush the transcription tail and persist the transcript.
        if (session != null) {
            val remaining = runCatching { session.finish() }
                .onFailure { Log.e("RecordingService", "session.finish failed", it) }
                .getOrDefault(emptyList())
            RecordingState.appendSegments(remaining)
        }
        val transcript = RecordingState.liveSegments.value
        if (transcript.isNotEmpty()) {
            RecordingRepository.writeTranscript(wavFile, transcript)
        }
        RecordingState.onRecordingStopped()
        DriveUploadWorker.enqueue(this)
        stopSelf()
    }

    private fun stopRecording() {
        capturing = false
        captureThread?.join(10_000)
        captureThread = null
    }

    override fun onDestroy() {
        capturing = false
        scope.cancel()
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
