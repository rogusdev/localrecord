use std::ffi::{c_int, CStr, CString};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::native::{Context, FullParams, NativeError, State};
use crate::session::{LiveSession, MIN_AUDIO_SAMPLES};
use crate::speakers::{self, SpeakerEncoder};
use crate::wav;
use crate::{lock, pcm16_bytes_to_f32};

/// Read by ggml when it creates the Vulkan device (first model load). On the
/// Adreno 840 ggml's fp16 shaders end in VK_ERROR_DEVICE_LOST; fp32 is correct
/// and still ~2.5x faster than CPU once warm (base.en, 11.6 s clip: 0.84 s).
#[cfg(feature = "vulkan")]
const DISABLE_GPU_FP16_ENV: &str = "GGML_VK_DISABLE_F16";

/// Flash attention: Adreno 840 fp32, same clip, 0.68 s vs 0.84 s per pass.
const FLASH_ATTN: bool = true;
/// Beam width for whole-recording passes (whisper's default; live is greedy).
const WHOLE_RECORDING_BEAM_SIZE: c_int = 5;
/// A segment repeating one of the previous two word for word, with at least
/// this many words, is whisper's repetition loop rather than speech.
const LOOP_MIN_WORDS: usize = 4;

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
    #[error("can't read audio file: {msg}")]
    AudioFile { msg: String },
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
    /// Word timings from whisper's token timestamps: approximate (whisper.cpp
    /// estimates them from token probabilities), within the segment's span.
    pub words: Vec<Word>,
    /// 0-based speaker within the session; None without a speaker model.
    pub speaker: Option<u32>,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct Word {
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
        let engine = Arc::new(Self {
            ctx: Arc::new(ctx),
            config,
            language,
            failure: Mutex::new(None),
        });
        if engine.config.use_gpu {
            engine.warm_up();
        }
        Ok(engine)
    }

    /// Transcribe a complete clip of 16 kHz mono s16le PCM. Blocking; call
    /// from a background thread/dispatcher.
    pub fn transcribe_pcm16(&self, pcm: Vec<u8>) -> Result<Vec<Segment>, WhisperEngineError> {
        let samples = pcm16_bytes_to_f32(&pcm);
        if samples.is_empty() {
            return Err(WhisperEngineError::EmptyAudio);
        }
        let mut state = self.new_state()?;
        self.run_inference(&mut state, &samples, 0, &Pass::default())
    }

    /// Accurate transcription of a whole recording (16 kHz mono 16-bit WAV),
    /// meant as the final pass after recording stops: one call over all of
    /// it, with beam search. With `speakers`, segments get speaker labels.
    /// Blocking, and slow on long recordings; call from a background thread.
    pub fn transcribe_wav(
        &self,
        path: String,
        speakers: Option<Arc<SpeakerEncoder>>,
    ) -> Result<Vec<Segment>, WhisperEngineError> {
        let mut samples = wav::read_pcm16_mono_16k(Path::new(&path))
            .map_err(|msg| WhisperEngineError::AudioFile { msg })?;
        if samples.is_empty() {
            return Err(WhisperEngineError::EmptyAudio);
        }
        let audio_len = samples.len();
        if audio_len < MIN_AUDIO_SAMPLES {
            samples.resize(MIN_AUDIO_SAMPLES, 0.0);
        }
        let started = Instant::now();
        let mut state = self.new_state()?;
        let whole = Pass {
            whole_recording: true,
            ..Pass::default()
        };
        let mut segments = self.run_inference(&mut state, &samples, 0, &whole)?;
        drop_repeated_segments(&mut segments);
        if let Some(encoder) = speakers {
            speakers::label_recording(&encoder, &mut segments, &samples[..audio_len]);
        }
        log::info!(
            "final pass over {} s took {:?}",
            audio_len / crate::session::SAMPLE_RATE_HZ,
            started.elapsed()
        );
        Ok(segments)
    }

    /// Start a live sliding-window transcription session. Feed audio as it
    /// is captured, poll `drain_segments`, then `finish` to flush the tail.
    /// With `speakers`, committed segments also get speaker labels.
    pub fn create_live_session(
        self: Arc<Self>,
        speakers: Option<Arc<SpeakerEncoder>>,
    ) -> Result<Arc<LiveSession>, WhisperEngineError> {
        LiveSession::spawn(self, speakers)
    }
}

