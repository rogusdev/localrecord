use std::ffi::{c_int, CString};
use std::sync::{Arc, Mutex};

use crate::native::{Context, FullParams, NativeError, State};
use crate::session::LiveSession;
use crate::{lock, pcm16_bytes_to_f32};

/// Read by ggml when it creates the Vulkan device (first model load). On the
/// Adreno 840 ggml's fp16 shaders end in VK_ERROR_DEVICE_LOST; fp32 is correct
/// and still ~2.5x faster than CPU once warm (base.en, 11.6 s clip: 0.84 s).
#[cfg(feature = "vulkan")]
const DISABLE_GPU_FP16_ENV: &str = "GGML_VK_DISABLE_F16";

/// Flash attention: Adreno 840 fp32, same clip, 0.68 s vs 0.84 s per pass.
const FLASH_ATTN: bool = true;

/// Flat so Kotlin exceptions carry the Display text below as their message.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum WhisperEngineError {
    #[error("failed to load model: {msg}")]
    ModelLoad { msg: String },
    #[error("inference failed: {msg}")]
    Inference { msg: String },
    /// whisper.cpp/ggml threw (e.g. GPU device lost). The engine refuses
    /// further inference until the app restarts.
    #[error("transcription backend failed: {msg}")]
    Backend { msg: String },
    #[error("audio buffer is empty")]
    EmptyAudio,
    #[error("live session already finished")]
    SessionFinished,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct EngineConfig {
    /// Use the Vulkan GPU backend when the crate is built with it.
    /// whisper.cpp falls back to CPU if no usable device is found.
    #[uniffi(default = true)]
    pub use_gpu: bool,
    /// CPU threads for the parts that stay on CPU (and full CPU fallback).
    #[uniffi(default = 4)]
    pub num_threads: u8,
    /// ISO 639-1 language hint; "en" for the *.en models.
    #[uniffi(default = "en")]
    pub language: String,
}

/// One transcribed span. Times are relative to the start of the
/// recording/clip in milliseconds.
#[derive(Clone, Debug, uniffi::Record)]
pub struct Segment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// A loaded Whisper model. Cheap to share; create one per model file and
/// reuse it across recordings. Inference state is per-call / per-session.
#[derive(uniffi::Object)]
pub struct WhisperEngine {
    ctx: Arc<Context>,
    config: EngineConfig,
    /// `config.language` as whisper.cpp takes it.
    language: CString,
    /// First backend exception. The GPU backend may be unusable after one,
    /// so later inference fails fast with it instead of running.
    failure: Mutex<Option<String>>,
}

#[uniffi::export]
impl WhisperEngine {
    #[uniffi::constructor]
    pub fn new(model_path: String, config: EngineConfig) -> Result<Arc<Self>, WhisperEngineError> {
        #[cfg(feature = "vulkan")]
        if config.use_gpu {
            std::env::set_var(DISABLE_GPU_FP16_ENV, "1");
        }
        let language = CString::new(config.language.as_str()).map_err(|_| {
            WhisperEngineError::ModelLoad {
                msg: "language contains a NUL byte".to_string(),
            }
        })?;
        let ctx = Context::load(&model_path, config.use_gpu, FLASH_ATTN)
            .map_err(|e| WhisperEngineError::ModelLoad { msg: e.to_string() })?;
        log::info!(
            "model loaded from {model_path} (use_gpu={}, flash_attn={FLASH_ATTN})",
            config.use_gpu
        );
        Ok(Arc::new(Self {
            ctx: Arc::new(ctx),
            config,
            language,
            failure: Mutex::new(None),
        }))
    }

    /// Transcribe a complete clip of 16 kHz mono s16le PCM. Blocking; call
    /// from a background thread/dispatcher.
    pub fn transcribe_pcm16(&self, pcm: Vec<u8>) -> Result<Vec<Segment>, WhisperEngineError> {
        let samples = pcm16_bytes_to_f32(&pcm);
        if samples.is_empty() {
            return Err(WhisperEngineError::EmptyAudio);
        }
        let mut state = self.new_state()?;
        self.run_inference(&mut state, &samples, 0)
    }

    /// Start a live sliding-window transcription session. Feed audio as it
    /// is captured, poll `drain_segments`, then `finish` to flush the tail.
    pub fn create_live_session(self: Arc<Self>) -> Result<Arc<LiveSession>, WhisperEngineError> {
        LiveSession::spawn(self)
    }
}

impl WhisperEngine {
    pub(crate) fn new_state(&self) -> Result<State, WhisperEngineError> {
        self.check_usable()?;
        State::new(Arc::clone(&self.ctx)).map_err(|e| self.record(e))
    }

    /// Run whisper full inference over `samples`, returning segments offset
    /// by `base_ms` (the recording-relative time of `samples[0]`).
    pub(crate) fn run_inference(
        &self,
        state: &mut State,
        samples: &[f32],
        base_ms: i64,
    ) -> Result<Vec<Segment>, WhisperEngineError> {
        self.check_usable()?;
        let mut params = FullParams::greedy(&self.language);
        let raw = params.raw_mut();
        raw.n_threads = c_int::from(self.config.num_threads);
        raw.print_special = false;
        raw.print_progress = false;
        raw.print_realtime = false;
        raw.print_timestamps = false;
        raw.no_context = true;
        raw.suppress_blank = true;
        // Suppress non-speech tokens (as openai-whisper does by default) so
        // silence doesn't produce "[BLANK_AUDIO]" / "(music)" annotations.
        raw.suppress_nst = true;

        state.full(&params, samples).map_err(|e| self.record(e))?;

        let segments = state
            .segments()
            .into_iter()
            .filter_map(|s| {
                let text = s.text.trim();
                // timestamps are in centiseconds
                (!text.is_empty()).then(|| Segment {
                    start_ms: base_ms + s.t0 * 10,
                    end_ms: base_ms + s.t1 * 10,
                    text: text.to_string(),
                })
            })
            .collect();
        Ok(segments)
    }

    /// Convert a native error, remembering the first backend exception.
    fn record(&self, e: NativeError) -> WhisperEngineError {
        match e {
            NativeError::Exception(msg) => {
                lock(&self.failure).get_or_insert_with(|| msg.clone());
                WhisperEngineError::Backend { msg }
            }
            NativeError::Failed(msg) => WhisperEngineError::Inference { msg },
        }
    }

    fn check_usable(&self) -> Result<(), WhisperEngineError> {
        match lock(&self.failure).as_ref() {
            Some(msg) => Err(WhisperEngineError::Backend { msg: msg.clone() }),
            None => Ok(()),
        }
    }
}
