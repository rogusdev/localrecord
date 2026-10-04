# CLAUDE.md

## Project: Local Whisper Voice Recorder for Android

A privacy-first Android voice recorder with on-device, GPU-accelerated live
transcription (Whisper via whisper.cpp/Vulkan), playback with a word-synced
transcript, and sharing via the system share sheet (e.g. "Save to Drive").
Built as a personal replacement for Pixel Recorder on a OnePlus 15.

## Architecture

- **UI/Android layer** (`app/`): Kotlin, single-activity, Jetpack Compose
  - `RecordingService`: foreground service; one capture thread does
    AudioRecord (16 kHz mono PCM16) → WAV file + Rust session, drains
    segments, flushes and writes the live transcript on stop as a draft: a
    WebVTT `.vtt`: a cue per segment, speaker as `<v Speaker n>`, an inline
    start timestamp before every word; playback reads it back
    (`RecordingRepository`). Then a single-thread queue runs the final pass
    (`transcribe_wav` with small.en) and overwrites the draft; the service
    stays foreground (mediaProcessing, dataSync before Android 15) until the
    queue drains. Playback's "Transcribe again" queues the same pass
  - `RecordingState`: process-wide StateFlows the UI collects (service is
    the only writer)
  - `EngineManager`: owns the loaded models: live engine (base.en) and
    speaker model, preloaded by the ViewModel; final engine (small.en)
    loaded on the first final pass
  - `Player` + `PlaybackScreen`: MediaPlayer playback; highlights the word
    at the play position from the `.vtt` word timestamps; tap a word or
    timestamp to seek
  - Sharing: `shareRecording` sends the `.wav` and `.vtt` through a
    FileProvider to the share sheet. No Drive API/OAuth on purpose: that
    needs a Google Cloud project
- **Transcription engine** (`rust-engine/`): Rust crate over
  `whisper-rs-sys` 0.15 (raw bindings; it bundles and builds whisper.cpp,
  nothing vendored here), built for Android via `cargo-ndk`
  - `src/native.rs` is the safe layer: every whisper.cpp call that can throw
    (ggml Vulkan throws, e.g. device lost) runs inside the C++ try/catch in
    `src/catch.cpp`. A backend exception disables transcription for the
    rest of the process; recording continues and the UI shows the error
  - Adreno 840: ggml's fp16 Vulkan shaders hit `ErrorDeviceLost`, so the
    engine sets `GGML_VK_DISABLE_F16` before the first model load
  - `transcribe_wav` (final pass): reads the WAV in Rust (`src/wav.rs`),
    one `whisper_full` over the whole recording with beam search (5) and
    no text carried between whisper's 30 s windows (carrying it set off a
    repetition loop on the phone), drops word-for-word repeated segments
    (loop safety net), then `speakers::label_recording` (live-style
    matching, then each segment moved to its nearest final speaker)
  - `src/speakers.rs`: live speaker labels. Each committed segment's audio
    gets a voiceprint (sherpa-onnx, 3D-Speaker CAM++ "zh_en common advanced")
    matched to the session's speakers
    (cosine to running means, `SAME_SPEAKER_SIMILARITY`); no match starts a
    new speaker, segments < 1 s inherit the previous label. sherpa-onnx
    links prebuilt libs: static on the host, `libsherpa-onnx-c-api.so` +
    `libonnxruntime.so` on Android (`build-rust.sh` copies them to jniLibs)
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
  discarded after its text is emitted or a pass found no speech. Each pass
  gets the last ~200 characters of committed text as its prompt (AMI
  excerpts: 34.6% → 28.4% WER); the held-back segments are exposed as
  `tentative` and shown grey. The engine runs one warm-up pass at load so
  Vulkan pipeline compilation (~2 s) doesn't delay the first live pass
- **Models**: `ggml-base.en-q5_1.bin` (~60 MB, live), `ggml-small.en-q5_1.bin`
  (~190 MB, final pass) and the 3D-Speaker CAM++ "advanced" voiceprint
  model (~28 MB, Apache-2.0), downloaded once from Hugging Face
  (URLs pinned to a revision, SHA-256 verified) into app-private storage.
  No other network use.

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

## Open questions / decisions pending

- Final-pass model choice (AMI excerpts, WER, beam 5, host CPU): base.en
  30.5%, small.en 24.0%, medium.en 25.7%, large-v3-turbo 23.9% at ~7x
  small's time; beam 1 was far worse (~42%). On the phone (GPU fp32)
  small.en scores ~26% and takes ~0.2x real time (~11 min per hour of
  audio); the phone CPU was far slower than its GPU
- Tune `STEP_MS` / `HOLDBACK_MS` / `MAX_WINDOW_MS` once we have real device
  latency numbers; consider whisper `audio_ctx` (encoder always runs a
  padded 30 s window otherwise) and whisper.cpp's VAD (needs a VAD model)
- Whether live transcription runs continuously during recording or only
  on-demand post-recording (battery/thermal tradeoff — revisit Pixel 8
  overheating lesson learned)

- Speaker model choice: of 11 sherpa-onnx speaker models, CAM++ "zh_en
  common advanced" was best on far-field AMI meetings (live labels 93% of
  segments right, 96% on held-out EN2002a; the VoxCeleb CAM++ got ~50%) at
  ~44 ms per 4 s of audio. Runner-ups: ERes2Net base (similar accuracy,
  2.5x slower), TitaNet small (87%, fastest). An offline "refine" pass with
  sherpa's diarization is not built; it did poorly with the old model

## Things NOT to do

- Don't add any cloud STT integration, even as an optional toggle —
  defeats the privacy/offline goal of this project
