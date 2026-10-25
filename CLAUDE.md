# CLAUDE.md

## Project: Local Voice Recorder for Android

A privacy-first Android voice recorder with on-device live transcription
(NVIDIA Parakeet TDT 0.6B v2 via sherpa-onnx / ONNX Runtime, on the CPU),
playback with a word-synced transcript, and sharing via the system share
sheet (e.g. "Save to Drive").
Built as a personal replacement for Pixel Recorder on a OnePlus 15.

## Architecture

- **UI/Android layer** (`app/`): Kotlin, single-activity, Jetpack Compose
  - `RecordingService`: foreground service; one capture thread does
    AudioRecord (16 kHz mono PCM16) → Ogg Opus file (Rust's
    `RecordingWriter`) + Rust session, drains
    segments, flushes and writes the live transcript on stop as a draft: a
    WebVTT `.vtt`: a cue per segment, speaker as `<v Speaker n>`, an inline
    start timestamp before every word; playback reads it back
    (`RecordingRepository` does the file IO, Rust's `vtt.rs` the format).
    The record button pauses; paused offers resume or stop. Pause stops
    AudioRecord (mic off); resume appends to the same file and live session.
    On stop a toast points to playback's "Refine transcript" button, which
    queues the final pass (`transcribe_recording`) on a single-thread,
    background-priority queue holding a partial wakelock; it overwrites the
    draft. Not automatic: minutes of CPU per hour of audio. The service
    stays foreground (mediaProcessing, dataSync before Android 15) until the
    queue drains
  - `RecordingState`: process-wide StateFlows the UI collects (service is
    the only writer)
  - `EngineManager`: owns the loaded models, preloaded by the ViewModel:
    one `SpeechEngine` (Parakeet, ~1.2 GB in memory) shared by live
    sessions and final passes, and the speaker model
  - `Player` + `PlaybackScreen`: AudioTrack playback of the recording
    (decoded in Rust: `RecordingFile` / `RecordingStream`), position
    from `AudioTrack.getTimestamp` (the frame at the speaker; MediaPlayer's
    position ran ~0.5 s ahead of the sound on the OnePlus 15); highlights
    the word at the play position from the `.vtt` word timestamps; tap a
    word or timestamp to seek
  - Sharing: `shareRecording` sends the `.ogg` and `.vtt` through a
    FileProvider to the share sheet. No Drive API/OAuth on purpose: that
    needs a Google Cloud project
  - Import (`importRecordings`): the system file picker (Drive etc. as
    document providers) copies `.ogg`/`.opus` recordings (saved as `.ogg`,
    must decode as Ogg Opus) and their `.vtt`s into the recordings dir;
    never overwrites, a `.vtt` must parse and have its recording
- **Transcription engine** (`rust-engine/`): Rust crate over sherpa-onnx
  1.13 running Parakeet TDT 0.6B v2 (int8), built for Android via
  `cargo-ndk`. sherpa-onnx links prebuilt libs: static on the host,
  `libsherpa-onnx-c-api.so` + `libonnxruntime.so` on Android
  (`build-rust.sh` copies them to jniLibs)
  - `src/native.rs` is the safe layer over sherpa-onnx's offline recognizer
    C API (`sherpa-onnx-sys`): every call runs inside the C++ try/catch in
    `src/catch.cpp`. ONNX Runtime throws through the C API (e.g. on empty
    input), which would abort the process; a caught exception fails only
    that call. The recognizer config is a zeroed C struct (null/0 = default)
  - `DECODE_THREADS` = 1: ONNX Runtime decodes on the calling thread, so a
    final pass on a background-priority thread stays background (a thread
    pool keeps the loading thread's priority), and one loaded model serves
    live and final passes at once (concurrent decodes are safe)
  - `src/words.rs`: Parakeet returns SentencePiece tokens with TDT times
    (80 ms frames), no segments. A leading space starts a word; a word ends
    at its last spoken token (punctuation comes in the pause after it).
    A segment ends at `.`/`?`/`!`, before a ≥ 1 s pause, or at 10 s
  - Word starts land up to ~0.15 s early after a pause; `src/onsets.rs`
    (audio energy) moves them to where the voice comes back
  - Audio under 1.1 s is zero-padded before decoding: Parakeet made up
    "Mm-hmm." from a fraction of a second of silence (not from a second)
  - Recordings are Ogg Opus only: `src/opus_file.rs` over libopus
    (`opus` crate, bundled libopus built by CMake, linked statically) and
    `src/ogg.rs` (pages). 16 kHz mono at 24 kbit/s (~11 MB an hour vs
    115 MB as PCM). A page reaches the file about once a second, so a crash
    loses at most ~1 s. Decoding is straight to 16 kHz; `RecordingFile`
    indexes the pages once, seeks start 80 ms early (decoder pre-roll), and
    `recording_duration_ms` reads only the first and last pages (list view)
  - `transcribe_recording` (final pass): decodes the Ogg Opus file in Rust,
    cuts it into 20–30 s pieces at the quietest 200 ms (`src/chunks.rs`;
    Parakeet attends over a whole piece, so decode time and memory grow
    faster than its length), decodes each, segments the words of the whole
    recording, then `speakers::label_recording` (live-style matching, then
    each segment moved to its nearest final speaker). Same model as live;
    it gains longer, pause-aligned context and recording-wide speaker labels
  - `src/vtt.rs`: the transcript file format: `transcript_to_vtt` /
    `transcript_from_vtt` (exported to Kotlin)
  - `src/speakers.rs`: live speaker labels. Each committed segment's audio
    gets a voiceprint (sherpa-onnx, 3D-Speaker CAM++ "zh_en common advanced")
    matched to the session's speakers
    (cosine to running means, `SAME_SPEAKER_SIMILARITY`); no match starts a
    new speaker, segments < 1 s inherit the previous label
  - Exposed to Kotlin via **uniffi** (bindings loaded through JNA),
    generated into `app/src/main/java/uniffi/` (gitignored)
- **Live transcription** (`rust-engine/src/session.rs`): Parakeet has no
  streaming mode. A worker re-decodes the uncommitted buffer every
  `STEP_MS` (4 s) of new audio; segments ending ≥ `HOLDBACK_MS` (1 s)
  before the buffer end are emitted and the buffer is trimmed to the last
  one's end, the rest is re-transcribed next pass. Forced commit at
  `MAX_WINDOW_MS` (20 s). Audio is only discarded after its text is
  emitted or a pass found no speech. The held-back segments are exposed as
  `tentative` and shown grey
- **Models**: sherpa-onnx's int8 export of Parakeet TDT 0.6B v2 (four
  files, ~661 MB, CC-BY-4.0, in `models/parakeet-tdt-0.6b-v2-int8/`) and
  the 3D-Speaker CAM++ "advanced" voiceprint model (~28 MB, Apache-2.0),
  downloaded once from Hugging Face (URLs pinned to a revision, SHA-256
  verified) into app-private storage. No other network use.

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
  `cargo run --release --example live_check -- <model-dir> <audio.raw> [campplus.onnx]`
  — prints live vs one-shot transcripts; use it when changing session.rs
- The NDK toolchain and AGP's aapt2 are x86_64-only binaries. On an aarch64
  host, aapt2/llvm-strip run under qemu-user with an x86_64 root
  (extracted amd64 debs: libc6, libgcc-s1, libstdc++6, zlib1g) at
  `QEMU_LD_PREFIX` — `env.sh` defaults it to `~/.cache/localrecord/x86root`.
  `build-rust.sh` compiles with host clang-18/lld via a stand-in NDK
  (`scripts/host-ndk.sh`)
- Transcription speed and heat only mean something on the physical
  device; don't trust emulator or host numbers for it

## Conventions

- Rust: clear error types, no unwrap()/expect() in non-test code, prefer
  explicit over clever. Nothing exported may panic across the FFI (mutex
  locks recover from poisoning), and no C++ exception may reach Rust
  outside `native.rs`'s catch (`speakers.rs` still calls CAM++ through the
  sherpa-onnx crate, uncaught). `WhisperEngineError` is a uniffi
  `flat_error`, so Kotlin gets `WhisperEngineException` with the Display
  text as its message
