# CLAUDE.md

## Project: Local Whisper Voice Recorder for Android

A privacy-first Android voice recorder with on-device, GPU-accelerated live
transcription (Whisper via whisper.cpp/Vulkan) and optional Google Drive
backup. Built as a personal replacement for Pixel Recorder on a OnePlus 15.

## Architecture

- **UI/Android layer** (`app/`): Kotlin, single-activity, Jetpack Compose
  - `RecordingService`: foreground service; one capture thread does
    AudioRecord (16 kHz mono PCM16) → WAV file + Rust session, drains
    segments, flushes and writes the `.txt` transcript on stop
  - `RecordingState`: process-wide StateFlows the UI collects (service is
    the only writer); also exposes the active file so Drive skips it
  - `EngineManager`: owns the single loaded model; preloaded by the
    ViewModel once the model is on disk
  - `DriveUploadWorker`: WorkManager, Drive REST resumable upload,
    `drive.file` scope, OAuth via Android Identity Services
- **Transcription engine** (`rust-engine/`): Rust crate over
  `whisper-rs-sys` 0.15 (raw bindings; it bundles and builds whisper.cpp,
  nothing vendored here), built for Android via `cargo-ndk`
  - `src/native.rs` is the safe layer: every whisper.cpp call that can throw
    (ggml Vulkan throws, e.g. device lost) runs inside the C++ try/catch in
    `src/catch.cpp`. A backend exception disables transcription for the
    rest of the process; recording continues and the UI shows the error
  - Adreno 840: ggml's fp16 Vulkan shaders hit `ErrorDeviceLost`, so the
    engine sets `GGML_VK_DISABLE_F16` before the first model load
  - Exposed to Kotlin via **uniffi** (bindings loaded through JNA),
    generated into `app/src/main/java/uniffi/` (gitignored)
  - `vulkan` cargo feature → ggml Vulkan backend for the Adreno GPU
- **Live transcription** (`rust-engine/src/session.rs`): Whisper has no
  streaming mode. A worker re-runs inference over the uncommitted buffer
  every `STEP_MS` (4 s) of new audio; segments ending ≥ `HOLDBACK_MS` (1 s)
  before the buffer end are emitted and the buffer is trimmed to the last
  one's end, the rest is re-transcribed next pass. Zero-length segments are
  whisper's (often hallucinated) guess at trailing audio and are never
  committed. Forced commit at `MAX_WINDOW_MS` (20 s). Audio is only
  discarded after its text is emitted or a pass found no speech.
- **Model**: `ggml-base.en-q5_1.bin` (~60 MB), downloaded once from Hugging
  Face (URL pinned to a revision, SHA-256 verified) into app-private
  storage. No other network use except opt-in Drive backup.

## Build commands

- One-time SDK/NDK: `scripts/setup-android-sdk.sh` (build-tools 36.0.0 and
  NDK 28.2.13676358, pinned to match in `app/build.gradle.kts`)
- Rust → `app/src/main/jniLibs` + regenerated Kotlin bindings:
  `scripts/build-rust.sh` (arm64-v8a only by default; `ABIS="..."` to add)
  — rerun after any change to the Rust API
- Android app: `./gradlew assembleDebug` (run from WSL2; `scripts/env.sh`
  sets SDK/NDK env vars)
- Rust tests (host): `cd rust-engine && cargo test`
- Chunking check against a real model (host, CPU):
  `cargo run --release --example live_check -- <model.bin> <audio.raw>`
  — prints live vs one-shot transcripts; use it when changing session.rs
- The NDK toolchain and AGP's aapt2 are x86_64-only binaries. On an aarch64
  host, aapt2/llvm-strip/glslc run under qemu-user with an x86_64 root
  (extracted amd64 debs: libc6, libgcc-s1, libstdc++6, zlib1g) at
  `QEMU_LD_PREFIX` — `env.sh` defaults it to `~/.cache/localrecord/x86root`.
  `build-rust.sh` compiles with host clang-18/lld via a stand-in NDK
  (`scripts/host-ndk.sh`), which also supplies Vulkan-Headers (`vulkan.hpp`)
- Physical device only for GPU/Vulkan testing — emulator Vulkan compute
  is unreliable, don't trust emulator results for transcription latency

## Conventions

- Rust: clear error types, no unwrap()/expect() in non-test code, prefer
  explicit over clever. Nothing exported may panic across the FFI (mutex
  locks recover from poisoning), and no C++ exception may reach Rust
  outside `native.rs`'s catch. `WhisperEngineError` is a uniffi
  `flat_error`, so Kotlin gets `WhisperEngineException` with the Display
  text as its message
- Kotlin: keep UI layer thin; all transcription logic lives in Rust,
  Kotlin just marshals audio buffers across the bridge and renders results.
  Nothing blocking (model load, `finish`, file IO) on the main thread
- No cloud transcription fallback — if Whisper/Vulkan path fails, surface
  the error, don't silently fall back to a network service (defeats the
  whole point of the app). Recording itself must keep working without
  transcription
- Drive Wi-Fi-only must hold at runtime too: the worker re-checks for a
  metered network between files, and changing the setting reschedules the
  queued work

## Open questions / decisions pending

- Tune `STEP_MS` / `HOLDBACK_MS` / `MAX_WINDOW_MS` once we have real device
  latency numbers; consider whisper `audio_ctx` (encoder always runs a
  padded 30 s window otherwise) and whisper.cpp's VAD (needs a VAD model)
- Whether live transcription runs continuously during recording or only
  on-demand post-recording (battery/thermal tradeoff — revisit Pixel 8
  overheating lesson learned)

## Things NOT to do

- Don't add any cloud STT integration, even as an optional toggle —
  defeats the privacy/offline goal of this project
- Don't default the Drive upload to running over cellular without an
  explicit Wi-Fi-only setting
