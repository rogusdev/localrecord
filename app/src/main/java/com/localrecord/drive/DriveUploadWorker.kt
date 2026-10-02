package com.localrecord.drive

import android.content.Context
import android.net.ConnectivityManager
import android.util.Log
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import com.localrecord.audio.RecordingState
import com.localrecord.data.RecordingRepository
import com.localrecord.settings.Settings
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.withContext
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody
import okhttp3.RequestBody.Companion.asRequestBody
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * Batched, retryable backup of recordings (and transcripts) to a
 * "LocalRecord" folder in Drive, via the REST API's resumable upload.
 * Files already uploaded are tracked locally by name in SharedPreferences.
 *
 * Wi-Fi-only by default; cellular upload is an explicit opt-in setting.
 */
class DriveUploadWorker(context: Context, params: WorkerParameters) :
    CoroutineWorker(context, params) {

    companion object {
        private const val TAG = "DriveUploadWorker"
        private const val WORK_NAME = "drive-upload"
        private const val FOLDER_NAME = "LocalRecord"
        private const val DRIVE_FILES = "https://www.googleapis.com/drive/v3/files"
        private const val DRIVE_UPLOAD =
            "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable"

        /**
         * Queue an upload pass. Appends behind a pass that is already running,
         * since that pass listed its files before the new recording existed.
         */
        fun enqueue(context: Context) = enqueue(context, ExistingWorkPolicy.APPEND_OR_REPLACE)

        /** Re-queue with the current network setting after it changes. */
        fun reschedule(context: Context) = enqueue(context, ExistingWorkPolicy.REPLACE)

        private fun enqueue(context: Context, policy: ExistingWorkPolicy) {
            val networkType =
                if (Settings.wifiOnlyUpload(context)) NetworkType.UNMETERED
                else NetworkType.CONNECTED
            val request = OneTimeWorkRequestBuilder<DriveUploadWorker>()
                .setConstraints(
                    Constraints.Builder().setRequiredNetworkType(networkType).build()
                )
                .build()
            WorkManager.getInstance(context)
                .enqueueUniqueWork(WORK_NAME, policy, request)
        }
    }

    private val client = OkHttpClient()

    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        if (!Settings.driveBackupEnabled(applicationContext)) return@withContext Result.success()
        val token = DriveAuth.accessTokenOrNull(applicationContext)
        if (token == null) {
            Log.i(TAG, "Drive not authorized yet; skipping upload")
            return@withContext Result.success()
        }

        val uploaded = Settings.uploadedFileNames(applicationContext)
        val recording = RecordingState.activeFile
        val pending = RecordingRepository.recordingsDir(applicationContext)
            .listFiles { f ->
                f.extension in setOf("wav", "txt") && f.name !in uploaded && f != recording
            }
            .orEmpty()
            .sortedBy { it.name }
        if (pending.isEmpty()) return@withContext Result.success()

        return@withContext try {
            val folderId = ensureFolder(token)
            for (file in pending) {
                // Uploads block, so check between files whether WorkManager
                // stopped us (e.g. Wi-Fi lost) or the network became metered.
                ensureActive()
                if (Settings.wifiOnlyUpload(applicationContext) && isNetworkMetered()) {
                    return@withContext Result.retry()
                }
                uploadFile(token, folderId, file)
                Settings.markUploaded(applicationContext, file.name)
                Log.i(TAG, "uploaded ${file.name}")
            }
            Result.success()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Log.w(TAG, "upload failed, will retry", e)
            Result.retry()
        }
    }

    /** Find or create the LocalRecord folder, returning its Drive id. */
    private fun ensureFolder(token: String): String {
        val query = "name='$FOLDER_NAME' and mimeType='application/vnd.google-apps.folder'" +
            " and trashed=false"
        val searchUrl = "$DRIVE_FILES?q=${java.net.URLEncoder.encode(query, "UTF-8")}&fields=files(id)"
        client.newCall(authed(token, Request.Builder().url(searchUrl).get())).execute().use { resp ->
            check(resp.isSuccessful) { "folder search failed: HTTP ${resp.code}" }
            val files = JSONObject(resp.body!!.string()).getJSONArray("files")
            if (files.length() > 0) return files.getJSONObject(0).getString("id")
        }

        val metadata = JSONObject()
            .put("name", FOLDER_NAME)
            .put("mimeType", "application/vnd.google-apps.folder")
        val body = metadata.toString().toRequestBody("application/json".toMediaType())
        client.newCall(authed(token, Request.Builder().url(DRIVE_FILES).post(body)))
            .execute().use { resp ->
                check(resp.isSuccessful) { "folder create failed: HTTP ${resp.code}" }
                return JSONObject(resp.body!!.string()).getString("id")
            }
    }

    private fun uploadFile(token: String, folderId: String, file: File) {
        val mimeType = if (file.extension == "wav") "audio/wav" else "text/plain"
        val metadata = JSONObject()
            .put("name", file.name)
            .put("parents", JSONArray().put(folderId))

        // Step 1: start a resumable session
        val initBody = metadata.toString().toRequestBody("application/json".toMediaType())
        val sessionUrl = client.newCall(
            authed(
                token,
                Request.Builder()
                    .url(DRIVE_UPLOAD)
                    .header("X-Upload-Content-Type", mimeType)
                    .header("X-Upload-Content-Length", file.length().toString())
                    .post(initBody)
            )
        ).execute().use { resp ->
            check(resp.isSuccessful) { "resumable init failed: HTTP ${resp.code}" }
            resp.header("Location") ?: error("no resumable session URL")
        }

        // Step 2: upload content. OkHttp streams the file body; Drive
        // accepts the whole file in one PUT and we lean on WorkManager
        // retry for interrupted transfers (fresh session next attempt).
        val putBody: RequestBody = file.asRequestBody(mimeType.toMediaType())
        client.newCall(Request.Builder().url(sessionUrl).put(putBody).build())
            .execute().use { resp ->
                check(resp.isSuccessful) { "content upload failed: HTTP ${resp.code}" }
            }
    }

    private fun isNetworkMetered(): Boolean =
        applicationContext.getSystemService(ConnectivityManager::class.java).isActiveNetworkMetered

    private fun authed(token: String, builder: Request.Builder): Request =
        builder.header("Authorization", "Bearer $token").build()
}