- Kotlin: keep UI layer thin; all transcription logic lives in Rust,
  Kotlin just marshals audio buffers across the bridge and renders results.
  Nothing blocking (model load, `finish`, file IO) on the main thread
- No cloud transcription fallback — if on-device transcription fails, surface
  the error, don't silently fall back to a network service (defeats the
  whole point of the app). Recording itself must keep working without
  transcription

## Open questions / decisions pending

- Speech model: Parakeet TDT 0.6B v2 replaced Whisper. Whisper baseline
  (AMI excerpts, WER, host CPU): base.en 30.5% (28.4% live with a text
  prompt), small.en 24.0% beam 5; on the phone GPU small.en ~26% at ~0.2x
  real time. Parakeet is not yet measured on those excerpts. On the
  OnePlus 15 (1 thread, AMI meeting ES2004a): whole-recording pass at
  nice 10 0.074x real time (~4.4 min per hour of audio), peak ~1.6 GB RSS;
  live kept up on a 5-min clip (`finish` waited 6.6 s) at ~0.6 of a core
  on average. Host (aarch64 laptop CPU, 1 thread): 0.093x real time
- Tune `STEP_MS` / `HOLDBACK_MS` / `MAX_WINDOW_MS` and `DECODE_THREADS`
  once we have real device latency numbers; Parakeet decodes only the
  audio given (no padded 30 s window), so a shorter step may be affordable.
  If the final pass is cheap enough on the phone, consider running it
  automatically on stop
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
