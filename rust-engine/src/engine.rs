use std::sync::Arc;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use crate::pcm16_bytes_to_f32;
use crate::session::LiveSession;

/// Flat so Kotlin exceptions carry the Display text below as their message.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum WhisperEngineError {
    #[error("failed to load model: {msg}")]
    ModelLoad { msg: String },
    #[error("inference failed: {msg}")]
    Inference { msg: String },
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
    pub(crate) ctx: Arc<WhisperContext>,
    pub(crate) config: EngineConfig,
}

#[uniffi::export]
impl WhisperEngine {
    #[uniffi::constructor]
    pub fn new(model_path: String, config: EngineConfig) -> Result<Arc<Self>, WhisperEngineError> {
        let mut ctx_params = WhisperContextParameters::default();
        ctx_params.use_gpu(config.use_gpu);
        let ctx = WhisperContext::new_with_params(&model_path, ctx_params)
            .map_err(|e| WhisperEngineError::ModelLoad { msg: e.to_string() })?;
        log::info!(
            "model loaded from {model_path} (use_gpu={})",
            config.use_gpu
        );
        Ok(Arc::new(Self {
            ctx: Arc::new(ctx),
            config,
        }))
    }

    /// Transcribe a complete clip of 16 kHz mono s16le PCM. Blocking; call
    /// from a background thread/dispatcher.
    pub fn transcribe_pcm16(&self, pcm: Vec<u8>) -> Result<Vec<Segment>, WhisperEngineError> {
        let samples = pcm16_bytes_to_f32(&pcm);
        if samples.is_empty() {
            return Err(WhisperEngineError::EmptyAudio);
        }
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| WhisperEngineError::Inference { msg: e.to_string() })?;
        run_inference(&mut state, &self.config, &samples, 0)
    }

    /// Start a live sliding-window transcription session. Feed audio as it
    /// is captured, poll `drain_segments`, then `finish` to flush the tail.
    pub fn create_live_session(self: Arc<Self>) -> Result<Arc<LiveSession>, WhisperEngineError> {
        LiveSession::spawn(self)
    }
}

/// Run whisper full inference over `samples`, returning segments offset by
/// `base_ms` (the recording-relative time of `samples[0]`).
pub(crate) fn run_inference(
    state: &mut WhisperState,
    config: &EngineConfig,
    samples: &[f32],
    base_ms: i64,
) -> Result<Vec<Segment>, WhisperEngineError> {
    let inference_err =
        |e: whisper_rs::WhisperError| WhisperEngineError::Inference { msg: e.to_string() };

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(i32::from(config.num_threads));
    params.set_language(Some(&config.language));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_no_context(true);
    params.set_suppress_blank(true);
    // Suppress non-speech tokens (as openai-whisper does by default) so
    // silence doesn't produce "[BLANK_AUDIO]" / "(music)" annotations.
    params.set_suppress_nst(true);

    state.full(params, samples).map_err(inference_err)?;

    let n_segments = state.full_n_segments();
    let mut out = Vec::with_capacity(n_segments as usize);
    for i in 0..n_segments {
        let Some(segment) = state.get_segment(i) else {
            continue;
        };
        let text = segment.to_str_lossy().map_err(inference_err)?;
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        // timestamps are in centiseconds
        out.push(Segment {
            start_ms: base_ms + segment.start_timestamp() * 10,
            end_ms: base_ms + segment.end_timestamp() * 10,
            text: text.to_string(),
        });
    }
    Ok(out)
}