/// How one inference pass decodes.
#[derive(Default)]
pub(crate) struct Pass<'a> {
    /// Text that came just before the audio (live: the committed transcript).
    pub prompt: Option<&'a CStr>,
    /// The whole recording in one call, decoded with beam search.
    pub whole_recording: bool,
}

/// Drop whisper repetition loops: segments that repeat one of the two before
/// them word for word (ignoring case and punctuation) and have at least
/// `LOOP_MIN_WORDS` words.
fn drop_repeated_segments(segments: &mut Vec<Segment>) {
    let words = |text: &str| -> Vec<String> {
        text.split_whitespace()
            .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase())
            .filter(|w| !w.is_empty())
            .collect()
    };
    let mut recent: Vec<Vec<String>> = Vec::new();
    segments.retain(|segment| {
        let key = words(&segment.text);
        let repeat = key.len() >= LOOP_MIN_WORDS && recent.iter().rev().take(2).any(|r| *r == key);
        if !repeat {
            recent.push(key);
        }
        !repeat
    });
}


impl WhisperEngine {
    /// The first GPU pass compiles ggml's Vulkan pipelines (~2 s on the
    /// Adreno 840). Run one on silence at load so the first live pass isn't
    /// late. A backend failure here is recorded like any other.
    fn warm_up(&self) {
        let started = Instant::now();
        let silence = vec![0.0; MIN_AUDIO_SAMPLES];
        match self
            .new_state()
            .and_then(|mut state| self.run_inference(&mut state, &silence, 0, &Pass::default()))
        {
            Ok(_) => log::info!("GPU warm-up took {:?}", started.elapsed()),
            Err(e) => log::error!("GPU warm-up failed: {e}"),
        }
    }

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
        pass: &Pass<'_>,
    ) -> Result<Vec<Segment>, WhisperEngineError> {
        self.check_usable()?;
        let mut params = if pass.whole_recording {
            FullParams::beam_search(&self.language, WHOLE_RECORDING_BEAM_SIZE)
        } else {
            FullParams::greedy(&self.language)
        };
        if let Some(prompt) = pass.prompt {
            params.set_prompt(prompt);
        }
        let raw = params.raw_mut();
        raw.n_threads = c_int::from(self.config.num_threads);
        raw.print_special = false;
        raw.print_progress = false;
        raw.print_realtime = false;
        raw.print_timestamps = false;
        // No text carried between whisper's own 30 s windows: live passes
        // re-read overlapping audio, and in whole-recording passes it set off a
        // repetition loop on the phone (AMI TS3003a) for no WER gain on host.
        // Context comes only from `prompt`.
        raw.no_context = true;
        raw.suppress_blank = true;
        // Suppress non-speech tokens (as openai-whisper does by default) so
        // silence doesn't produce "[BLANK_AUDIO]" / "(music)" annotations.
        raw.suppress_nst = true;
        // Per-token times, grouped into words for playback highlighting.
        raw.token_timestamps = true;

        state.full(&params, samples).map_err(|e| self.record(e))?;

        let segments = state
            .segments()
            .into_iter()
            .filter_map(|s| {
                let text = s.text.trim();
                if text.is_empty() {
                    return None;
                }
                // timestamps are in centiseconds
                let to_ms = |t: i64| base_ms + t.clamp(s.t0, s.t1) * 10;
                let words = s
                    .words
                    .into_iter()
                    .map(|w| Word {
                        start_ms: to_ms(w.t0),
                        end_ms: to_ms(w.t1),
                        text: w.text,
                    })
                    .collect();
                Some(Segment {
                    start_ms: base_ms + s.t0 * 10,
                    end_ms: base_ms + s.t1 * 10,
                    text: text.to_string(),
                    words,
                    speaker: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str) -> Segment {
        Segment {
            start_ms: 0,
            end_ms: 0,
            text: text.to_string(),
            words: Vec::new(),
            speaker: None,
        }
    }

    #[test]
    fn repetition_loops_are_dropped() {
        let mut segments = vec![
            seg("We're going to draw an animal."),
            seg("we're going to draw an animal"),
            seg("Okay."),
            seg("We're going to draw an animal."),
            seg("Yes."),
            seg("Yes."),
            seg("Then the next thing we do is this."),
        ];
        drop_repeated_segments(&mut segments);
        let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            ["We're going to draw an animal.", "Okay.", "Yes.", "Yes.", "Then the next thing we do is this."]
        );
    }
}
