//! Minimal reader for the app's recordings: RIFF/WAVE, PCM, 16-bit, mono,
//! 16 kHz (what RecordingService writes). Anything else is rejected.

use std::path::Path;

use crate::session::SAMPLE_RATE_HZ;

const PCM_FORMAT: u16 = 1;

/// Samples of a 16 kHz mono 16-bit WAV as f32 in [-1, 1).
pub(crate) fn read_pcm16_mono_16k(path: &Path) -> Result<Vec<f32>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn parse(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".to_string());
    }
    let mut format_ok = false;
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let declared = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        let body = pos + 8;
        let remaining = bytes.len() - body;
        match id {
            b"fmt " => {
                let fmt = bytes.get(body..body + 16).ok_or("truncated fmt chunk")?;
                let u16_at = |i: usize| u16::from_le_bytes([fmt[i], fmt[i + 1]]);
                let rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                if u16_at(0) != PCM_FORMAT || u16_at(2) != 1 || rate as usize != SAMPLE_RATE_HZ || u16_at(14) != 16 {
                    return Err(format!(
                        "need PCM mono 16 kHz 16-bit, got format {} channels {} rate {rate} bits {}",
                        u16_at(0),
                        u16_at(2),
                        u16_at(14)
                    ));
                }
                format_ok = true;
            }
            b"data" => {
                if !format_ok {
                    return Err("data chunk before fmt chunk".to_string());
                }
                // A recording cut short (crash) can have a size of 0 or past
                // the end of the file; take what is there.
                let len = if declared == 0 || declared > remaining { remaining } else { declared };
                return Ok(bytes[body..body + len]
                    .chunks_exact(2)
                    .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
                    .collect());
            }
            _ => {}
        }
        // chunks are padded to an even length
        pos = body.saturating_add(declared + (declared & 1));
    }
    Err("no data chunk".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(data_size_field: u32, samples: &[i16]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&16_000u32.to_le_bytes());
        b.extend_from_slice(&32_000u32.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_size_field.to_le_bytes());
        samples.iter().for_each(|s| b.extend_from_slice(&s.to_le_bytes()));
        b
    }

    #[test]
    fn reads_samples() {
        let out = parse(&wav(4, &[16_384, -32_768])).unwrap();
        assert_eq!(out, [0.5, -1.0]);
    }

    #[test]
    fn size_past_the_end_reads_what_is_there() {
        assert_eq!(parse(&wav(1_000, &[0, 0, 0])).unwrap().len(), 3);
        assert_eq!(parse(&wav(0, &[0, 0])).unwrap().len(), 2);
    }

    #[test]
    fn rejects_other_formats() {
        let mut b = wav(2, &[0]);
        b[24..28].copy_from_slice(&44_100u32.to_le_bytes());
        assert!(parse(&b).is_err());
        assert!(parse(b"nope").is_err());
    }
}
