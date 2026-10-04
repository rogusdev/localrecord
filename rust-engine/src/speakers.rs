//! Live speaker labels. Each committed segment's audio becomes a voiceprint
//! (speaker embedding: sherpa-onnx running 3D-Speaker's CAM++ "zh_en common
//! advanced" model) that is matched
//! against the speakers heard so far in the session; no match starts a new
//! speaker. One label per segment, so a segment spanning a turn change gets
//! whichever voice dominates it.

use std::sync::Arc;

use sherpa_onnx::{SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig};

use crate::engine::{Segment, WhisperEngineError};
use crate::session::SAMPLE_RATE_HZ;

/// Shorter segments give unreliable voiceprints; they keep the previous
/// segment's speaker instead.
const MIN_VOICEPRINT_MS: i64 = 1_000;
/// Cosine similarity to a speaker's mean voiceprint at or above which a
/// segment is that speaker. Best single value for this model over four
/// far-field AMI meetings, one headset mix and a clean dialog (93% of
/// segments right); 96% on held-out meeting EN2002a.
const SAME_SPEAKER_SIMILARITY: f32 = 0.35;
/// CPU threads for one voiceprint; runs on the live worker between passes.
const VOICEPRINT_THREADS: i32 = 2;

const SAMPLES_PER_MS: i64 = SAMPLE_RATE_HZ as i64 / 1000;

/// Loaded speaker-embedding model. Shareable across sessions.
#[derive(uniffi::Object)]
pub struct SpeakerEncoder {
    extractor: SpeakerEmbeddingExtractor,
}

#[uniffi::export]
impl SpeakerEncoder {
    #[uniffi::constructor]
    pub fn new(model_path: String) -> Result<Arc<Self>, WhisperEngineError> {
        let config = SpeakerEmbeddingExtractorConfig {
            model: Some(model_path.clone()),
            num_threads: VOICEPRINT_THREADS,
            ..Default::default()
        };
        let extractor =
            SpeakerEmbeddingExtractor::create(&config).ok_or_else(|| WhisperEngineError::ModelLoad {
                msg: format!("could not load speaker model {model_path}"),
            })?;
        log::info!("speaker model loaded from {model_path} (dim {})", extractor.dim());
        Ok(Arc::new(Self { extractor }))
    }
}

impl SpeakerEncoder {
    /// Voiceprint of 16 kHz mono `samples`; None if the model can't produce one.
    fn voiceprint(&self, samples: &[f32]) -> Option<Vec<f32>> {
        let stream = self.extractor.create_stream()?;
        stream.accept_waveform(SAMPLE_RATE_HZ as i32, samples);
        stream.input_finished();
        if !self.extractor.is_ready(&stream) {
            return None;
        }
        self.extractor.compute(&stream)
    }
}

/// Speaker memory for one live session.
pub(crate) struct SpeakerTracker {
    encoder: Arc<SpeakerEncoder>,
    speakers: Speakers,
    last: Option<u32>,
}

impl SpeakerTracker {
    pub(crate) fn new(encoder: Arc<SpeakerEncoder>) -> Self {
        Self {
            encoder,
            speakers: Speakers::default(),
            last: None,
        }
    }

    /// Set `speaker` on each of `segments`, whose audio is in `audio`
    /// (starting at recording time `audio_start_ms`).
    pub(crate) fn label(&mut self, segments: &mut [Segment], audio: &[f32], audio_start_ms: i64) {
        for segment in segments {
            let clip = clip(audio, audio_start_ms, segment.start_ms, segment.end_ms);
            let long_enough = clip.len() as i64 >= MIN_VOICEPRINT_MS * SAMPLES_PER_MS;
            let speaker = long_enough
                .then(|| self.encoder.voiceprint(clip))
                .flatten()
                .map(|voiceprint| self.speakers.assign(voiceprint))
                .or(self.last);
            segment.speaker = speaker;
            self.last = speaker;
        }
    }
}

/// The part of `audio` (starting at `audio_start_ms`) between two recording times.
fn clip(audio: &[f32], audio_start_ms: i64, start_ms: i64, end_ms: i64) -> &[f32] {
    let index = |ms: i64| {
        usize::try_from((ms - audio_start_ms) * SAMPLES_PER_MS)
            .unwrap_or(0)
            .min(audio.len())
    };
    &audio[index(start_ms)..index(end_ms).max(index(start_ms))]
}

/// Speakers as running sums of unit-length voiceprints; a sum's direction is
/// the speaker's mean voice.
#[derive(Default)]
struct Speakers {
    sums: Vec<Vec<f32>>,
}

impl Speakers {
    /// Index of the most similar speaker at or above the threshold (folding
    /// the voiceprint into it), else of a new speaker.
    fn assign(&mut self, voiceprint: Vec<f32>) -> u32 {
        let voiceprint = normalized(voiceprint);
        let best = self
            .sums
            .iter()
            .enumerate()
            .map(|(i, sum)| (i, dot(&normalized(sum.clone()), &voiceprint)))
            .filter(|&(_, similarity)| similarity >= SAME_SPEAKER_SIMILARITY)
            .max_by(|a, b| a.1.total_cmp(&b.1));
        let index = match best {
            Some((i, _)) => {
                self.sums[i].iter_mut().zip(&voiceprint).for_each(|(s, v)| *s += v);
                i
            }
            None => {
                self.sums.push(voiceprint);
                self.sums.len() - 1
            }
        };
        u32::try_from(index).unwrap_or(u32::MAX)
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn normalized(mut v: Vec<f32>) -> Vec<f32> {
    let norm = dot(&v, &v).sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_voiceprints_share_a_speaker() {
        let mut speakers = Speakers::default();
        assert_eq!(speakers.assign(vec![1.0, 0.1, 0.0]), 0);
        assert_eq!(speakers.assign(vec![0.0, 1.0, 0.1]), 1);
        assert_eq!(speakers.assign(vec![0.9, 0.2, 0.1]), 0);
        assert_eq!(speakers.assign(vec![0.1, 0.0, 1.0]), 2);
        assert_eq!(speakers.assign(vec![0.1, 0.8, 0.0]), 1);
    }

    #[test]
    fn clip_is_bounded_by_the_audio() {
        let audio: Vec<f32> = (0..32_000).map(|i| i as f32).collect(); // 2 s from t=1000 ms
        assert_eq!(clip(&audio, 1_000, 1_500, 2_000).len(), 8_000);
        assert_eq!(clip(&audio, 1_000, 500, 1_250)[0], 0.0);
        assert_eq!(clip(&audio, 1_000, 2_500, 9_000).len(), 8_000);
        assert!(clip(&audio, 1_000, 5_000, 6_000).is_empty());
    }
}
