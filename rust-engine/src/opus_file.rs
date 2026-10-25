//! The app's recordings: Opus in Ogg (RFC 7845), 16 kHz mono. Encoded as the
//! audio is captured, decoded back to 16 kHz for playback and the final pass.

use std::collections::VecDeque;
use std::fmt::Display;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use opus::{Application, Bitrate, Channels, Decoder, Encoder};

use crate::engine::WhisperEngineError;
use crate::lock;
use crate::ogg::{self, PageWriter, FLAG_BEGIN, FLAG_CONTINUED, FLAG_END, NO_GRANULE};
use crate::session::SAMPLE_RATE_HZ;

/// Speech at 24 kbit/s: ~11 MB an hour, a tenth of 16-bit PCM.
const BITRATE_BPS: i32 = 24_000;
/// 20 ms frames.
const FRAME_SAMPLES: usize = SAMPLE_RATE_HZ / 50;
/// libopus's recommended output buffer for one packet.
const MAX_PACKET_BYTES: usize = 4000;
/// Longest Opus packet: 120 ms.
const MAX_PACKET_SAMPLES: usize = SAMPLE_RATE_HZ * 120 / 1000;
/// A page is written once it holds this many packets (1 s) and the next one
/// arrives, so a crash loses at most ~1 s of audio.
const PACKETS_PER_PAGE: usize = 50;
/// Opus granule positions count 48 kHz samples whatever the input rate.
const GRANULES_PER_SAMPLE: u64 = (48_000 / SAMPLE_RATE_HZ) as u64;
/// Decoding converges within 80 ms of where it starts (RFC 7845 §4.6), so
/// a seek starts that far before its target.
const SEEK_PREROLL_SAMPLES: u64 = (SAMPLE_RATE_HZ * 80 / 1000) as u64;
const OPUS_HEAD: &[u8; 8] = b"OpusHead";
const OPUS_HEAD_BYTES: usize = 19;
const OPUS_TAGS: &[u8; 8] = b"OpusTags";
const VENDOR: &str = "localrecord";
/// [`recording_duration_ms`] looks for the last page in this much of the end.
const TAIL_BYTES: u64 = 64 * 1024;

/// Encodes a recording to an Ogg Opus file as it is captured. Pages reach
/// the file about once a second; a file cut short by a crash still plays.
#[derive(uniffi::Object)]
pub struct RecordingWriter {
    /// None once finished.
    writer: Mutex<Option<Writer>>,
}

#[uniffi::export]
impl RecordingWriter {
    /// Creates (or truncates) the file and writes the stream headers.
    #[uniffi::constructor]
    pub fn new(path: String) -> Result<Arc<Self>, WhisperEngineError> {
        let writer = Writer::create(Path::new(&path))
            .map_err(|msg| WhisperEngineError::AudioWrite { msg })?;
        Ok(Arc::new(Self {
            writer: Mutex::new(Some(writer)),
        }))
    }

    /// Appends 16 kHz mono s16le PCM.
    pub fn write_pcm16(&self, pcm: Vec<u8>) -> Result<(), WhisperEngineError> {
        let mut guard = lock(&self.writer);
        let writer = guard.as_mut().ok_or_else(|| WhisperEngineError::AudioWrite {
            msg: "recording already finished".to_string(),
        })?;
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        writer
            .write(&samples)
            .map_err(|msg| WhisperEngineError::AudioWrite { msg })
    }

    /// Encodes the rest and ends the stream. Later calls do nothing.
    pub fn finish(&self) -> Result<(), WhisperEngineError> {
        match lock(&self.writer).take() {
            Some(writer) => writer
                .finish()
                .map_err(|msg| WhisperEngineError::AudioWrite { msg }),
            None => Ok(()),
        }
    }
}

impl Drop for RecordingWriter {
    fn drop(&mut self) {
        if let Some(writer) = lock(&self.writer).take() {
            if let Err(e) = writer.finish() {
                log::warn!("{e}");
            }
        }
    }
}

/// A recording opened for playback. Holds the file's index, not the file.
#[derive(uniffi::Object)]
pub struct RecordingFile {
    path: PathBuf,
    index: Index,
}

