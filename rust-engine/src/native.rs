//! Minimal safe layer over sherpa-onnx's offline recognizer (its C API, via
//! sherpa-onnx-sys) that survives C++ exceptions. ONNX Runtime throws (e.g.
//! `Ort::Exception` on an input it can't take) and the C API lets it through;
//! an exception reaching Rust aborts the whole process (recording included).
//! Every call that can throw runs inside the C++ try/catch in src/catch.cpp
//! and comes back as `NativeError::Exception`.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fmt;
use std::path::Path;
use std::ptr::NonNull;

use serde::Deserialize;
use sherpa_onnx_sys as sys;

/// Buffer for a caught exception's message, NUL included.
const EXCEPTION_MSG_CAP: usize = 512;
/// A model directory's files, named as in sherpa-onnx's int8 Parakeet exports.
const ENCODER_FILE: &str = "encoder.int8.onnx";
const DECODER_FILE: &str = "decoder.int8.onnx";
const JOINER_FILE: &str = "joiner.int8.onnx";
const TOKENS_FILE: &str = "tokens.txt";
/// NeMo transducers, Parakeet TDT among them.
const MODEL_TYPE: &CStr = c"nemo_transducer";
const SAMPLE_RATE_HZ: c_int = 16_000;
/// sherpa-onnx's default; NeMo models use the feature size in the model file
/// (128 mel bins for Parakeet).
const FEATURE_DIM: c_int = 80;

extern "C" {
    /// src/catch.cpp: calls `f(data)`; on a C++ exception writes its message
    /// to `err` and returns nonzero.
    fn lr_catch_exceptions(
        f: unsafe extern "C-unwind" fn(*mut c_void),
        data: *mut c_void,
        err: *mut c_char,
        err_len: usize,
    ) -> c_int;
}

// The sherpa-onnx entry points used here, redeclared "C-unwind" (sherpa-onnx-sys
// declares everything "C") so an exception may legally unwind through the Rust
// frames between them and lr_catch_exceptions.
extern "C-unwind" {
    fn SherpaOnnxCreateOfflineRecognizer(
        config: *const sys::OfflineRecognizerConfig,
    ) -> *const sys::OfflineRecognizer;
    fn SherpaOnnxDestroyOfflineRecognizer(recognizer: *const sys::OfflineRecognizer);
    fn SherpaOnnxCreateOfflineStream(
        recognizer: *const sys::OfflineRecognizer,
    ) -> *const sys::OfflineStream;
    fn SherpaOnnxDestroyOfflineStream(stream: *const sys::OfflineStream);
    fn SherpaOnnxAcceptWaveformOffline(
        stream: *const sys::OfflineStream,
        sample_rate: c_int,
        samples: *const f32,
        n: c_int,
    );
    fn SherpaOnnxDecodeOfflineStream(
        recognizer: *const sys::OfflineRecognizer,
        stream: *const sys::OfflineStream,
    );
    fn SherpaOnnxGetOfflineStreamResultAsJson(stream: *const sys::OfflineStream) -> *const c_char;
    fn SherpaOnnxDestroyOfflineStreamResultJson(json: *const c_char);
}

#[derive(Debug)]
pub(crate) enum NativeError {
    /// A C++ exception escaped sherpa-onnx / ONNX Runtime.
    Exception(String),
    /// sherpa-onnx reported failure, or its output was unusable.
    Failed(String),
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exception(msg) => write!(f, "native exception: {msg}"),
            Self::Failed(msg) => f.write_str(msg),
        }
    }
}

/// Run `f` with C++ exceptions caught. `f` must not panic: C++ would catch the
/// panic, and Rust aborts when a foreign handler swallows one.
fn catch_exceptions<F: FnOnce() -> R, R>(f: F) -> Result<R, NativeError> {
    struct Call<F, R> {
        f: Option<F>,
        out: Option<R>,
    }

    unsafe extern "C-unwind" fn trampoline<F: FnOnce() -> R, R>(data: *mut c_void) {
        // SAFETY: `data` is the `Call` below, exclusively borrowed for this call.
        let call = unsafe { &mut *data.cast::<Call<F, R>>() };
        if let Some(f) = call.f.take() {
            call.out = Some(f());
        }
    }

    let mut call = Call {
        f: Some(f),
        out: None,
    };
    let mut err = [0 as c_char; EXCEPTION_MSG_CAP];
    // SAFETY: `trampoline` matches the callback type and `call` and `err`
    // outlive the call.
    let threw = unsafe {
        lr_catch_exceptions(
            trampoline::<F, R>,
            std::ptr::addr_of_mut!(call).cast(),
            err.as_mut_ptr(),
            err.len(),
        )
    } != 0;
    if threw {
        // SAFETY: lr_catch_exceptions NUL-terminates `err`.
        let msg = unsafe { CStr::from_ptr(err.as_ptr()) };
        return Err(NativeError::Exception(msg.to_string_lossy().into_owned()));
    }
    call.out
        .ok_or_else(|| NativeError::Failed("native callback did not run".to_string()))
}

