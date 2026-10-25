//! A whole recording is decoded in pieces of 20–30 s, each cut at the
//! quietest moment in its last 10 s so that no word is split.

use std::ops::Range;

use crate::onsets::{frame_loudness, FRAME_SAMPLES};
use crate::session::SAMPLE_RATE_HZ;

/// Every piece but the last is at least this long.
const MIN_CHUNK_MS: usize = 20_000;
/// The longest audio decoded at once. Parakeet attends over all of it, so a
/// decode's time and memory grow faster than its length (host, int8: 37 s
/// took ~0.3 GB on top of the loaded model, 66 s ~0.75 GB).
pub(crate) const MAX_CHUNK_MS: usize = 30_000;
/// Loudness is compared over windows of this many 20 ms frames (200 ms), so a
/// cut lands in a pause rather than in a stop consonant.
const QUIET_WINDOW_FRAMES: usize = 10;

const SAMPLES_PER_MS: usize = SAMPLE_RATE_HZ / 1000;
const MIN_CHUNK_SAMPLES: usize = MIN_CHUNK_MS * SAMPLES_PER_MS;
const MAX_CHUNK_SAMPLES: usize = MAX_CHUNK_MS * SAMPLES_PER_MS;

/// Sample ranges covering `samples` in order, each at most `MAX_CHUNK_MS`.
pub(crate) fn chunk_bounds(samples: &[f32]) -> Vec<Range<usize>> {
    let loudness = frame_loudness(samples);
    let window = |frame: usize| -> f32 {
        loudness
            .get(frame..frame + QUIET_WINDOW_FRAMES)
            .map_or(f32::MAX, |frames| frames.iter().sum())
    };
    let mut bounds = Vec::new();
    let mut start = 0;
    while samples.len() - start > MAX_CHUNK_SAMPLES {
        let first = (start + MIN_CHUNK_SAMPLES) / FRAME_SAMPLES;
        let last = (start + MAX_CHUNK_SAMPLES) / FRAME_SAMPLES - QUIET_WINDOW_FRAMES;
        let quietest = (first..=last)
            .min_by(|&a, &b| window(a).total_cmp(&window(b)))
            .unwrap_or(first);
        let cut = (quietest + QUIET_WINDOW_FRAMES / 2) * FRAME_SAMPLES;
        bounds.push(start..cut);
        start = cut;
    }
    bounds.push(start..samples.len());
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOUD: f32 = 0.3;
    const QUIET: f32 = 0.01;

    /// `ms` of audio at `level` (a square wave, so the RMS is exact).
    fn audio(ms: usize, level: f32) -> Vec<f32> {
        (0..ms * SAMPLES_PER_MS)
            .map(|i| if i % 2 == 0 { level } else { -level })
            .collect()
    }

    #[test]
    fn short_audio_is_one_chunk() {
        let bounds = chunk_bounds(&audio(MAX_CHUNK_MS, LOUD));
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds.first(), Some(&(0..MAX_CHUNK_SAMPLES)));
        let bounds = chunk_bounds(&[]);
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds.first(), Some(&(0..0)));
    }

    #[test]
    fn cut_lands_in_the_pause() {
        // speech, a 400 ms pause at 24.0-24.4 s, speech
        let mut samples = audio(24_000, LOUD);
        samples.extend(audio(400, QUIET));
        samples.extend(audio(16_000, LOUD));
        let bounds = chunk_bounds(&samples);
        assert_eq!(bounds.len(), 2);
        let cut_ms = bounds[0].end / SAMPLES_PER_MS;
        assert!((24_000..24_400).contains(&cut_ms), "cut at {cut_ms} ms");
        assert_eq!(bounds[1], bounds[0].end..samples.len());
    }

    #[test]
    fn long_audio_is_covered_by_bounded_chunks() {
        let samples = audio(95_000, LOUD);
        let bounds = chunk_bounds(&samples);
        assert_eq!(bounds.first().map(|r| r.start), Some(0));
        assert_eq!(bounds.last().map(|r| r.end), Some(samples.len()));
        for pair in bounds.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        for range in &bounds[..bounds.len() - 1] {
            assert!((MIN_CHUNK_SAMPLES..=MAX_CHUNK_SAMPLES).contains(&range.len()));
        }
        assert!(bounds.last().is_some_and(|r| r.len() <= MAX_CHUNK_SAMPLES));
    }
}