#[uniffi::export]
impl RecordingFile {
    /// Reads through the file once to index it. Blocking.
    #[uniffi::constructor]
    pub fn new(path: String) -> Result<Arc<Self>, WhisperEngineError> {
        let path = PathBuf::from(path);
        let index = Index::build(&path).map_err(|msg| WhisperEngineError::AudioFile { msg })?;
        Ok(Arc::new(Self { path, index }))
    }

    /// Length in 16 kHz samples.
    pub fn frames(&self) -> i64 {
        i64::try_from(self.index.frames()).unwrap_or(i64::MAX)
    }

    /// Decodes from `frame` (a 16 kHz sample) on. Each stream has its own
    /// file handle and decoder. Blocking.
    pub fn stream_from(&self, frame: i64) -> Result<Arc<RecordingStream>, WhisperEngineError> {
        let frame = u64::try_from(frame).unwrap_or(0);
        let decoding = Decoding::new(&self.path, &self.index, frame)
            .map_err(|msg| WhisperEngineError::AudioFile { msg })?;
        Ok(Arc::new(RecordingStream {
            decoding: Mutex::new(decoding),
        }))
    }
}

#[derive(uniffi::Object)]
pub struct RecordingStream {
    decoding: Mutex<Decoding>,
}

#[uniffi::export]
impl RecordingStream {
    /// The next `max_frames` samples as 16 kHz mono s16le PCM; fewer at the
    /// end, none after it. Blocking.
    pub fn read_pcm16(&self, max_frames: u32) -> Result<Vec<u8>, WhisperEngineError> {
        let max = max_frames as usize;
        let mut samples = Vec::with_capacity(max);
        lock(&self.decoding)
            .read(&mut samples, max)
            .map_err(|msg| WhisperEngineError::AudioFile { msg })?;
        Ok(samples
            .iter()
            .flat_map(|&s| ((s * 32768.0).clamp(-32768.0, 32767.0) as i16).to_le_bytes())
            .collect())
    }
}

/// A recording's length in ms, from its first and last pages: cheap enough
/// to call for every recording in a list. Blocking.
#[uniffi::export]
pub fn recording_duration_ms(path: String) -> Result<i64, WhisperEngineError> {
    let samples =
        duration_samples(Path::new(&path)).map_err(|msg| WhisperEngineError::AudioFile { msg })?;
    Ok(i64::try_from(samples * 1000 / SAMPLE_RATE_HZ as u64).unwrap_or(i64::MAX))
}

/// All of a recording as 16 kHz mono f32 samples in [-1, 1].
pub(crate) fn read_samples(path: &Path) -> Result<Vec<f32>, String> {
    let index = Index::build(path)?;
    let frames = usize::try_from(index.frames()).map_err(|e| failure(path, e))?;
    let mut samples = Vec::with_capacity(frames);
    Decoding::new(path, &index, 0)?.read(&mut samples, frames)?;
    Ok(samples)
}

/// The file's name for messages: they're shown in the UI.
fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn failure(path: &Path, e: impl Display) -> String {
    format!("{}: {e}", file_name(path))
}

/// The ID header (RFC 7845 §5.1), as far as decoding needs it.
struct Head {
    /// 48 kHz samples to drop from the start of the decoded stream.
    pre_skip: u64,
    /// Output gain in Q7.8 dB.
    gain: i16,
}

impl Head {
    fn packet(pre_skip: u16) -> Vec<u8> {
        let mut p = Vec::with_capacity(OPUS_HEAD_BYTES);
        p.extend_from_slice(OPUS_HEAD);
        p.push(1); // version
        p.push(1); // channels
        p.extend_from_slice(&pre_skip.to_le_bytes());
        p.extend_from_slice(&(SAMPLE_RATE_HZ as u32).to_le_bytes());
        p.extend_from_slice(&0i16.to_le_bytes()); // output gain
        p.push(0); // channel mapping family: mono/stereo
        p
    }

