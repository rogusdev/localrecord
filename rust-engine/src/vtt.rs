//! The app's transcript files: WebVTT, one cue per segment, the speaker as a
//! voice span and every word preceded by its start time:
//!
//! ```text
//! WEBVTT
//!
//! 00:00:01.240 --> 00:00:04.800
//! <v Speaker 1><00:00:01.240>Hello <00:00:01.900>there.
//! ```
//!
//! Word end times aren't stored: read back, a word ends where the next one
//! starts (the last at the cue end), and a segment's text is its words joined
//! by spaces. Segments without words are written as plain cue text.

use crate::engine::{Segment, WhisperEngineError, Word};

const HEADER: &str = "WEBVTT";
const ARROW: &str = "-->";
const VOICE_TAG: &str = "<v ";
const VOICE_END_TAG: &str = "</v>";
/// Voice span names are "Speaker n", n 1-based; `Segment::speaker` is 0-based.
const SPEAKER_NAME: &str = "Speaker ";
const BYTE_ORDER_MARK: char = '\u{feff}';

/// The transcript as a WebVTT file.
#[uniffi::export]
pub fn transcript_to_vtt(segments: Vec<Segment>) -> String {
    let mut out = format!("{HEADER}\n");
    for seg in &segments {
        out.push('\n');
        out.push_str(&format!(
            "{} {ARROW} {}\n",
            timestamp(seg.start_ms),
            timestamp(seg.end_ms)
        ));
        if let Some(speaker) = seg.speaker {
            out.push_str(&format!(
                "{VOICE_TAG}{SPEAKER_NAME}{}>",
                u64::from(speaker) + 1
            ));
        }
        if seg.words.is_empty() {
            out.push_str(&escape(&seg.text));
        } else {
            let words: Vec<String> = seg
                .words
                .iter()
                .map(|w| format!("<{}>{}", timestamp(w.start_ms), escape(&w.text)))
                .collect();
            out.push_str(&words.join(" "));
        }
        out.push('\n');
    }
    out
}

/// Reads what [`transcript_to_vtt`] writes. Blocks without a timing line
/// (NOTE, STYLE, header metadata) are skipped; a malformed cue is an error.
#[uniffi::export]
pub fn transcript_from_vtt(vtt: String) -> Result<Vec<Segment>, WhisperEngineError> {
    let vtt = vtt.strip_prefix(BYTE_ORDER_MARK).unwrap_or(&vtt);
    let mut lines = vtt.lines().enumerate().map(|(i, line)| (i + 1, line));
    match lines.next() {
        Some((_, first))
            if first
                .strip_prefix(HEADER)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t'])) => {}
        _ => return Err(error(1, "missing WEBVTT header")),
    }
    let mut segments = Vec::new();
    let mut block = Vec::new();
    for (number, line) in lines.chain(std::iter::once((0, ""))) {
        if !line.trim().is_empty() {
            block.push((number, line));
            continue;
        }
        if let Some(segment) = parse_block(&block)? {
            segments.push(segment);
        }
        block.clear();
    }
    Ok(segments)
}

/// A cue block: optional identifier line, timing line, text lines.
fn parse_block(block: &[(usize, &str)]) -> Result<Option<Segment>, WhisperEngineError> {
    let Some(timing) = block.iter().position(|(_, line)| line.contains(ARROW)) else {
        return Ok(None);
    };
    let (number, timing_line) = block[timing];
    let (start, rest) = timing_line.split_once(ARROW).unwrap_or_default();
    let end = rest.split_whitespace().next().unwrap_or_default();
    let (Some(start_ms), Some(end_ms)) = (parse_timestamp(start.trim()), parse_timestamp(end))
    else {
        return Err(error(number, "bad cue timing"));
    };
    let text: Vec<&str> = block[timing + 1..]
        .iter()
        .map(|(_, line)| line.trim())
        .collect();
    parse_cue_text(&text.join(" "), start_ms, end_ms)
        .map(Some)
        .map_err(|msg| error(number + 1, msg))
}

