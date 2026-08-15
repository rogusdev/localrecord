package com.localrecord.settings

import android.content.Context
import android.content.SharedPreferences

object Settings {
    private const val PREFS = "settings"
    private const val KEY_DRIVE_BACKUP = "drive_backup_enabled"
    private const val KEY_WIFI_ONLY = "wifi_only_upload"
    private const val KEY_UPLOADED = "uploaded_file_names"

    private fun prefs(context: Context): SharedPreferences =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun driveBackupEnabled(context: Context): Boolean =
        prefs(context).getBoolean(KEY_DRIVE_BACKUP, false)

    fun setDriveBackupEnabled(context: Context, enabled: Boolean) =
        prefs(context).edit().putBoolean(KEY_DRIVE_BACKUP, enabled).apply()

    /** Default true — never upload over cellular unless explicitly enabled. */
    fun wifiOnlyUpload(context: Context): Boolean =
        prefs(context).getBoolean(KEY_WIFI_ONLY, true)

    fun setWifiOnlyUpload(context: Context, wifiOnly: Boolean) =
        prefs(context).edit().putBoolean(KEY_WIFI_ONLY, wifiOnly).apply()

    fun uploadedFileNames(context: Context): Set<String> =
        prefs(context).getStringSet(KEY_UPLOADED, emptySet()) ?: emptySet()

    fun markUploaded(context: Context, name: String) {
        val current = uploadedFileNames(context)
        prefs(context).edit().putStringSet(KEY_UPLOADED, current + name).apply()
    }
}
