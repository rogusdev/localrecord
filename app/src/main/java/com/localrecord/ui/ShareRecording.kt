package com.localrecord.ui

import android.content.ClipData
import android.content.Context
import android.content.Intent
import androidx.core.content.FileProvider
import com.localrecord.data.Recording

/** FileProvider authority declared in AndroidManifest.xml. */
private const val FILE_PROVIDER_SUFFIX = ".files"

/**
 * Opens the system share sheet with a recording's audio, transcript and
 * timings, so any installed app can take them (e.g. Drive's "Save to Drive").
 */
fun shareRecording(context: Context, recording: Recording) {
    val authority = context.packageName + FILE_PROVIDER_SUFFIX
    val uris = listOfNotNull(recording.wavFile, recording.transcriptFile, recording.timingsFile)
        .map { FileProvider.getUriForFile(context, authority, it) }
    val send = Intent(Intent.ACTION_SEND_MULTIPLE).apply {
        type = if (uris.size == 1) "audio/wav" else "*/*"
        putParcelableArrayListExtra(Intent.EXTRA_STREAM, ArrayList(uris))
        // The chooser only forwards read grants for URIs in clipData.
        clipData = ClipData.newRawUri(recording.name, uris.first()).apply {
            uris.drop(1).forEach { addItem(ClipData.Item(it)) }
        }
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    }
    context.startActivity(Intent.createChooser(send, "Share ${recording.name}"))
}
