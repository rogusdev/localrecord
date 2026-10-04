plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "com.localrecord"
    compileSdk = 36
    // Match what scripts/setup-android-sdk.sh and scripts/env.sh install.
    buildToolsVersion = "36.0.0"
    ndkVersion = "28.2.13676358"

    defaultConfig {
        applicationId = "com.localrecord"
        minSdk = 31
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        // Rust engine is built for these ABIs via cargo-ndk (scripts/build-rust.sh)
        ndk {
            abiFilters += listOf("arm64-v8a")
        }
    }

    // Play upload key, from ~/.gradle/gradle.properties (never the repo).
    // Without these the release build comes out unsigned.
    val uploadStoreFile = providers.gradleProperty("localrecord.upload.storeFile").orNull
    signingConfigs {
        if (uploadStoreFile != null) {
            create("upload") {
                storeFile = file(uploadStoreFile)
                storePassword = providers.gradleProperty("localrecord.upload.storePassword").get()
                keyAlias = providers.gradleProperty("localrecord.upload.keyAlias").get()
                keyPassword = providers.gradleProperty("localrecord.upload.keyPassword").get()
            }
        }
    }

    buildTypes {
        release {
            // R8 would strip the classes JNA/uniffi reach by reflection.
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("upload")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
    }

    packaging {
        jniLibs {
            // JNA ships per-ABI libjnidispatch.so inside its .aar
            useLegacyPackaging = false
        }
    }
}

dependencies {
    implementation(platform(libs.compose.bom))
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.compose.ui)
    implementation(libs.compose.ui.tooling.preview)
    implementation(libs.compose.material3)
    implementation(libs.compose.material.icons)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.okhttp)
    // uniffi-generated Kotlin bindings load the Rust cdylib through JNA
    implementation("${libs.jna.get()}@aar")
}
