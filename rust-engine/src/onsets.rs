//! Word start times from the audio. DTW alignment gives good word ends, but a
//! word's start is only the previous word's end, so after a pause it starts
//! too early. This moves such a start to where the voice comes back: the first
//! loud frame after the last pause inside the word's span.

use crate::engine::Segment;
use crate::session::SAMPLE_RATE_HZ;

/// 20 ms analysis frames.
const FRAME_SAMPLES: usize = SAMPLE_RATE_HZ / 50;
const FRAME_MS: i64 = 20;
/// A pause is at least this many quiet frames in a row (100 ms).
const MIN_PAUSE_FRAMES: usize = 5;
/// The noise floor is this percentile of frame loudness over the audio.
const NOISE_PERCENTILE: f32 = 0.1;
/// Frames louder than this many times the noise floor are voice.
const VOICE_OVER_NOISE: f32 = 2.5;
/// Voice threshold floor (RMS of samples in [-1, 1]), for near-digital
/// silence such as zero padding.
const MIN_VOICE_RMS: f32 = 0.002;

/// Move each word's start past the last pause in its span (start..end).
/// `samples` are the audio the segments were transcribed from, starting at
/// recording time `base_ms`.
pub(crate) fn refine_word_starts(segments: &mut [Segment], samples: &[f32], base_ms: i64) {
    let loudness: Vec<f32> = samples.chunks_exact(FRAME_SAMPLES).map(rms).collect();
    let Some(threshold) = voice_threshold(&loudness) else {
        return;
    };
    let frame_at = |ms: i64| usize::try_from((ms - base_ms) / FRAME_MS).unwrap_or(0);
    for word in segments.iter_mut().flat_map(|s| s.words.iter_mut()) {
        let span = frame_at(word.start_ms)..frame_at(word.end_ms).min(loudness.len());
        let mut quiet_run = 0;
        let mut onset = None;
        for frame in span {
            if loudness[frame] < threshold {
                quiet_run += 1;
            } else {
                if quiet_run >= MIN_PAUSE_FRAMES {
                    onset = Some(frame);
                }
                quiet_run = 0;
            }
        }
        if let Some(frame) = onset {
            let ms = base_ms + frame as i64 * FRAME_MS;
            word.start_ms = word.start_ms.max(ms);
        }
    }
}

fn rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt()
}

fn voice_threshold(loudness: &[f32]) -> Option<f32> {
    let mut sorted = loudness.to_vec();
    sorted.sort_by(f32::total_cmp);
    let noise = sorted.get((sorted.len() as f32 * NOISE_PERCENTILE) as usize)?;
    Some((noise * VOICE_OVER_NOISE).max(MIN_VOICE_RMS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Word;

    const NOISE: f32 = 0.01;
    const VOICE: f32 = 0.3;

    /// Audio of 20 ms frames, each at one of two levels (a square wave so
    /// the RMS is exact).
    fn audio(frames: &[f32]) -> Vec<f32> {
        frames
            .iter()
            .flat_map(|&level| {
                (0..FRAME_SAMPLES).map(move |i| if i % 2 == 0 { level } else { -level })
            })
            .collect()
    }

    fn segment(words: &[(i64, i64)]) -> Segment {
        Segment {
            start_ms: 0,
            end_ms: words.last().map_or(0, |w| w.1),
            text: String::new(),
            words: words
                .iter()
                .map(|&(start_ms, end_ms)| Word {
                    start_ms,
                    end_ms,
                    text: String::new(),
                })
                .collect(),
            speaker: None,
        }
    }

    fn starts(segments: &[Segment]) -> Vec<i64> {
        segments[0].words.iter().map(|w| w.start_ms).collect()
    }

    #[test]
    fn start_moves_past_a_pause() {
        // voice 0-200 ms, pause 200-600, voice 600-1000
        let frames: Vec<f32> = (0..50)
            .map(|f| if (10..30).contains(&f) { NOISE } else { VOICE })
            .collect();
        let mut segments = vec![segment(&[(0, 200), (200, 1000)])];
        refine_word_starts(&mut segments, &audio(&frames), 0);
        assert_eq!(starts(&segments), [0, 600]);
    }

    #[test]
    fn short_gaps_and_no_pause_keep_the_start() {
        // a 60 ms dip at 400 ms inside speech, silence from 600 ms
        let frames: Vec<f32> = (0..50)
            .map(|f| {
                if (20..23).contains(&f) || f >= 30 {
                    NOISE
                } else {
                    VOICE
                }
            })
            .collect();
        let mut segments = vec![segment(&[(0, 300), (300, 600)])];
        refine_word_starts(&mut segments, &audio(&frames), 0);
        assert_eq!(starts(&segments), [0, 300]);
    }

    #[test]
    fn times_are_relative_to_base() {
        // leading silence 0-400 ms of the window, which starts at 5 s
        let frames: Vec<f32> = (0..50)
            .map(|f| if f < 20 { NOISE } else { VOICE })
            .collect();
        let mut segments = vec![segment(&[(5_000, 5_900)])];
        refine_word_starts(&mut segments, &audio(&frames), 5_000);
        assert_eq!(starts(&segments), [5_400]);
    }

    #[test]
    fn silent_audio_changes_nothing() {
        let mut segments = vec![segment(&[(0, 500)])];
        refine_word_starts(&mut segments, &vec![0.0; 16_000], 0);
        assert_eq!(starts(&segments), [0]);
    }
}
