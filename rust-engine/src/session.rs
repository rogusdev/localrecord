//! Live transcription via sliding-window chunking.
//!
//! Whisper has no native streaming mode, so a worker thread accumulates
//! captured audio and re-runs full inference over the uncommitted buffer every
//! `STEP_MS` of new audio. Segments ending at least `HOLDBACK_MS` before the
//! buffer end are complete: they are emitted and the buffer is trimmed to the
//! end of the last one, so the rest (including a word cut at the buffer edge)
//! is re-transcribed with more context on the next pass. Sizes are a first
//! guess — tune once real device latency numbers exist (see CLAUDE.md).

use std::ffi::CString;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use crate::engine::{Pass, Segment, WhisperEngine, WhisperEngineError};
use crate::native::State;
use crate::speakers::{SpeakerEncoder, SpeakerTracker};
use crate::{lock, pcm16_bytes_to_f32};

pub const SAMPLE_RATE_HZ: usize = 16_000;
const SAMPLES_PER_MS: usize = SAMPLE_RATE_HZ / 1000;
/// New audio between inference passes.
const STEP_MS: usize = 4_000;
/// Segments ending this close to the buffer end may be cut mid-word; they are
/// held back for the next pass.
const HOLDBACK_MS: usize = 1_000;
/// Once the uncommitted buffer reaches this length everything is emitted.
/// MAX_WINDOW_MS + STEP_MS must stay under whisper's 30 s context so one pass
/// covers the whole buffer.
const MAX_WINDOW_MS: usize = 20_000;
/// whisper.cpp rejects buffers shorter than ~1 s; pad the final flush.
const MIN_AUDIO_MS: usize = 1_100;
/// Trailing committed text given to each pass as decoder context (whisper
/// keeps at most ~224 prompt tokens; 200 characters is ~45).
const PROMPT_CHARS: usize = 200;

const STEP_SAMPLES: usize = STEP_MS * SAMPLES_PER_MS;
const MAX_WINDOW_SAMPLES: usize = MAX_WINDOW_MS * SAMPLES_PER_MS;
pub(crate) const MIN_AUDIO_SAMPLES: usize = MIN_AUDIO_MS * SAMPLES_PER_MS;

/// One live recording's transcription session. Created via
/// `WhisperEngine::create_live_session`. Thread-safe; intended flow:
/// audio thread calls `feed_pcm16`, UI polls `drain_segments` and
/// `tentative`, `finish` flushes the remainder and joins the worker.
#[derive(uniffi::Object)]
pub struct LiveSession {
    tx: Mutex<Option<Sender<Vec<f32>>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    segments: Arc<Mutex<Vec<Segment>>>,
    /// The latest pass's held-back segments, replaced every pass.
    tentative: Arc<Mutex<Vec<Segment>>>,
    /// Set when the backend fails mid-session; transcription stops for good.
    failure: Arc<Mutex<Option<String>>>,
}

#[uniffi::export]
impl LiveSession {
    /// Feed captured 16 kHz mono s16le PCM bytes. Non-blocking: audio is
    /// queued for the inference worker. No-op after `finish`.
    pub fn feed_pcm16(&self, pcm: Vec<u8>) {
        let samples = pcm16_bytes_to_f32(&pcm);
        if samples.is_empty() {
            return;
        }
        match lock(&self.tx).as_ref() {
            Some(tx) => {
                if tx.send(samples).is_err() {
                    log::error!("live session worker died; dropping audio");
                }
            }
            None => log::warn!("feed_pcm16 after finish; dropping audio"),
        }
    }

    /// Take all newly stable segments since the last call.
    pub fn drain_segments(&self) -> Vec<Segment> {
        std::mem::take(&mut *lock(&self.segments))
    }

    /// Text after the last committed segment as the latest pass heard it:
    /// shown as a preview, it may still change before it is committed.
    pub fn tentative(&self) -> Vec<Segment> {
        lock(&self.tentative).clone()
    }

    /// Why transcription stopped mid-session, if it did. Audio fed afterwards
    /// is accepted and dropped, so recording can carry on.
    pub fn failure(&self) -> Option<String> {
        lock(&self.failure).clone()
    }

