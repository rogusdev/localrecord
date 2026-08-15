//! Live transcription via sliding-window chunking.
//!
//! Whisper has no native streaming mode, so a worker thread accumulates
//! captured audio and re-runs full inference on a window whenever enough new
//! audio has arrived. Segments that end before the window's overlap tail are
//! considered stable and emitted; the tail is carried into the next window so
//! words spanning a window boundary aren't split. Window/overlap sizes are a
//! first guess — tune once real device latency numbers exist (see CLAUDE.md).

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use crate::engine::{run_inference, Segment, WhisperEngine, WhisperEngineError};
use crate::pcm16_bytes_to_f32;

pub const SAMPLE_RATE_HZ: usize = 16_000;
const SAMPLES_PER_MS: usize = SAMPLE_RATE_HZ / 1000;
/// Inference window length.
const WINDOW_MS: usize = 5_000;
/// Tail carried over into the next window to avoid splitting words.
const OVERLAP_MS: usize = 1_000;
/// whisper.cpp rejects buffers shorter than ~1 s; pad the final flush.
const MIN_AUDIO_MS: usize = 1_100;

const WINDOW_SAMPLES: usize = WINDOW_MS * SAMPLES_PER_MS;
const OVERLAP_SAMPLES: usize = OVERLAP_MS * SAMPLES_PER_MS;
const MIN_AUDIO_SAMPLES: usize = MIN_AUDIO_MS * SAMPLES_PER_MS;

enum Command {
    Audio(Vec<f32>),
}

/// One live recording's transcription session. Created via
/// `WhisperEngine::create_live_session`. Thread-safe; intended flow:
/// audio thread calls `feed_pcm16`, UI polls `drain_segments`,
/// `finish` flushes the remainder and joins the worker.
#[derive(uniffi::Object)]
pub struct LiveSession {
    tx: Mutex<Option<Sender<Command>>>,
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
        let tx = self.tx.lock().expect("session tx lock poisoned");
        match tx.as_ref() {
            Some(tx) => {
                if tx.send(Command::Audio(samples)).is_err() {
                    log::error!("live session worker died; dropping audio");
                }
            }
            None => log::warn!("feed_pcm16 after finish; dropping audio"),
        }
    }

    /// Take all newly stable segments since the last call.
    pub fn drain_segments(&self) -> Vec<Segment> {
        std::mem::take(&mut *self.segments.lock().expect("segments lock poisoned"))
    }

    /// Flush remaining audio, stop the worker, and return any segments not
    /// yet drained (including the flushed tail). Blocking.
    pub fn finish(&self) -> Result<Vec<Segment>, WhisperEngineError> {
        {
            let mut tx = self.tx.lock().expect("session tx lock poisoned");
            if tx.take().is_none() {
                return Err(WhisperEngineError::SessionFinished);
            }
            // dropping the sender ends the worker's recv loop
        }
        let handle = self
            .worker
            .lock()
            .expect("worker lock poisoned")
            .take()
            .ok_or(WhisperEngineError::SessionFinished)?;
        handle.join().map_err(|_| WhisperEngineError::Inference {
            msg: "inference worker panicked".to_string(),
        })?;
        Ok(self.drain_segments())
    }
}

impl LiveSession {
    pub(crate) fn spawn(engine: Arc<WhisperEngine>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let segments = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&segments);
        let handle = std::thread::Builder::new()
            .name("whisper-live".to_string())
            .spawn(move || worker_loop(engine, rx, out))
            .expect("failed to spawn whisper worker thread");
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(handle)),
            segments,
        })
    }
}

fn worker_loop(engine: Arc<WhisperEngine>, rx: Receiver<Command>, out: Arc<Mutex<Vec<Segment>>>) {
    let mut state = match engine.ctx.create_state() {
        Ok(s) => s,
        Err(e) => {
            log::error!("failed to create whisper state: {e}");
            return;
        }
    };

    let mut buf: Vec<f32> = Vec::with_capacity(WINDOW_SAMPLES * 2);
    // Recording-relative time of buf[0].
    let mut base_ms: i64 = 0;
    // End time of the last emitted segment; used to skip re-emitting text
    // that the overlap region already produced in the previous window.
    let mut watermark_ms: i64 = 0;

    while let Ok(Command::Audio(samples)) = rx.recv() {
        buf.extend_from_slice(&samples);
        while buf.len() >= WINDOW_SAMPLES {
            let window = &buf[..WINDOW_SAMPLES];
            match run_inference(&mut state, &engine.config, window, base_ms) {
                Ok(segments) => {
                    let window_end_ms = base_ms + WINDOW_MS as i64;
                    let stable_cutoff_ms = window_end_ms - OVERLAP_MS as i64;
                    let mut stable: Vec<Segment> = segments
                        .into_iter()
                        .filter(|s| s.end_ms <= stable_cutoff_ms && s.end_ms > watermark_ms)
                        .collect();
                    if let Some(last) = stable.last() {
                        watermark_ms = last.end_ms;
                    }
                    if !stable.is_empty() {
                        out.lock().expect("segments lock poisoned").append(&mut stable);
                    }
                }
                Err(e) => log::error!("window inference failed: {e}"),
            }
            // Slide: drop everything except the overlap tail.
            let drained = WINDOW_SAMPLES - OVERLAP_SAMPLES;
            buf.drain(..drained);
            base_ms += (drained / SAMPLES_PER_MS) as i64;
        }
    }

    // Channel closed (finish or sender dropped): flush whatever remains.
    if !buf.is_empty() {
        if buf.len() < MIN_AUDIO_SAMPLES {
            buf.resize(MIN_AUDIO_SAMPLES, 0.0);
        }
        match run_inference(&mut state, &engine.config, &buf, base_ms) {
            Ok(segments) => {
                let mut fresh: Vec<Segment> = segments
                    .into_iter()
                    .filter(|s| s.end_ms > watermark_ms)
                    .collect();
                if !fresh.is_empty() {
                    out.lock().expect("segments lock poisoned").append(&mut fresh);
                }
            }
            Err(e) => log::error!("final flush inference failed: {e}"),
        }
    }
}
