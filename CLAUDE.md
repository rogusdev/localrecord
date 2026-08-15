# CLAUDE.md

## Project: Local Whisper Voice Recorder for Android

A privacy-first Android voice recorder with on-device, GPU-accelerated live
transcription (Whisper via whisper.cpp/Vulkan) and automatic Google Drive
backup. Built as a personal replacement for Pixel Recorder on a OnePlus 15.

## Architecture

- **UI/Android layer**: Kotlin, single-activity, Jetpack Compose
  - AudioRecord/MediaRecorder for capture, foreground service for
    background recording
  - WorkManager for batched/retryable Drive uploads
  - Google Drive REST API (resumable upload), OAuth via Android Identity
    Services
- **Transcription engine**: Rust crate wrapping `whisper-rs`
  (binding over whisper.cpp), built for Android via `cargo-ndk`
  - Exposed to Kotlin via JNI (uniffi or hand-rolled JNI bridge — TBD,
    default to uniffi unless it fights us)
  - whisper.cpp built with Vulkan backend (`GGML_VULKAN=ON`) targeting
    Adreno GPU on Snapdragon 8 Elite Gen 5
- **Live transcription**: sliding-window chunking (~3-5s windows,
  overlap to avoid word-splitting), not true streaming — Whisper has no
  native streaming mode
- **Model**: start with `ggml-base.en` or `small.en` quantized
  (balance latency vs accuracy on-device); model file ships as an
  asset or downloads once on first run (no further network use)

## Repo layout (proposed)

- `/app` — Kotlin/Compose Android app module
- `/rust-engine` — Rust crate, whisper-rs wrapper, cargo-ndk build config
- `/whisper.cpp` — vendored or submodule, Vulkan backend enabled
- `/scripts` — build scripts (cargo-ndk invocation, NDK env setup)

## Build commands

- Rust → Android libs: `cargo ndk -t arm64-v8a -t armeabi-v7a -o app/src/main/jniLibs build --release`
- Android app: `./gradlew assembleDebug` (run from WSL2, NDK/SDK env vars
  must be set — see scripts/env.sh)
- Physical device only for GPU/Vulkan testing — emulator Vulkan compute
  is unreliable, don't trust emulator results for transcription latency

## Conventions

- Rust: standard preferred conventions apply (clear error types, no
  unwrap() in non-test code, prefer explicit over clever)
- Kotlin: keep UI layer thin; all transcription logic lives in Rust,
  Kotlin just marshals audio buffers across JNI and renders results
- No cloud transcription fallback — if Whisper/Vulkan path fails, surface
  the error, don't silently fall back to a network service (defeats the
  whole point of the app)

## Open questions / decisions pending

- uniffi vs hand-rolled JNI for the Rust↔Kotlin bridge
- exact chunking window size/overlap tuning once we have real device
  latency numbers
- whether live transcription runs continuously during recording or only
  on-demand post-recording (battery/thermal tradeoff — revisit Pixel 8
  overheating lesson learned)

## Things NOT to do

- Don't add any cloud STT integration, even as an optional toggle —
  defeats the privacy/offline goal of this project
- Don't default the Drive upload to running over cellular without an
  explicit Wi-Fi-only setting
