//! Recognizer tokens to words, and words to transcript segments. Parakeet
//! returns one token sequence per decode and no segments of its own; here a
//! segment ends at a sentence end, before a pause, or once it gets long.

use crate::engine::{Segment, Word};
use crate::native::Token;

/// A pause between two words at least this long ends a segment.
const SEGMENT_PAUSE_MS: i64 = 1_000;
/// A segment ends at the first word end at least this long after its start.
const MAX_SEGMENT_MS: i64 = 10_000;
/// A word ending in one of these ends a sentence, and with it a segment.
const SENTENCE_ENDS: [char; 3] = ['.', '?', '!'];

/// Group tokens into words: a token with a leading space starts a word, any
/// other (word pieces, punctuation) extends the current one. A word runs from
/// its first token's start to the end of its last spoken token (punctuation
/// is emitted in the pause after a word). `base_ms` is the recording time of
/// the decoded audio's start.
pub(crate) fn words_from_tokens(tokens: &[Token], base_ms: i64) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut starts_word = true;
    for token in tokens {
        let text = token.text.trim_start();
        starts_word |= text.len() != token.text.len();
        if text.is_empty() {
            continue;
        }
        let start_ms = base_ms + secs_to_ms(token.start_s);
        let end_ms = base_ms + secs_to_ms(token.start_s + token.duration_s);
        match words.last_mut() {
            Some(word) if !starts_word => {
                word.text.push_str(text);
                if text.chars().any(char::is_alphanumeric) {
                    word.end_ms = end_ms;
                }
            }
            _ => words.push(Word {
                start_ms,
                end_ms,
                text: text.to_string(),
            }),
        }
        starts_word = false;
    }
    words
}

/// Split words into segments (see the module docs); a segment's text is its
/// words joined by spaces.
pub(crate) fn segments_from_words(words: Vec<Word>) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut current: Vec<Word> = Vec::new();
    let mut words = words.into_iter().peekable();
    while let Some(word) = words.next() {
        let sentence_end = word.text.ends_with(SENTENCE_ENDS);
        let end_ms = word.end_ms;
        current.push(word);
        let long = end_ms - current[0].start_ms >= MAX_SEGMENT_MS;
        let pause_or_end = words
            .peek()
            .is_none_or(|next| next.start_ms - end_ms >= SEGMENT_PAUSE_MS);
        if sentence_end || long || pause_or_end {
            segments.push(segment_of(std::mem::take(&mut current)));
        }
    }
    segments
}

fn segment_of(words: Vec<Word>) -> Segment {
    Segment {
        start_ms: words.first().map_or(0, |w| w.start_ms),
        end_ms: words.last().map_or(0, |w| w.end_ms),
        text: words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        words,
        speaker: None,
    }
}

fn secs_to_ms(secs: f32) -> i64 {
    // `as` saturates; recognizer times are small and non-negative.
    (secs * 1000.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(text: &str, start_s: f32, duration_s: f32) -> Token {
        Token {
            text: text.to_string(),
            start_s,
            duration_s,
        }
    }

    fn word(start_ms: i64, end_ms: i64, text: &str) -> Word {
        Word {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    fn texts(segments: &[Segment]) -> Vec<&str> {
        segments.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn tokens_group_into_words() {
        // as Parakeet emits "Well, I don't see Phebe."
        let tokens = [
            token(" Well", 0.4, 0.24),
            token(",", 0.64, 0.08),
            token(" I", 0.72, 0.16),
            token(" don", 0.88, 0.16),
            token("'", 1.04, 0.0),
            token("t", 1.04, 0.0),
            token(" see", 1.44, 0.16),
            token(" P", 2.64, 0.08),
            token("he", 2.72, 0.0),
            token("be", 2.88, 0.16),
            token(".", 3.12, 0.24),
        ];
        assert_eq!(
            words_from_tokens(&tokens, 10_000),
            [
                word(10_400, 10_640, "Well,"),
                word(10_720, 10_880, "I"),
                word(10_880, 11_040, "don't"),
                word(11_440, 11_600, "see"),
                word(12_640, 13_040, "Phebe."),
            ]
        );
    }

    #[test]
    fn first_token_without_space_and_bare_space_tokens_start_words() {
        let tokens = [
            token("Hi", 0.0, 0.08),
            token(" ", 0.08, 0.0),
            token("5", 0.16, 0.08),
        ];
        assert_eq!(
            words_from_tokens(&tokens, 0),
            [word(0, 80, "Hi"), word(160, 240, "5")]
        );
    }

    #[test]
    fn segments_end_at_sentence_ends_and_pauses() {
        let words = vec![
            word(0, 300, "Hello"),
            word(300, 600, "there."),
            word(700, 900, "How"),
            word(900, 1_200, "are"),
            word(2_500, 2_800, "you"),
            word(2_800, 3_000, "doing?"),
            word(3_100, 3_300, "Fine"),
        ];
        let segments = segments_from_words(words);
        assert_eq!(
            texts(&segments),
            ["Hello there.", "How are", "you doing?", "Fine"]
        );
        assert_eq!((segments[1].start_ms, segments[1].end_ms), (700, 1_200));
        assert_eq!(segments[1].words.len(), 2);
    }

    #[test]
    fn long_run_on_speech_is_split() {
        let words: Vec<Word> = (0..30)
            .map(|i| word(i * 500, i * 500 + 400, "and"))
            .collect();
        let segments = segments_from_words(words);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].end_ms, 10_400);
        assert_eq!(segments[1].start_ms, 10_500);
    }

    #[test]
    fn no_words_no_segments() {
        assert!(segments_from_words(Vec::new()).is_empty());
    }
}
