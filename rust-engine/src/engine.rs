use std::ffi::c_int;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::chunks;
use crate::native::{NativeError, Recognizer};
use crate::onsets;
use crate::opus_file;
use crate::pcm16_bytes_to_f32;
use crate::session::{LiveSession, SAMPLE_RATE_HZ};
use crate::speakers::{self, SpeakerEncoder};
use crate::words;

/// ONNX Runtime threads per decode. With one it decodes on the calling
/// thread, so a final pass on a background-priority thread stays background
/// (a pool's threads are created at load and keep the loading thread's
/// priority). That lets live and final passes share one loaded model
/// (~1.2 GB) at their own priorities.
const DECODE_THREADS: c_int = 1;
/// Shorter audio is zero-padded to this before decoding: the recognizer
/// throws on empty input and made up words ("Mm-hmm.") from a fraction of a
/// second of silence, but not from a whole second of it.
const MIN_AUDIO_MS: usize = 1_100;

const SAMPLES_PER_MS: usize = SAMPLE_RATE_HZ / 1000;
const MIN_AUDIO_SAMPLES: usize = MIN_AUDIO_MS * SAMPLES_PER_MS;

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
    #[error("can't read audio file: {msg}")]
    AudioFile { msg: String },
    #[error("can't write audio file: {msg}")]
    AudioWrite { msg: String },
    #[error("live session already finished")]
    SessionFinished,
    #[error("can't read transcript: {msg}")]
    Transcript { msg: String },
}

/// One transcribed span. Times are relative to the start of the
/// recording/clip in milliseconds.
#[derive(Clone, Debug, uniffi::Record)]
pub struct Segment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    /// The segment's words, timed from the recognizer's tokens with starts
    /// refined from the audio.
    pub words: Vec<Word>,
    /// 0-based speaker within the session; None without a speaker model.
    pub speaker: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct Word {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// A loaded speech model (Parakeet TDT 0.6B v2, int8, run by sherpa-onnx).
/// Load once and share: live sessions and whole-recording passes can use it
/// at the same time.
#[derive(uniffi::Object)]
pub struct SpeechEngine {
    recognizer: Recognizer,
}

#[uniffi::export]
impl SpeechEngine {
    /// `model_dir` holds sherpa-onnx's int8 Parakeet export: encoder.int8.onnx,
    /// decoder.int8.onnx, joiner.int8.onnx and tokens.txt. Blocking (~2 s).
    #[uniffi::constructor]
    pub fn new(model_dir: String) -> Result<Arc<Self>, WhisperEngineError> {
        let started = Instant::now();
        let recognizer = Recognizer::load(Path::new(&model_dir), DECODE_THREADS)
            .map_err(|e| WhisperEngineError::ModelLoad { msg: e.to_string() })?;
        log::info!("model loaded from {model_dir} in {:?}", started.elapsed());
        Ok(Arc::new(Self { recognizer }))
    }

    /// Transcribe a complete clip of 16 kHz mono s16le PCM. Blocking; call
    /// from a background thread/dispatcher.
    pub fn transcribe_pcm16(&self, pcm: Vec<u8>) -> Result<Vec<Segment>, WhisperEngineError> {
        self.transcribe_samples(&pcm16_bytes_to_f32(&pcm), None)
    }

    /// Transcription of a whole recording (an Ogg Opus file as
    /// `RecordingWriter` writes), meant as the final pass after recording
    /// stops: decoded in 20-30 s pieces cut at pauses, so each has more
    /// context than a live pass got. With `speakers`, segments get speaker
    /// labels matched across the whole recording. Blocking, and runs at the
    /// calling thread's priority; call from a background thread.
    pub fn transcribe_recording(
        &self,
        path: String,
        speakers: Option<Arc<SpeakerEncoder>>,
    ) -> Result<Vec<Segment>, WhisperEngineError> {
        let samples = opus_file::read_samples(Path::new(&path))
            .map_err(|msg| WhisperEngineError::AudioFile { msg })?;
        self.transcribe_samples(&samples, speakers.as_deref())
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

impl SpeechEngine {
    fn transcribe_samples(
        &self,
        samples: &[f32],
        speakers: Option<&SpeakerEncoder>,
    ) -> Result<Vec<Segment>, WhisperEngineError> {
        if samples.is_empty() {
            return Err(WhisperEngineError::EmptyAudio);
        }
        let started = Instant::now();
        let mut words = Vec::new();
        for range in chunks::chunk_bounds(samples) {
            let base_ms = (range.start / SAMPLES_PER_MS) as i64;
            words.extend(self.recognize_words(&samples[range], base_ms)?);
        }
        let mut segments = words::segments_from_words(words);
        if let Some(encoder) = speakers {
            speakers::label_recording(encoder, &mut segments, samples);
        }
        log::info!(
            "whole-recording pass over {} s took {:?}",
            samples.len() / SAMPLE_RATE_HZ,
            started.elapsed()
        );
        Ok(segments)
    }

    /// The words in 16 kHz mono `samples`, which start at recording time
    /// `base_ms`. Keep `samples` to about `chunks::MAX_CHUNK_MS`.
    pub(crate) fn recognize_words(
        &self,
        samples: &[f32],
        base_ms: i64,
    ) -> Result<Vec<Word>, WhisperEngineError> {
        let tokens = if samples.len() < MIN_AUDIO_SAMPLES {
            let mut padded = samples.to_vec();
            padded.resize(MIN_AUDIO_SAMPLES, 0.0);
            self.recognizer.recognize(&padded)
        } else {
            self.recognizer.recognize(samples)
        }
        .map_err(inference_error)?;
        let mut words = words::words_from_tokens(&tokens, base_ms);
        onsets::refine_word_starts(&mut words, samples, base_ms);
        Ok(words)
    }
}

fn inference_error(e: NativeError) -> WhisperEngineError {
    WhisperEngineError::Inference { msg: e.to_string() }
}
