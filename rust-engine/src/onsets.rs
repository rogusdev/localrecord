//! Word start times from the audio. The recognizer times words in 80 ms
//! encoder frames, and after a pause a word's start can land before the voice
//! comes back (by ~0.1-0.15 s on test speech). A start sitting in a pause
//! moves to the first loud frame after it. Only the start is checked: a word's
//! span can run on into the pause after it, so a later pause in the span is no
//! sign of where the word begins.

use crate::engine::Word;
use crate::session::SAMPLE_RATE_HZ;

/// 20 ms analysis frames.
pub(crate) const FRAME_SAMPLES: usize = SAMPLE_RATE_HZ / 50;
const FRAME_MS: i64 = 20;
/// A start moves only past at least this many quiet frames (100 ms), so a
/// soft first consonant keeps its start.
const MIN_PAUSE_FRAMES: usize = 5;
/// The noise floor is this percentile of frame loudness over the audio.
const NOISE_PERCENTILE: f32 = 0.1;
/// Frames louder than this many times the noise floor are voice.
const VOICE_OVER_NOISE: f32 = 2.5;
/// Voice threshold floor (RMS of samples in [-1, 1]), for near-digital
/// silence such as zero padding.
const MIN_VOICE_RMS: f32 = 0.002;

/// Move each word's start that sits in a pause to the first loud frame
/// after it, if that comes before the word's end. `samples` are the audio
/// the words were recognized in, starting at recording time `base_ms`.
pub(crate) fn refine_word_starts(words: &mut [Word], samples: &[f32], base_ms: i64) {
    let loudness = frame_loudness(samples);
    let Some(threshold) = voice_threshold(&loudness) else {
        return;
    };
    let frame_at = |ms: i64| usize::try_from((ms - base_ms) / FRAME_MS).unwrap_or(0);
    for word in words {
        let start = frame_at(word.start_ms);
        let end = frame_at(word.end_ms).min(loudness.len());
        let onset = (start..end).find(|&frame| loudness[frame] >= threshold);
        if let Some(frame) = onset.filter(|&frame| frame - start >= MIN_PAUSE_FRAMES) {
            let ms = base_ms + frame as i64 * FRAME_MS;
            word.start_ms = word.start_ms.max(ms);
        }
    }
}

/// RMS of each whole 20 ms frame of `samples`.
pub(crate) fn frame_loudness(samples: &[f32]) -> Vec<f32> {
    samples.chunks_exact(FRAME_SAMPLES).map(rms).collect()
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

    fn words_at(spans: &[(i64, i64)]) -> Vec<Word> {
        spans
            .iter()
            .map(|&(start_ms, end_ms)| Word {
                start_ms,
                end_ms,
                text: String::new(),
            })
            .collect()
    }

    fn starts(words: &[Word]) -> Vec<i64> {
        words.iter().map(|w| w.start_ms).collect()
    }

    #[test]
    fn start_moves_past_a_pause() {
        // voice 0-200 ms, pause 200-600, voice 600-1000
        let frames: Vec<f32> = (0..50)
            .map(|f| if (10..30).contains(&f) { NOISE } else { VOICE })
            .collect();
        let mut words = words_at(&[(0, 200), (200, 1000)]);
        refine_word_starts(&mut words, &audio(&frames), 0);
        assert_eq!(starts(&words), [0, 600]);
    }

    #[test]
    fn a_pause_later_in_the_span_keeps_the_start() {
        // voice 0-400 ms, pause 400-800, a breath at 800-820, quiet after
        let frames: Vec<f32> = (0..50)
            .map(|f| if f < 20 || f == 40 { VOICE } else { NOISE })
            .collect();
        let mut words = words_at(&[(0, 900)]);
        refine_word_starts(&mut words, &audio(&frames), 0);
        assert_eq!(starts(&words), [0]);
    }

    #[test]
    fn a_short_quiet_start_is_kept() {
        // 60 ms of quiet (a soft consonant) before the voice at 360 ms
        let frames: Vec<f32> = (0..50)
            .map(|f| if (15..18).contains(&f) { NOISE } else { VOICE })
            .collect();
        let mut words = words_at(&[(300, 800)]);
        refine_word_starts(&mut words, &audio(&frames), 0);
        assert_eq!(starts(&words), [300]);
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
        let mut words = words_at(&[(0, 300), (300, 600)]);
        refine_word_starts(&mut words, &audio(&frames), 0);
        assert_eq!(starts(&words), [0, 300]);
    }

    #[test]
    fn times_are_relative_to_base() {
        // leading silence 0-400 ms of the window, which starts at 5 s
        let frames: Vec<f32> = (0..50)
            .map(|f| if f < 20 { NOISE } else { VOICE })
            .collect();
        let mut words = words_at(&[(5_000, 5_900)]);
        refine_word_starts(&mut words, &audio(&frames), 5_000);
        assert_eq!(starts(&words), [5_400]);
    }

    #[test]
    fn silent_audio_changes_nothing() {
        let mut words = words_at(&[(0, 500)]);
        refine_word_starts(&mut words, &vec![0.0; 16_000], 0);
        assert_eq!(starts(&words), [0]);
    }
}
