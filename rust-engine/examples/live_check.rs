//! Host-side check of live chunking against a real model: feeds raw 16 kHz
//! mono s16le PCM through a `LiveSession` at real-time pace and prints its
//! segments next to a one-shot transcription of the same audio. Use when
//! tuning the window sizes in src/session.rs (CPU only on host).
//!
//!   ffmpeg -i speech.wav -f s16le -ar 16000 -ac 1 speech.raw
//!   cargo run --release --example live_check -- ggml-base.en-q5_1.bin speech.raw

use std::error::Error;
use std::time::Duration;

use whisper_engine::{EngineConfig, Segment, WhisperEngine};

/// 100 ms of audio per feed, matching RecordingService's read size.
const CHUNK_BYTES: usize = 3_200;
const CHUNK_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(model), Some(pcm_path)) = (args.next(), args.next()) else {
        return Err("usage: live_check <model.bin> <audio.raw>".into());
    };
    let pcm = std::fs::read(pcm_path)?;
    let config = EngineConfig {
        use_gpu: false,
        num_threads: 4,
        language: "en".to_string(),
    };
    let engine = WhisperEngine::new(model, config)?;

    print_segments("one-shot", &engine.transcribe_pcm16(pcm.clone())?);

    let session = engine.create_live_session()?;
    let mut live = Vec::new();
    for chunk in pcm.chunks(CHUNK_BYTES) {
        session.feed_pcm16(chunk.to_vec());
        std::thread::sleep(CHUNK_INTERVAL);
        live.extend(session.drain_segments());
    }
    live.extend(session.finish()?);
    print_segments("live", &live);
    Ok(())
}

fn print_segments(label: &str, segments: &[Segment]) {
    println!("== {label}");
    for s in segments {
        println!("[{:>7} - {:>7}] {}", s.start_ms, s.end_ms, s.text);
    }
}