    /// Flush remaining audio, stop the worker, and return any segments not
    /// yet drained (including the flushed tail). Blocking.
    pub fn finish(&self) -> Result<Vec<Segment>, WhisperEngineError> {
        // dropping the sender ends the worker's recv loop
        if lock(&self.tx).take().is_none() {
            return Err(WhisperEngineError::SessionFinished);
        }
        let handle = lock(&self.worker)
            .take()
            .ok_or(WhisperEngineError::SessionFinished)?;
        handle.join().map_err(|_| WhisperEngineError::Inference {
            msg: "inference worker panicked".to_string(),
        })?;
        Ok(self.drain_segments())
    }
}

impl LiveSession {
    pub(crate) fn spawn(
        engine: Arc<WhisperEngine>,
        speakers: Option<Arc<SpeakerEncoder>>,
    ) -> Result<Arc<Self>, WhisperEngineError> {
        let state = engine.new_state()?;
        let tracker = speakers.map(SpeakerTracker::new);
        let (tx, rx) = mpsc::channel();
        let segments = Arc::new(Mutex::new(Vec::new()));
        let tentative = Arc::new(Mutex::new(Vec::new()));
        let failure = Arc::new(Mutex::new(None));
        let shared = Shared {
            out: Arc::clone(&segments),
            tentative: Arc::clone(&tentative),
            failure: Arc::clone(&failure),
        };
        let handle = std::thread::Builder::new()
            .name("whisper-live".to_string())
            .spawn(move || worker_loop(&engine, state, tracker, &rx, &shared))
            .map_err(|e| WhisperEngineError::Inference {
                msg: format!("failed to spawn inference worker: {e}"),
            })?;
        Ok(Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(handle)),
            segments,
            tentative,
            failure,
        }))
    }
}

/// What the worker publishes to the `LiveSession`.
struct Shared {
    out: Arc<Mutex<Vec<Segment>>>,
    tentative: Arc<Mutex<Vec<Segment>>>,
    failure: Arc<Mutex<Option<String>>>,
}

fn worker_loop(
    engine: &WhisperEngine,
    mut state: State,
    mut speakers: Option<SpeakerTracker>,
    rx: &Receiver<Vec<f32>>,
    shared: &Shared,
) {
    let mut window = Window::default();
    let mut context = Context::default();
    while let Ok(samples) = rx.recv() {
        window.push(&samples);
        // Fold any backlog into one pass so a slow device catches up instead
        // of queueing audio without bound.
        for samples in rx.try_iter() {
            window.push(&samples);
        }
        if !window.ready() {
            continue;
        }
        let prompt = context.prompt();
        let pass = Pass {
            prompt: prompt.as_deref(),
            ..Pass::default()
        };
        let mut committed =
            match engine.run_inference(&mut state, &window.buf, window.start_ms(), &pass) {
                Ok(segments) => window.commit(segments),
                Err(WhisperEngineError::Backend { msg }) => {
                    log::error!(
                        "transcription backend failed; recording continues untranscribed: {msg}"
                    );
                    *lock(&shared.failure) = Some(msg);
                    lock(&shared.tentative).clear();
                    // Drain until finish so feed_pcm16 doesn't see a dead worker.
                    for _ in rx.iter() {}
                    return;
                }
                Err(e) => {
                    log::error!("window inference failed: {e}");
                    // Treat as silence so the buffer stays bounded.
                    window.commit(Vec::new())
                }
            };
        if let Some(tracker) = speakers.as_mut() {
            tracker.label(
                &mut committed.segments,
                &committed.audio,
                committed.audio_start_ms,
            );
        }
        context.extend(&committed.segments);
        lock(&shared.out).extend(committed.segments);
        *lock(&shared.tentative) = committed.pending;
    }

    // Channel closed (finish or session dropped): flush whatever remains.
    if !window.buf.is_empty() {
        flush(
            engine,
            &mut state,
            speakers.as_mut(),
            &window,
            &context,
            shared,
        );
    }
    lock(&shared.tentative).clear();
}

