//! On-device speech transcription engine: Parakeet TDT run by sherpa-onnx.
//!
//! Exposed to Kotlin via uniffi. Audio comes in as 16 kHz mono signed 16-bit
//! little-endian PCM bytes (`ByteArray` on the Kotlin side — uniffi's only
//! zero-boxing sequence type, which matters at 32 KB/s of live audio).

uniffi::setup_scaffolding!();

use std::sync::{Mutex, MutexGuard, PoisonError};

mod chunks;
mod engine;
mod native;
mod ogg;
mod onsets;
mod opus_file;
mod session;
mod speakers;
mod vtt;
mod words;

pub use engine::{Segment, SpeechEngine, WhisperEngineError, Word};
pub use opus_file::{recording_duration_ms, RecordingFile, RecordingStream, RecordingWriter};
pub use session::LiveSession;
pub use speakers::SpeakerEncoder;
pub use vtt::{transcript_from_vtt, transcript_to_vtt};

/// Route engine logs to logcat (or env_logger on host); sherpa-onnx logs to
/// logcat itself. Call once from Kotlin before creating an engine.
#[uniffi::export]
pub fn init_logging() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("whisper-engine"),
    );
    #[cfg(not(target_os = "android"))]
    {
        let _ = env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Info)
            .try_init();
    }
}

/// Lock, ignoring poisoning: every guarded value here stays consistent if a
/// holder panics, and a panic in an exported method would cross the FFI.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Convert s16le PCM bytes to the f32 samples the engine works on.
pub(crate) fn pcm16_bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect()
}