/// One recognized token: a SentencePiece piece, where a leading space starts
/// a word. Times are seconds from the start of the decoded audio.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Token {
    pub text: String,
    /// The encoder frame (80 ms) the token was emitted on.
    pub start_s: f32,
    /// Frames TDT skipped after the token: 0 when the next token shares its frame.
    pub duration_s: f32,
}

/// The parts of sherpa-onnx's JSON result used here.
#[derive(Deserialize)]
struct RawResult {
    tokens: Vec<String>,
    timestamps: Option<Vec<f32>>,
    durations: Option<Vec<f32>>,
}

/// A loaded model. Shared across threads: each decode has its own stream.
pub(crate) struct Recognizer(NonNull<sys::OfflineRecognizer>);

// SAFETY: sherpa-onnx only reads the recognizer after loading, and ONNX
// Runtime sessions allow concurrent runs.
unsafe impl Send for Recognizer {}
unsafe impl Sync for Recognizer {}

impl Recognizer {
    /// Load a Parakeet TDT model from the files in `dir`. ONNX Runtime runs a
    /// decode on `num_threads` threads; with 1, on the calling thread only.
    pub(crate) fn load(dir: &Path, num_threads: c_int) -> Result<Self, NativeError> {
        let path = |file: &str| {
            let path = dir.join(file);
            // sherpa-onnx only logs which file is missing; name it in the error.
            if !path.is_file() {
                return Err(NativeError::Failed(format!("missing {}", path.display())));
            }
            CString::new(path.to_string_lossy().into_owned())
                .map_err(|_| NativeError::Failed("model path contains a NUL byte".to_string()))
        };
        let encoder = path(ENCODER_FILE)?;
        let decoder = path(DECODER_FILE)?;
        let joiner = path(JOINER_FILE)?;
        let tokens = path(TOKENS_FILE)?;
        // SAFETY: a plain C struct of pointers and numbers, for which all-zero
        // (null / 0) means "unset, use the default" to the C API.
        let mut config: sys::OfflineRecognizerConfig = unsafe { std::mem::zeroed() };
        config.feat_config.sample_rate = SAMPLE_RATE_HZ;
        config.feat_config.feature_dim = FEATURE_DIM;
        config.model_config.transducer.encoder = encoder.as_ptr();
        config.model_config.transducer.decoder = decoder.as_ptr();
        config.model_config.transducer.joiner = joiner.as_ptr();
        config.model_config.tokens = tokens.as_ptr();
        config.model_config.model_type = MODEL_TYPE.as_ptr();
        config.model_config.num_threads = num_threads;
        // SAFETY: `config` and the strings it points to outlive the call; the
        // C API copies what it keeps.
        let ptr = catch_exceptions(|| unsafe { SherpaOnnxCreateOfflineRecognizer(&config) })?;
        NonNull::new(ptr.cast_mut())
            .map(Self)
            .ok_or_else(|| NativeError::Failed("sherpa-onnx could not load the model".to_string()))
    }