fn parse_cue_text(text: &str, start_ms: i64, end_ms: i64) -> Result<Segment, &'static str> {
    let mut text = text.trim();
    text = text.strip_suffix(VOICE_END_TAG).unwrap_or(text);
    let mut speaker = None;
    if let Some(voice) = text.strip_prefix(VOICE_TAG) {
        let (name, rest) = voice.split_once('>').ok_or("unclosed voice tag")?;
        speaker = name
            .strip_prefix(SPEAKER_NAME)
            .and_then(|n| n.trim().parse::<u32>().ok())
            .and_then(|n| n.checked_sub(1));
        text = rest;
    }

    // Escaped text holds no raw '<', so every one opens a word timestamp.
    let mut chunks = text.split('<');
    let lead = chunks.next().unwrap_or_default().trim();
    let mut starts = Vec::new();
    for chunk in chunks {
        let (tag, word) = chunk.split_once('>').ok_or("unclosed tag")?;
        let start = parse_timestamp(tag).ok_or("tag isn't a word timestamp")?;
        starts.push((start, unescape(word.trim())));
    }
    if starts.is_empty() {
        return Ok(Segment {
            start_ms,
            end_ms,
            text: unescape(lead),
            words: Vec::new(),
            speaker,
        });
    }
    if !lead.is_empty() {
        return Err("text before the first word timestamp");
    }
    let ends = starts
        .iter()
        .skip(1)
        .map(|(start, _)| *start)
        .chain([end_ms]);
    let words: Vec<Word> = starts
        .iter()
        .zip(ends)
        .map(|((start, text), end)| Word {
            start_ms: *start,
            end_ms: end,
            text: text.clone(),
        })
        .collect();
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Segment {
        start_ms,
        end_ms,
        text,
        words,
        speaker,
    })
}

/// hh:mm:ss.mmm; negative times are written as 0.
fn timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// [hh:]mm:ss.mmm, hours of any width, minutes and seconds two digits < 60.
fn parse_timestamp(text: &str) -> Option<i64> {
    let (hms, millis) = text.split_once('.')?;
    let fields: Vec<&str> = hms.split(':').collect();
    let (hours, minutes, seconds) = match fields.as_slice() {
        [h, m, s] => (number(h, None)?, *m, *s),
        [m, s] => (0, *m, *s),
        _ => return None,
    };
    let minutes = number(minutes, Some(2)).filter(|m| *m < 60)?;
    let seconds = number(seconds, Some(2)).filter(|s| *s < 60)?;
    let millis = number(millis, Some(3))?;
    Some(((hours * 60 + minutes) * 60 + seconds) * 1000 + millis)
}

/// Decimal digits only (no sign), optionally of an exact width.
fn number(text: &str, width: Option<usize>) -> Option<i64> {
    let digits_ok = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let width_ok = width.is_none_or(|w| text.len() == w);
    (digits_ok && width_ok).then(|| text.parse().ok()).flatten()
}

