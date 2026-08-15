package com.localrecord

import android.app.Application
import com.localrecord.model.ModelDownloader

class App : Application() {
    override fun onCreate() {
        super.onCreate()
        ModelDownloader.refreshState(this)
    }
}