    /// Recognize 16 kHz mono `samples` in one decode. Must not be empty
    /// (ONNX Runtime throws).
    pub(crate) fn recognize(&self, samples: &[f32]) -> Result<Vec<Token>, NativeError> {
        let n = c_int::try_from(samples.len())
            .map_err(|_| NativeError::Failed("audio too long for one decode".to_string()))?;
        let recognizer = self.0.as_ptr().cast_const();
        let stream = Stream::new(recognizer)?;
        let stream_ptr = stream.0.as_ptr().cast_const();
        // SAFETY: the recognizer and stream are live, `samples` holds `n` floats.
        let json = catch_exceptions(|| unsafe {
            SherpaOnnxAcceptWaveformOffline(stream_ptr, SAMPLE_RATE_HZ, samples.as_ptr(), n);
            SherpaOnnxDecodeOfflineStream(recognizer, stream_ptr);
            let json = SherpaOnnxGetOfflineStreamResultAsJson(stream_ptr);
            if json.is_null() {
                return None;
            }
            let out = CStr::from_ptr(json).to_string_lossy().into_owned();
            SherpaOnnxDestroyOfflineStreamResultJson(json);
            Some(out)
        })?
        .ok_or_else(|| NativeError::Failed("no recognition result".to_string()))?;
        tokens_from_json(&json)
    }
}

impl Drop for Recognizer {
    fn drop(&mut self) {
        let ptr = self.0.as_ptr().cast_const();
        // SAFETY: owned pointer, freed once.
        if let Err(e) = catch_exceptions(|| unsafe { SherpaOnnxDestroyOfflineRecognizer(ptr) }) {
            log::error!("leaking recognizer: {e}");
        }
    }
}

/// One decode's input and result.
struct Stream(NonNull<sys::OfflineStream>);

impl Stream {
    fn new(recognizer: *const sys::OfflineRecognizer) -> Result<Self, NativeError> {
        // SAFETY: `recognizer` is live (borrowed from a `Recognizer`).
        let ptr = catch_exceptions(|| unsafe { SherpaOnnxCreateOfflineStream(recognizer) })?;
        NonNull::new(ptr.cast_mut())
            .map(Self)
            .ok_or_else(|| NativeError::Failed("could not create a recognizer stream".to_string()))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let ptr = self.0.as_ptr().cast_const();
        // SAFETY: owned pointer, freed once.
        if let Err(e) = catch_exceptions(|| unsafe { SherpaOnnxDestroyOfflineStream(ptr) }) {
            log::error!("leaking recognizer stream: {e}");
        }
    }
}

fn tokens_from_json(json: &str) -> Result<Vec<Token>, NativeError> {
    let raw: RawResult = serde_json::from_str(json)
        .map_err(|e| NativeError::Failed(format!("unreadable recognition result: {e}")))?;
    let (Some(starts), Some(durations)) = (raw.timestamps, raw.durations) else {
        return Err(NativeError::Failed(
            "recognition result has no token times".to_string(),
        ));
    };
    if starts.len() != raw.tokens.len() || durations.len() != raw.tokens.len() {
        return Err(NativeError::Failed(
            "recognition result's token times don't match its tokens".to_string(),
        ));
    }
    Ok(raw
        .tokens
        .into_iter()
        .zip(starts.into_iter().zip(durations))
        .map(|(text, (start_s, duration_s))| Token {
            text,
            start_s,
            duration_s,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C-unwind" {
        fn lr_throw_for_test(msg: *const c_char);
    }

    #[test]
    fn cpp_exception_becomes_error() {
        // SAFETY: valid C string; the throw is what's under test.
        let err = catch_exceptions(|| unsafe { lr_throw_for_test(c"bad input".as_ptr()) });
        assert!(matches!(err, Err(NativeError::Exception(msg)) if msg == "bad input"));
    }

    #[test]
    fn result_passes_through() {
        assert_eq!(catch_exceptions(|| 7).ok(), Some(7));
    }

    #[test]
    fn json_result_becomes_tokens() {
        let json = r#"{"lang": "", "text": " Hi.", "tokens": [" Hi", "."],
            "timestamps": [0.4, 0.64], "durations": [0.24, 0.08], "words": []}"#;
        let tokens = tokens_from_json(json).ok();
        assert_eq!(
            tokens,
            Some(vec![
                Token {
                    text: " Hi".to_string(),
                    start_s: 0.4,
                    duration_s: 0.24
                },
                Token {
                    text: ".".to_string(),
                    start_s: 0.64,
                    duration_s: 0.08
                },
            ])
        );
    }

    #[test]
    fn json_result_without_times_is_an_error() {
        let json = r#"{"text": "Hi", "tokens": [" Hi"]}"#;
        assert!(matches!(
            tokens_from_json(json),
            Err(NativeError::Failed(_))
        ));
    }
}
