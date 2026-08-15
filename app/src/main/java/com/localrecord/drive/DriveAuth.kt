package com.localrecord.drive

import android.content.Context
import com.google.android.gms.auth.api.identity.AuthorizationRequest
import com.google.android.gms.auth.api.identity.AuthorizationResult
import com.google.android.gms.auth.api.identity.Identity
import com.google.android.gms.common.api.Scope
import kotlinx.coroutines.tasks.await

/**
 * Drive authorization via Android Identity Services. `drive.file` scope only:
 * the app can touch files it created, nothing else in the user's Drive.
 *
 * Setup note: requires an OAuth client ID for this package name + signing
 * key in a Google Cloud project with the Drive API enabled — see README.
 */
object DriveAuth {

    private val SCOPES = listOf(Scope("https://www.googleapis.com/auth/drive.file"))

    /**
     * Returns an authorization result. If `hasResolution` is true the caller
     * (Activity) must launch `pendingIntent` to let the user pick an account
     * and consent; afterwards call this again for the token.
     */
    suspend fun authorize(context: Context): AuthorizationResult {
        val request = AuthorizationRequest.builder()
            .setRequestedScopes(SCOPES)
            .build()
        return Identity.getAuthorizationClient(context).authorize(request).await()
    }

    /** Access token if already authorized, else null. Never prompts. */
    suspend fun accessTokenOrNull(context: Context): String? =
        runCatching { authorize(context) }
            .getOrNull()
            ?.takeUnless { it.hasResolution() }
            ?.accessToken
}