    fn parse(p: &[u8]) -> Result<Self, &'static str> {
        if p.len() < OPUS_HEAD_BYTES || &p[..8] != OPUS_HEAD {
            return Err("not an Ogg Opus file");
        }
        // Versions up to 15 are compatible with 1.
        if p[8] > 15 {
            return Err("unsupported Ogg Opus version");
        }
        // Family 0 is one or two channels, the only layout a plain decoder takes.
        if p[18] != 0 || !(1..=2).contains(&p[9]) {
            return Err("unsupported Opus channel layout");
        }
        Ok(Self {
            pre_skip: u64::from(u16::from_le_bytes([p[10], p[11]])),
            gain: i16::from_le_bytes([p[16], p[17]]),
        })
    }
}

fn tags_packet() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(OPUS_TAGS);
    p.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    p.extend_from_slice(VENDOR.as_bytes());
    p.extend_from_slice(&0u32.to_le_bytes()); // no comments
    p
}

/// Identifies the file's one logical stream; only has to differ from
/// other streams in the same file, so the clock will do.
fn stream_serial() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(1)
}

struct Writer {
    path: PathBuf,
    pages: PageWriter<File>,
    encoder: Encoder,
    /// Encoder delay in samples; the header's pre-skip drops it on decoding.
    lookahead: u64,
    /// Captured samples not yet encoded (less than a frame).
    frame: Vec<i16>,
    captured: u64,
    /// Samples encoded, including the zeros that end the stream.
    encoded: u64,
    packet: Vec<u8>,
}

impl Writer {
    fn create(path: &Path) -> Result<Self, String> {
        let fail = |e: &dyn Display| failure(path, e);
        let mut encoder = Encoder::new(SAMPLE_RATE_HZ as u32, Channels::Mono, Application::Audio)
            .map_err(|e| fail(&e))?;
        encoder
            .set_bitrate(Bitrate::Bits(BITRATE_BPS))
            .map_err(|e| fail(&e))?;
        let lookahead = encoder.get_lookahead().map_err(|e| fail(&e))?;
        let pre_skip = u16::try_from(i64::from(lookahead) * GRANULES_PER_SAMPLE as i64)
            .map_err(|_| fail(&"encoder delay out of range"))?;
        let file = File::create(path).map_err(|e| fail(&e))?;
        let mut pages = PageWriter::new(file, stream_serial());
        // Each header packet ends its page (RFC 7845 §3).
        let headers = (|| -> io::Result<()> {
            pages.add(&Head::packet(pre_skip), 0)?;
            pages.write_page(FLAG_BEGIN)?;
            pages.add(&tags_packet(), 0)?;
            pages.write_page(0)
        })();
        headers.map_err(|e| fail(&e))?;
        Ok(Self {
            path: path.to_path_buf(),
            pages,
            encoder,
            lookahead: u64::from(pre_skip) / GRANULES_PER_SAMPLE,
            frame: Vec::with_capacity(FRAME_SAMPLES),
            captured: 0,
            encoded: 0,
            packet: vec![0; MAX_PACKET_BYTES],
        })
    }

