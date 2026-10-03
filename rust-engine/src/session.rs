//! Live transcription via sliding-window chunking.
//!
//! Whisper has no native streaming mode, so a worker thread accumulates
//! captured audio and re-runs full inference over the uncommitted buffer every
//! `STEP_MS` of new audio. Segments ending at least `HOLDBACK_MS` before the
//! buffer end are complete: they are emitted and the buffer is trimmed to the
//! end of the last one, so the rest (including a word cut at the buffer edge)
//! is re-transcribed with more context on the next pass. Sizes are a first
//! guess — tune once real device latency numbers exist (see CLAUDE.md).

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use whisper_rs::WhisperState;

use crate::engine::{run_inference, Segment, WhisperEngine, WhisperEngineError};
use crate::pcm16_bytes_to_f32;

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

const STEP_SAMPLES: usize = STEP_MS * SAMPLES_PER_MS;
const MAX_WINDOW_SAMPLES: usize = MAX_WINDOW_MS * SAMPLES_PER_MS;
const MIN_AUDIO_SAMPLES: usize = MIN_AUDIO_MS * SAMPLES_PER_MS;

/// One live recording's transcription session. Created via
/// `WhisperEngine::create_live_session`. Thread-safe; intended flow:
/// audio thread calls `feed_pcm16`, UI polls `drain_segments`,
/// `finish` flushes the remainder and joins the worker.
#[derive(uniffi::Object)]
pub struct LiveSession {
    tx: Mutex<Option<Sender<Vec<f32>>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    segments: Arc<Mutex<Vec<Segment>>>,
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
    pub(crate) fn spawn(engine: Arc<WhisperEngine>) -> Result<Arc<Self>, WhisperEngineError> {
        let state = engine
            .ctx
            .create_state()
            .map_err(|e| WhisperEngineError::Inference { msg: e.to_string() })?;
        let (tx, rx) = mpsc::channel();
        let segments = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&segments);
        let handle = std::thread::Builder::new()
            .name("whisper-live".to_string())
            .spawn(move || worker_loop(&engine, state, &rx, &out))
            .map_err(|e| WhisperEngineError::Inference {
                msg: format!("failed to spawn inference worker: {e}"),
            })?;
        Ok(Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(handle)),
            segments,
        }))
    }
}

/// Lock, ignoring poisoning: every guarded value here stays consistent if a
/// holder panics, and a panic in an exported method would cross the FFI.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn worker_loop(
    engine: &WhisperEngine,
    mut state: WhisperState,
    rx: &Receiver<Vec<f32>>,
    out: &Mutex<Vec<Segment>>,
) {
    let mut window = Window::default();
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
        let committed =
            match run_inference(&mut state, &engine.config, &window.buf, window.start_ms()) {
                Ok(segments) => window.commit(segments),
                Err(e) => {
                    log::error!("window inference failed: {e}");
                    // Treat as silence so the buffer stays bounded.
                    window.commit(Vec::new())
                }
            };
        lock(out).extend(committed);
    }

    // Channel closed (finish or session dropped): flush whatever remains.
    if window.buf.is_empty() {
        return;
    }
    let start_ms = window.start_ms();
    let mut buf = window.buf;
    if buf.len() < MIN_AUDIO_SAMPLES {
        buf.resize(MIN_AUDIO_SAMPLES, 0.0);
    }
    match run_inference(&mut state, &engine.config, &buf, start_ms) {
        Ok(segments) => lock(out).extend(segments),
        Err(e) => log::error!("final flush inference failed: {e}"),
    }
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
    fn commit(&mut self, mut segments: Vec<Segment>) -> Vec<Segment> {
        self.new_samples = 0;
        let end_ms = to_ms(self.start + self.buf.len());
        let holdback_cutoff_ms = end_ms - HOLDBACK_MS as i64;
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
            segments.truncate(complete);
            segments.last().map_or(self.start_ms(), |s| s.end_ms)
        };
        self.trim_to(trim_to_ms);
        segments
    }

    fn trim_to(&mut self, ms: i64) {
        let ms = usize::try_from(ms - self.start_ms()).unwrap_or(0);
        let n = (ms * SAMPLES_PER_MS).min(self.buf.len());
        self.buf.drain(..n);
        self.start += n;
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
        assert!(w.commit(vec![seg(0, STEP_MS as i64)]).is_empty());
        assert_eq!(w.start_ms(), 0);
        assert_eq!(w.buf.len(), STEP_SAMPLES);
        assert!(!w.ready());
    }

    #[test]
    fn complete_segments_emitted_and_trimmed() {
        let mut w = window_of(5_000);
        let out = w.commit(vec![seg(0, 1_500), seg(1_500, 3_200), seg(3_200, 5_000)]);
        assert_eq!(out.len(), 2);
        assert_eq!(w.start_ms(), 3_200);
        assert_eq!(w.buf.len(), 1_800 * SAMPLES_PER_MS);
    }

    #[test]
    fn next_pass_is_offset_by_trimmed_audio() {
        let mut w = window_of(5_000);
        w.commit(vec![seg(0, 3_000), seg(3_000, 5_000)]);
        w.push(&vec![0.0; STEP_SAMPLES]);
        assert_eq!(w.start_ms(), 3_000);
        let out = w.commit(vec![seg(3_000, 6_500), seg(6_500, 9_000)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].end_ms, 6_500);
        assert_eq!(w.start_ms(), 6_500);
    }

    #[test]
    fn zero_length_segment_is_not_committed() {
        let mut w = window_of(13_000);
        let out = w.commit(vec![seg(0, 10_000), seg(10_000, 10_000)]);
        assert_eq!(out.len(), 1);
        assert_eq!(w.start_ms(), 10_000);
    }

    #[test]
    fn silence_keeps_only_holdback() {
        let mut w = window_of(5_000);
        assert!(w.commit(Vec::new()).is_empty());
        assert_eq!(w.start_ms(), 4_000);
        assert_eq!(w.buf.len(), HOLDBACK_MS * SAMPLES_PER_MS);
    }

    #[test]
    fn max_window_emits_everything() {
        let mut w = window_of(MAX_WINDOW_MS);
        let out = w.commit(vec![seg(0, 12_000), seg(12_000, MAX_WINDOW_MS as i64)]);
        assert_eq!(out.len(), 2);
        assert!(w.buf.is_empty());
        assert_eq!(w.start_ms(), MAX_WINDOW_MS as i64);
    }
}