/// Cue text can't hold a raw '<' or '&' (or "-->"), nor line breaks.
fn escape(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn error(line: usize, msg: &str) -> WhisperEngineError {
    WhisperEngineError::Transcript {
        msg: format!("line {line}: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(start_ms: i64, end_ms: i64, text: &str) -> Word {
        Word {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    fn segment(start_ms: i64, end_ms: i64, words: Vec<Word>, speaker: Option<u32>) -> Segment {
        let text = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        Segment {
            start_ms,
            end_ms,
            text,
            words,
            speaker,
        }
    }

    fn assert_same(a: &[Segment], b: &[Segment]) {
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    #[test]
    fn writes_cues_with_speakers_and_word_timestamps() {
        let vtt = transcript_to_vtt(vec![
            segment(
                1240,
                4800,
                vec![word(1240, 1900, "Hello"), word(1900, 4800, "there.")],
                Some(0),
            ),
            Segment {
                start_ms: 8000,
                end_ms: 9000,
                text: " no\nwords ".into(),
                words: vec![],
                speaker: None,
            },
        ]);
        assert_eq!(
            vtt,
            "WEBVTT\n\n\
             00:00:01.240 --> 00:00:04.800\n\
             <v Speaker 1><00:00:01.240>Hello <00:00:01.900>there.\n\n\
             00:00:08.000 --> 00:00:09.000\n\
             no words\n"
        );
    }

    #[test]
    fn round_trips() {
        let segments = vec![
            segment(
                1240,
                4800,
                vec![word(1240, 1900, "Hello"), word(1900, 4800, "there.")],
                Some(0),
            ),
            segment(
                3_725_100,
                3_727_350,
                vec![
                    word(3_725_100, 3_725_420, "A"),
                    word(3_725_420, 3_726_000, "<b>-->"),
                    word(3_726_000, 3_727_350, "&lt;"),
                ],
                Some(1),
            ),
            segment(10_000, 11_000, vec![word(10_000, 11_000, "Solo")], None),
            Segment {
                start_ms: 12_000,
                end_ms: 13_000,
                text: "no words".into(),
                words: vec![],
                speaker: Some(2),
            },
        ];
        let read = transcript_from_vtt(transcript_to_vtt(segments.clone())).unwrap();
        assert_same(&read, &segments);
    }

    #[test]
    fn word_ends_at_next_start_then_cue_end() {
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:05.000\n<00:00:01.500>a <00:00:02.000>b\n";
        let words = &transcript_from_vtt(vtt.into()).unwrap()[0].words;
        assert_eq!((words[0].start_ms, words[0].end_ms), (1500, 2000));
        assert_eq!((words[1].start_ms, words[1].end_ms), (2000, 5000));
    }

    #[test]
    fn reads_other_vtt_conventions() {
        let vtt = "\u{feff}WEBVTT - transcript\r\nKind: captions\r\n\r\n\
                   NOTE written by hand\r\n\r\n\
                   cue-1\r\n01:02.500 --> 01:04.000 align:start\r\n<v Speaker 3>two\r\nlines</v>\r\n";
        let segments = transcript_from_vtt(vtt.into()).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!((segments[0].start_ms, segments[0].end_ms), (62_500, 64_000));
        assert_eq!(segments[0].text, "two lines");
        assert_eq!(segments[0].speaker, Some(2));
    }

    #[test]
    fn non_numbered_voice_has_no_speaker() {
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<v Alice>hi\n";
        let segments = transcript_from_vtt(vtt.into()).unwrap();
        assert_eq!(
            (segments[0].text.as_str(), segments[0].speaker),
            ("hi", None)
        );
    }

    #[test]
    fn empty_transcript() {
        assert!(transcript_from_vtt(transcript_to_vtt(vec![]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rejects_malformed_files() {
        for (vtt, expected) in [
            ("", "line 1: missing WEBVTT header"),
            ("WEBVTTX\n", "line 1: missing WEBVTT header"),
            (
                "WEBVTT\n\n00:00:01.000 --> soon\nhi\n",
                "line 3: bad cue timing",
            ),
            (
                "WEBVTT\n\n00:00:61.000 --> 00:01:02.000\nhi\n",
                "line 3: bad cue timing",
            ),
            (
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<b>hi\n",
                "line 4: tag isn't a word timestamp",
            ),
            (
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nso <00:00:01.500>hi\n",
                "line 4: text before the first word timestamp",
            ),
        ] {
            let err = transcript_from_vtt(vtt.into()).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("can't read transcript: {expected}"),
                "{vtt:?}"
            );
        }
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(3_725_100), "01:02:05.100");
        assert_eq!(timestamp(-5), "00:00:00.000");
        assert_eq!(parse_timestamp("100:00:00.001"), Some(360_000_001));
        assert_eq!(parse_timestamp("00:00.1"), None);
        assert_eq!(parse_timestamp("-1:00:00.000"), None);
    }
}