    fn write(&mut self, mut samples: &[i16]) -> Result<(), String> {
        self.captured += samples.len() as u64;
        while !samples.is_empty() {
            let take = (FRAME_SAMPLES - self.frame.len()).min(samples.len());
            self.frame.extend_from_slice(&samples[..take]);
            samples = &samples[take..];
            if self.frame.len() == FRAME_SAMPLES {
                self.encode_frame()?;
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), String> {
        // Zeros push the last captured samples through the encoder's delay;
        // the final granule position trims them off again.
        while self.encoded < self.captured + self.lookahead {
            self.frame.resize(FRAME_SAMPLES, 0);
            self.encode_frame()?;
        }
        self.pages
            .write_page(FLAG_END)
            .and_then(|()| self.pages.into_inner().sync_all())
            .map_err(|e| failure(&self.path, e))
    }

    fn encode_frame(&mut self) -> Result<(), String> {
        let len = self
            .encoder
            .encode(&self.frame, &mut self.packet)
            .map_err(|e| failure(&self.path, e))?;
        self.frame.clear();
        self.encoded += FRAME_SAMPLES as u64;
        // A packet's granule position is where it ends in the decoded stream,
        // which runs `lookahead` behind the captured audio.
        let end = self.encoded.min(self.captured + self.lookahead);
        // The page being collected is only written when another packet comes,
        // so the stream's last page always has one to carry its end flag.
        let written = if self.pages.pending_packets() >= PACKETS_PER_PAGE {
            self.pages.write_page(0)
        } else {
            Ok(())
        };
        written
            .and_then(|()| {
                self.pages
                    .add(&self.packet[..len], (end * GRANULES_PER_SAMPLE) as i64)
            })
            .map_err(|e| failure(&self.path, e))
    }
}

/// What decoding needs to know about a file, from one pass over it. Sample
/// counts are 16 kHz samples of the decoded stream, pre-skip included.
struct Index {
    serial: u32,
    head: Head,
    /// Where decoding can start: (a page's byte offset, samples decoded
    /// before its first packet). Ascending; the first is the first audio page.
    seek_points: Vec<(u64, u64)>,
    /// Output is the decoded stream's samples in [start, end).
    start: u64,
    end: u64,
}

impl Index {
    fn build(path: &Path) -> Result<Self, String> {
        let fail = |e: &dyn Display| failure(path, e);
        let mut pages = BufReader::new(File::open(path).map_err(|e| fail(&e))?);
        let (serial, head) = read_head(&mut pages).map_err(|e| fail(&e))?;
        let mut offset = pages.stream_position().map_err(|e| fail(&e))?;

        let mut seek_points = Vec::new();
        let mut decoded = 0u64;
        let mut packet = Vec::new();
        // The first granule position minus the samples before it: where the
        // stream starts, normally 0 (RFC 7845 §4.5).
        let mut start_granule: Option<i64> = None;
        let mut last_granule = None;
        loop {
            let page = match ogg::read_page(&mut pages) {
                Ok(Some(page)) => page,
                Ok(None) => break,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    log::warn!("{}: {e} at byte {offset}; reading up to there", file_name(path));
                    break;
                }
                Err(e) => return Err(fail(&e)),
            };
            let at = offset;
            offset += page.len;
            if page.serial != serial {
                continue;
            }
            if page.flags & FLAG_CONTINUED == 0 {
                packet.clear();
                seek_points.push((at, decoded));
            }
            for (bytes, ends) in page.pieces() {
                packet.extend_from_slice(bytes);
                if ends {
                    let samples = opus::packet::get_nb_samples(&packet, SAMPLE_RATE_HZ as u32)
                        .map_err(|e| fail(&format_args!("corrupt audio at byte {at}: {e}")))?;
                    decoded += samples as u64;
                    packet.clear();
                }
            }
            if page.granule != NO_GRANULE {
                let before = (decoded * GRANULES_PER_SAMPLE) as i64;
                start_granule.get_or_insert((page.granule - before).max(0));
                last_granule = Some(page.granule);
            }
            if page.flags & FLAG_END != 0 {
                break;
            }
        }
        // End trimming: the last granule position marks the end of the audio.
        let end = match (last_granule, start_granule) {
            (Some(last), Some(first)) => {
                u64::try_from(last - first).unwrap_or(0) / GRANULES_PER_SAMPLE
            }
            _ => decoded,
        };
        Ok(Self {
            serial,
            start: head.pre_skip / GRANULES_PER_SAMPLE,
            end: end.min(decoded),
            head,
            seek_points,
        })
    }

    fn frames(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

/// Reads the ID and comment headers; leaves `pages` at the first audio page.
fn read_head(pages: &mut impl Read) -> Result<(u32, Head), String> {
    let not_opus = || "not an Ogg Opus file".to_string();
    let first = ogg::read_page(pages)
        .map_err(|_| not_opus())?
        .ok_or_else(not_opus)?;
    let pieces = first.pieces();
    let head = match pieces.first() {
        Some((bytes, true)) if first.flags & FLAG_BEGIN != 0 => Head::parse(bytes)?,
        _ => return Err(not_opus()),
    };
    // The comment header can span pages; audio starts on the page after it ends.
    loop {
        let page = ogg::read_page(pages)
            .map_err(|e| e.to_string())?
            .ok_or("incomplete Ogg Opus headers")?;
        if page.serial == first.serial && page.pieces().iter().any(|&(_, ends)| ends) {
            return Ok((first.serial, head));
        }
    }
}

/// The ID header's serial and pre-skip, and the last granule position in
/// the file's tail, without reading the rest; indexes the whole file if no
/// whole page with a granule position is in the tail.
fn duration_samples(path: &Path) -> Result<u64, String> {
    let fail = |e: &dyn Display| failure(path, e);
    let mut file = File::open(path).map_err(|e| fail(&e))?;
    let (serial, head) = read_head(&mut BufReader::new(&file)).map_err(|e| fail(&e))?;
    let len = file.metadata().map_err(|e| fail(&e))?.len();
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .and_then(|_| file.read_to_end(&mut tail))
        .map_err(|e| fail(&e))?;
    for at in (0..tail.len().saturating_sub(3)).rev() {
        if &tail[at..at + 4] != b"OggS" {
            continue;
        }
        if let Ok(Some(page)) = ogg::read_page(&mut &tail[at..]) {
            if page.serial == serial {
                if let Ok(granule) = u64::try_from(page.granule) {
                    let start = head.pre_skip / GRANULES_PER_SAMPLE;
                    return Ok((granule / GRANULES_PER_SAMPLE).saturating_sub(start));
                }
            }
        }
    }
    Ok(Index::build(path)?.frames())
}

/// Decodes a file from a seek point on, keeping the output in range.
struct Decoding {
    path: PathBuf,
    pages: BufReader<File>,
    serial: u32,
    decoder: Decoder,
    /// Bytes of a packet that goes on in the next page.
    packet: Vec<u8>,
    /// Whole packets of the last page read, not decoded yet.
    packets: VecDeque<Vec<u8>>,
    /// Samples decoded so far, counted from the start of the stream.
    position: u64,
    /// Output is the decoded samples in [from, end).
    from: u64,
    end: u64,
    /// Decoded output not returned yet.
    ready: Vec<f32>,
    buf: Vec<f32>,
    /// No pages left.
    pages_done: bool,
}

impl Decoding {
    fn new(path: &Path, index: &Index, frame: u64) -> Result<Self, String> {
        let fail = |e: &dyn Display| failure(path, e);
        let from = index.start.saturating_add(frame).min(index.end);
        let target = from.saturating_sub(SEEK_PREROLL_SAMPLES);
        let seek_point = index
            .seek_points
            .iter()
            .rev()
            .find(|&&(_, decoded)| decoded <= target)
            .copied();
        let mut pages = BufReader::new(File::open(path).map_err(|e| fail(&e))?);
        let (offset, position) = seek_point.unwrap_or((0, index.end));
        pages
            .seek(SeekFrom::Start(offset))
            .map_err(|e| fail(&e))?;
        let mut decoder =
            Decoder::new(SAMPLE_RATE_HZ as u32, Channels::Mono).map_err(|e| fail(&e))?;
        decoder
            .set_gain(i32::from(index.head.gain))
            .map_err(|e| fail(&e))?;
        Ok(Self {
            path: path.to_path_buf(),
            pages,
            serial: index.serial,
            decoder,
            packet: Vec::new(),
            packets: Default::default(),
            position,
            from,
            end: index.end,
            ready: Vec::new(),
            buf: vec![0.0; MAX_PACKET_SAMPLES],
            pages_done: seek_point.is_none(),
        })
    }

    /// Appends up to `max` samples of output to `out`; fewer at the end.
    fn read(&mut self, out: &mut Vec<f32>, max: usize) -> Result<(), String> {
        let mut wanted = max;
        loop {
            let take = wanted.min(self.ready.len());
            out.extend(self.ready.drain(..take));
            wanted -= take;
            if wanted == 0 || self.position >= self.end {
                return Ok(());
            }
            let Some(packet) = self.next_packet()? else {
                return Ok(());
            };
            let n = self
                .decoder
                .decode_float(&packet, &mut self.buf, false)
                .map_err(|e| failure(&self.path, e))?;
            let start = self.position;
            self.position += n as u64;
            let keep_from = self.from.max(start);
            let keep_to = self.end.min(self.position);
            if keep_from < keep_to {
                self.ready.extend_from_slice(
                    &self.buf[(keep_from - start) as usize..(keep_to - start) as usize],
                );
            }
        }
    }

    fn next_packet(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            if let Some(packet) = self.packets.pop_front() {
                return Ok(Some(packet));
            }
            if self.pages_done {
                return Ok(None);
            }
            let page = match ogg::read_page(&mut self.pages) {
                Ok(Some(page)) => page,
                // The index stopped at the same place.
                Ok(None) => {
                    self.pages_done = true;
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    self.pages_done = true;
                    continue;
                }
                Err(e) => return Err(failure(&self.path, e)),
            };
            if page.serial != self.serial {
                continue;
            }
            if page.flags & FLAG_CONTINUED == 0 {
                self.packet.clear();
            }
            for (bytes, ends) in page.pieces() {
                self.packet.extend_from_slice(bytes);
                if ends {
                    self.packets.push_back(std::mem::take(&mut self.packet));
                }
            }
            if page.flags & FLAG_END != 0 {
                self.pages_done = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test file in the system temp dir, removed when dropped.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            Self(std::env::temp_dir().join(format!("{}-{name}.ogg", std::process::id())))
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A sweep from 200 Hz up to 2 kHz, rising in loudness: no two stretches
    /// alike, so a misaligned decode can't match it.
    fn tone(samples: usize) -> Vec<i16> {
        let seconds = samples as f64 / SAMPLE_RATE_HZ as f64;
        (0..samples)
            .map(|i| {
                let t = i as f64 / SAMPLE_RATE_HZ as f64;
                let phase = 200.0 * t + 1800.0 * t * t / (2.0 * seconds);
                let level = 2000.0 + 6000.0 * t / seconds;
                (level * (2.0 * std::f64::consts::PI * phase).sin()) as i16
            })
            .collect()
    }

    fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn write(file: &TempFile, samples: &[i16], chunk: usize) {
        let writer = RecordingWriter::new(file.path()).unwrap();
        for part in samples.chunks(chunk) {
            writer.write_pcm16(pcm_bytes(part)).unwrap();
        }
        writer.finish().unwrap();
    }

    fn rms_error(a: &[f32], b: &[f32]) -> f32 {
        let sum: f32 = a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum();
        (sum / a.len() as f32).sqrt()
    }

    /// The shift of `b` against `a`, within `max`, that matches them best.
    fn best_lag(a: &[f32], b: &[f32], max: usize) -> i64 {
        let n = a.len() - 2 * max;
        (-(max as i64)..=max as i64)
            .map(|lag| {
                let dot: f32 = (0..n)
                    .map(|i| a[max + i] * b[(max as i64 + i as i64 + lag) as usize])
                    .sum();
                (lag, dot)
            })
            .max_by(|x, y| x.1.total_cmp(&y.1))
            .map_or(0, |(lag, _)| lag)
    }

    #[test]
    fn decodes_to_the_captured_length_and_signal() {
        let file = TempFile::new("round-trip");
        // 3.33 s: not a whole number of frames or pages.
        let input = tone(53_333);
        write(&file, &input, 1_600);

        let recording = RecordingFile::new(file.path()).unwrap();
        assert_eq!(recording.frames(), input.len() as i64);
        assert_eq!(recording_duration_ms(file.path()).unwrap(), 3_333);
        let output = read_samples(&file.0).unwrap();
        assert_eq!(output.len(), input.len());
        let expected: Vec<f32> = input.iter().map(|&s| f32::from(s) / 32768.0).collect();
        // Lossy, but aligned: the pre-skip removed the encoder's delay.
        assert_eq!(best_lag(&expected, &output, 200), 0);
        assert!(rms_error(&output[1_000..], &expected[1_000..]) < 0.03);
        // ~24 kbit/s
        let bytes = std::fs::metadata(&file.0).unwrap().len();
        assert!(bytes < 3 * 3_000 + 3_000, "{bytes} bytes");
    }

    #[test]
    fn a_seek_decodes_what_reading_from_the_start_does() {
        let file = TempFile::new("seek");
        let input = tone(SAMPLE_RATE_HZ * 5);
        write(&file, &input, 1_600);
        let recording = RecordingFile::new(file.path()).unwrap();
        let whole = read_samples(&file.0).unwrap();

        for from in [0usize, 7, 16_000, 33_333, 75_000] {
            let stream = recording.stream_from(from as i64).unwrap();
            let pcm = stream.read_pcm16(4_000).unwrap();
            let got: Vec<f32> = pcm
                .chunks_exact(2)
                .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
                .collect();
            let want = &whole[from..(from + 4_000).min(whole.len())];
            assert_eq!(got.len(), want.len(), "from {from}");
            // A decoder started mid-stream differs at the level of the coding
            // noise at first, then converges.
            assert!(rms_error(&got[..1_600], &want[..1_600]) < 0.01, "from {from}");
            assert!(rms_error(&got[2_400..], &want[2_400..]) < 0.000_5, "from {from}");
        }
        let past_the_end = recording.stream_from(1_000_000).unwrap();
        assert!(past_the_end.read_pcm16(100).unwrap().is_empty());
    }

    #[test]
    fn reads_in_chunks_to_the_end() {
        let file = TempFile::new("chunks");
        let input = tone(20_000);
        write(&file, &input, 333);
        let stream = RecordingFile::new(file.path())
            .unwrap()
            .stream_from(0)
            .unwrap();
        let mut total = 0;
        loop {
            let pcm = stream.read_pcm16(1_234).unwrap();
            if pcm.is_empty() {
                break;
            }
            total += pcm.len() / 2;
        }
        assert_eq!(total, input.len());
    }

    #[test]
    fn a_file_cut_short_plays_up_to_its_last_whole_page() {
        let file = TempFile::new("cut");
        let input = tone(SAMPLE_RATE_HZ * 4);
        write(&file, &input, 1_600);
        let bytes = std::fs::read(&file.0).unwrap();
        std::fs::write(&file.0, &bytes[..bytes.len() * 2 / 3]).unwrap();

        let frames = RecordingFile::new(file.path()).unwrap().frames();
        // Pages hold 1 s; the cut one and those after it are lost.
        assert!((SAMPLE_RATE_HZ as i64 - 400..SAMPLE_RATE_HZ as i64 * 3).contains(&frames));
        assert_eq!(read_samples(&file.0).unwrap().len() as i64, frames);
        let ms = recording_duration_ms(file.path()).unwrap();
        assert!((ms - frames * 1000 / SAMPLE_RATE_HZ as i64).abs() <= 30, "{ms} ms");
    }

    #[test]
    fn an_empty_recording_has_no_audio() {
        let file = TempFile::new("empty");
        write(&file, &[], 1);
        assert_eq!(RecordingFile::new(file.path()).unwrap().frames(), 0);
        assert!(read_samples(&file.0).unwrap().is_empty());
        assert_eq!(recording_duration_ms(file.path()).unwrap(), 0);
    }

    #[test]
    fn dropping_an_unfinished_writer_ends_the_stream() {
        let file = TempFile::new("dropped");
        let writer = RecordingWriter::new(file.path()).unwrap();
        writer.write_pcm16(pcm_bytes(&tone(8_000))).unwrap();
        drop(writer);
        assert_eq!(RecordingFile::new(file.path()).unwrap().frames(), 8_000);
    }

    #[test]
    fn rejects_other_files() {
        let file = TempFile::new("not-ogg");
        std::fs::write(&file.0, b"RIFF\0\0\0\0WAVEfmt definitely not ogg").unwrap();
        let err = RecordingFile::new(file.path()).err().unwrap().to_string();
        assert!(err.contains("not an Ogg Opus file"), "{err}");
        assert!(read_samples(&file.0).is_err());
        assert!(recording_duration_ms(file.path()).is_err());
    }
}