fn flush(
    engine: &WhisperEngine,
    state: &mut State,
    speakers: Option<&mut SpeakerTracker>,
    window: &Window,
    context: &Context,
    shared: &Shared,
) {
    let start_ms = window.start_ms();
    let mut buf = window.buf.clone();
    if buf.len() < MIN_AUDIO_SAMPLES {
        buf.resize(MIN_AUDIO_SAMPLES, 0.0);
    }
    let prompt = context.prompt();
    let pass = Pass {
        prompt: prompt.as_deref(),
        ..Pass::default()
    };
    match engine.run_inference(state, &buf, start_ms, &pass) {
        Ok(mut segments) => {
            if let Some(tracker) = speakers {
                tracker.label(&mut segments, &window.buf, start_ms);
            }
            lock(&shared.out).extend(segments);
        }
        Err(WhisperEngineError::Backend { msg }) => {
            log::error!("transcription backend failed in final flush: {msg}");
            *lock(&shared.failure) = Some(msg);
        }
        Err(e) => log::error!("final flush inference failed: {e}"),
    }
}

/// The tail of the committed transcript, given to each pass as its prompt.
#[derive(Default)]
struct Context {
    text: String,
}

impl Context {
    fn extend(&mut self, segments: &[Segment]) {
        for segment in segments {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            self.text.push_str(&segment.text);
        }
        // Keep the last PROMPT_CHARS characters, from a word start.
        let chars = self.text.chars().count();
        if chars > PROMPT_CHARS {
            let cut = self
                .text
                .char_indices()
                .nth(chars - PROMPT_CHARS)
                .map_or(0, |(i, _)| i);
            let start = self.text[cut..].find(' ').map_or(cut, |i| cut + i + 1);
            self.text.drain(..start);
        }
    }

    fn prompt(&self) -> Option<CString> {
        if self.text.is_empty() {
            return None;
        }
        CString::new(self.text.as_str()).ok()
    }
}

/// Segments a pass made final, with the audio trimmed off along with them.
struct Committed {
    segments: Vec<Segment>,
    /// The pass's held-back segments (not final; re-transcribed next pass).
    pending: Vec<Segment>,
    /// Covers every segment above; starts at recording time `audio_start_ms`.
    audio: Vec<f32>,
    audio_start_ms: i64,
}

/// Uncommitted audio and the rules for what to emit after each pass. Kept
/// free of whisper so the windowing logic is unit-testable.
#[derive(Default)]
struct Window {
    buf: Vec<f32>,
    /// Recording-relative sample index of `buf[0]`.
    start: usize,
    /// Samples appended since the last pass.
    new_samples: usize,
}

impl Window {
    fn push(&mut self, samples: &[f32]) {
        self.buf.extend_from_slice(samples);
        self.new_samples += samples.len();
    }

    fn ready(&self) -> bool {
        self.new_samples >= STEP_SAMPLES
    }

    fn start_ms(&self) -> i64 {
        to_ms(self.start)
    }

    /// Take the segments from a pass over `buf`, return the complete ones, and
    /// trim the buffer past them. Audio is only dropped once its text has been
    /// emitted, or when the pass found no speech in it.
    fn commit(&mut self, mut segments: Vec<Segment>) -> Committed {
        self.new_samples = 0;
        let end_ms = to_ms(self.start + self.buf.len());
        let holdback_cutoff_ms = end_ms - HOLDBACK_MS as i64;
        let mut pending = Vec::new();
        let trim_to_ms = if self.buf.len() >= MAX_WINDOW_SAMPLES {
            end_ms
        } else if segments.is_empty() {
            // Keep the tail in case speech is just starting at the edge.
            holdback_cutoff_ms
        } else {
            // A zero-length segment is whisper's trailing guess at audio it
            // ran out of (its text can be hallucinated); treat it as incomplete.
            let complete = segments
                .iter()
                .take_while(|s| s.end_ms > s.start_ms && s.end_ms <= holdback_cutoff_ms)
                .count();
            pending = segments.split_off(complete);
            pending.retain(|s| s.end_ms > s.start_ms);
            segments.last().map_or(self.start_ms(), |s| s.end_ms)
        };
        let audio_start_ms = self.start_ms();
        let audio = self.trim_to(trim_to_ms);
        Committed {
            segments,
            pending,
            audio,
            audio_start_ms,
        }
    }

