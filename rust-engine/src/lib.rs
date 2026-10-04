//! On-device Whisper transcription engine.
//!
//! Exposed to Kotlin via uniffi. Audio comes in as 16 kHz mono signed 16-bit
//! little-endian PCM bytes (`ByteArray` on the Kotlin side — uniffi's only
//! zero-boxing sequence type, which matters at 32 KB/s of live audio).

uniffi::setup_scaffolding!();

use std::sync::{Mutex, MutexGuard, PoisonError};

mod engine;
mod native;
mod session;
mod speakers;
mod wav;

pub use engine::{EngineConfig, Segment, WhisperEngine, WhisperEngineError, Word};
pub use session::LiveSession;
pub use speakers::SpeakerEncoder;

/// Route whisper.cpp / engine logs to logcat (or env_logger on host).
/// Call once from Kotlin before creating an engine.
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
    native::install_logging();
}

/// Lock, ignoring poisoning: every guarded value here stays consistent if a
/// holder panics, and a panic in an exported method would cross the FFI.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Convert s16le PCM bytes to the f32 samples whisper.cpp expects.
pub(crate) fn pcm16_bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect()
}
