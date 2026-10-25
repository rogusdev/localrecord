//! Check of live chunking against a real model: feeds raw 16 kHz mono s16le
//! PCM through a `LiveSession` at real-time pace and prints its segments next
//! to a one-shot transcription of the same audio, with how long the one-shot
//! pass took and how long `finish` waited (live passes falling behind show up
//! there). Use when tuning the window sizes in src/session.rs or, given a
//! speaker model, the live speaker labels in src/speakers.rs. Runs on the
//! host, or on the phone over adb (built with cargo-ndk, next to the sherpa
//! and libc++ shared libs).
//!
//!   ffmpeg -i speech.wav -f s16le -ar 16000 -ac 1 speech.raw
//!   cargo run --release --example live_check -- <parakeet-model-dir> speech.raw [campplus.onnx]
//!
//! The model dir holds sherpa-onnx's int8 Parakeet TDT 0.6B v2 export
//! (encoder.int8.onnx, decoder.int8.onnx, joiner.int8.onnx, tokens.txt).

use std::error::Error;
use std::time::{Duration, Instant};

use whisper_engine::{Segment, SpeakerEncoder, SpeechEngine};

/// 100 ms of audio per feed, matching RecordingService's read size.
const CHUNK_BYTES: usize = 3_200;
/// 16 kHz mono s16.
const BYTES_PER_SECOND: f64 = 32_000.0;
const CHUNK_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(model), Some(pcm_path)) = (args.next(), args.next()) else {
        return Err("usage: live_check <model-dir> <audio.raw> [speaker-model.onnx]".into());
    };
    let speakers = args.next().map(SpeakerEncoder::new).transpose()?;
    let pcm = std::fs::read(pcm_path)?;
    let engine = SpeechEngine::new(model)?;

    let audio_s = pcm.len() as f64 / BYTES_PER_SECOND;

    let started = Instant::now();
    let one_shot = engine.transcribe_pcm16(pcm.clone())?;
    let took = started.elapsed().as_secs_f64();
    print_segments(
        &format!(
            "one-shot: {audio_s:.0} s of audio in {took:.1} s ({:.3}x real time)",
            took / audio_s
        ),
        &one_shot,
    );

    let session = engine.create_live_session(speakers)?;
    let mut live = Vec::new();
    for chunk in pcm.chunks(CHUNK_BYTES) {
        session.feed_pcm16(chunk.to_vec());
        std::thread::sleep(CHUNK_INTERVAL);
        live.extend(session.drain_segments());
    }
    let started = Instant::now();
    live.extend(session.finish()?);
    let finish_s = started.elapsed().as_secs_f64();
    print_segments(&format!("live: finish took {finish_s:.1} s"), &live);
    Ok(())
}

fn print_segments(label: &str, segments: &[Segment]) {
    println!("== {label}");
    for s in segments {
        let speaker = s.speaker.map_or(String::new(), |n| format!("S{} ", n + 1));
        println!("[{:>7} - {:>7}] {speaker}{}", s.start_ms, s.end_ms, s.text);
    }
}