    /// Drop audio before recording time `ms`, returning it.
    fn trim_to(&mut self, ms: i64) -> Vec<f32> {
        let ms = usize::try_from(ms - self.start_ms()).unwrap_or(0);
        let n = (ms * SAMPLES_PER_MS).min(self.buf.len());
        self.start += n;
        self.buf.drain(..n).collect()
    }
}

fn to_ms(samples: usize) -> i64 {
    (samples / SAMPLES_PER_MS) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_ms: i64, end_ms: i64) -> Segment {
        Segment {
            start_ms,
            end_ms,
            text: format!("{start_ms}-{end_ms}"),
            words: Vec::new(),
            speaker: None,
        }
    }

    fn window_of(ms: usize) -> Window {
        let mut w = Window::default();
        w.push(&vec![0.0; ms * SAMPLES_PER_MS]);
        w
    }

    #[test]
    fn segment_running_to_buffer_end_is_kept_not_dropped() {
        let mut w = window_of(STEP_MS);
        assert!(w.ready());
        assert!(w.commit(vec![seg(0, STEP_MS as i64)]).segments.is_empty());
        assert_eq!(w.start_ms(), 0);
        assert_eq!(w.buf.len(), STEP_SAMPLES);
        assert!(!w.ready());
    }

    #[test]
    fn complete_segments_emitted_and_trimmed() {
        let mut w = window_of(5_000);
        let committed = w.commit(vec![seg(0, 1_500), seg(1_500, 3_200), seg(3_200, 5_000)]);
        let out = committed.segments;
        assert_eq!(out.len(), 2);
        assert_eq!(committed.audio_start_ms, 0);
        assert_eq!(committed.audio.len(), 3_200 * SAMPLES_PER_MS);
        assert_eq!(w.start_ms(), 3_200);
        assert_eq!(w.buf.len(), 1_800 * SAMPLES_PER_MS);
    }

    #[test]
    fn next_pass_is_offset_by_trimmed_audio() {
        let mut w = window_of(5_000);
        w.commit(vec![seg(0, 3_000), seg(3_000, 5_000)]);
        w.push(&vec![0.0; STEP_SAMPLES]);
        assert_eq!(w.start_ms(), 3_000);
        let out = w
            .commit(vec![seg(3_000, 6_500), seg(6_500, 9_000)])
            .segments;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].end_ms, 6_500);
        assert_eq!(w.start_ms(), 6_500);
    }

    #[test]
    fn zero_length_segment_is_not_committed() {
        let mut w = window_of(13_000);
        let out = w.commit(vec![seg(0, 10_000), seg(10_000, 10_000)]).segments;
        assert_eq!(out.len(), 1);
        assert_eq!(w.start_ms(), 10_000);
    }

    #[test]
    fn held_back_segments_are_pending_except_zero_length() {
        let mut w = window_of(9_000);
        let c = w.commit(vec![seg(0, 4_000), seg(4_000, 8_500), seg(8_500, 8_500)]);
        assert_eq!(c.segments.len(), 1);
        assert_eq!(c.pending.len(), 1);
        assert_eq!(c.pending[0].end_ms, 8_500);
    }

    #[test]
    fn context_keeps_the_tail_from_a_word_start() {
        let mut context = Context::default();
        let mut s = seg(0, 1);
        s.text = "word ".repeat(100).trim_end().to_string();
        context.extend(&[s]);
        assert!(context.text.len() <= PROMPT_CHARS);
        assert!(context.text.starts_with("word"));
        assert!(context.prompt().is_some());
    }

    #[test]
    fn silence_keeps_only_holdback() {
        let mut w = window_of(5_000);
        assert!(w.commit(Vec::new()).segments.is_empty());
        assert_eq!(w.start_ms(), 4_000);
        assert_eq!(w.buf.len(), HOLDBACK_MS * SAMPLES_PER_MS);
    }

    #[test]
    fn max_window_emits_everything() {
        let mut w = window_of(MAX_WINDOW_MS);
        let out = w
            .commit(vec![seg(0, 12_000), seg(12_000, MAX_WINDOW_MS as i64)])
            .segments;
        assert_eq!(out.len(), 2);
        assert!(w.buf.is_empty());
        assert_eq!(w.start_ms(), MAX_WINDOW_MS as i64);
    }
}
