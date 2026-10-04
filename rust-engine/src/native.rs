//! Minimal safe layer over whisper.cpp (via whisper-rs-sys) that survives C++
//! exceptions. whisper.cpp and ggml's Vulkan backend throw; an exception
//! reaching Rust aborts the whole process (recording included). Every call
//! that can throw runs inside the C++ try/catch in src/catch.cpp and comes
//! back as `NativeError::Exception`.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::Arc;

use whisper_rs_sys as sys;

/// Buffer for a caught exception's message, NUL included.
const EXCEPTION_MSG_CAP: usize = 512;

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

// The whisper.cpp entry points that can throw, redeclared "C-unwind" (whisper-rs-sys
// declares everything "C") so an exception may legally unwind through the Rust
// frames between them and lr_catch_exceptions.
extern "C-unwind" {
    fn whisper_init_from_file_with_params_no_state(
        path_model: *const c_char,
        params: sys::whisper_context_params,
    ) -> *mut sys::whisper_context;
    fn whisper_init_state(ctx: *mut sys::whisper_context) -> *mut sys::whisper_state;
    fn whisper_full_with_state(
        ctx: *mut sys::whisper_context,
        state: *mut sys::whisper_state,
        params: sys::whisper_full_params,
        samples: *const f32,
        n_samples: c_int,
    ) -> c_int;
    // Freeing GPU buffers waits on fences, which throws after a device loss.
    fn whisper_free_state(state: *mut sys::whisper_state);
    fn whisper_free(ctx: *mut sys::whisper_context);
}

#[derive(Debug)]
pub(crate) enum NativeError {
    /// A C++ exception escaped whisper.cpp/ggml. The GPU backend may be gone
    /// for the rest of the process (e.g. Vulkan device lost).
    Exception(String),
    /// whisper.cpp reported failure through its return value.
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

    let mut call = Call { f: Some(f), out: None };
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

/// A loaded model. whisper.cpp only reads it after loading, so it is shared
/// across threads; each concurrent inference needs its own `State`.
pub(crate) struct Context(NonNull<sys::whisper_context>);

// SAFETY: the context is immutable after load (see above).
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Context {
    pub(crate) fn load(path: &str, use_gpu: bool, flash_attn: bool) -> Result<Self, NativeError> {
        let c_path = CString::new(path)
            .map_err(|_| NativeError::Failed("model path contains a NUL byte".to_string()))?;
        // SAFETY: plain value constructor.
        let mut params = unsafe { sys::whisper_context_default_params() };
        params.use_gpu = use_gpu;
        params.flash_attn = flash_attn;
        // SAFETY: `c_path` is a valid C string for the duration of the call.
        let ptr = catch_exceptions(|| unsafe {
            whisper_init_from_file_with_params_no_state(c_path.as_ptr(), params)
        })?;
        NonNull::new(ptr)
            .map(Self)
            .ok_or_else(|| NativeError::Failed(format!("whisper.cpp could not load {path}")))
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        let ptr = self.0.as_ptr();
        // SAFETY: owned pointer, freed once; every `State` holds an Arc to
        // this context, so none outlives it.
        if let Err(e) = catch_exceptions(|| unsafe { whisper_free(ptr) }) {
            log::error!("leaking whisper context: {e}");
        }
    }
}

/// Inference parameters; borrows the language string they point at.
pub(crate) struct FullParams<'a> {
    raw: sys::whisper_full_params,
    _language: PhantomData<&'a CStr>,
}

impl<'a> FullParams<'a> {
    pub(crate) fn greedy(language: &'a CStr) -> Self {
        // SAFETY: plain value constructor.
        let mut raw = unsafe {
            sys::whisper_full_default_params(sys::whisper_sampling_strategy_WHISPER_SAMPLING_GREEDY)
        };
        raw.language = language.as_ptr();
        Self {
            raw,
            _language: PhantomData,
        }
    }

    /// Plain fields to tune. Leave the pointer fields (`language`, callbacks)
    /// alone: their lifetimes aren't tracked.
    pub(crate) fn raw_mut(&mut self) -> &mut sys::whisper_full_params {
        &mut self.raw
    }
}

/// One segment as whisper.cpp reports it; times in centiseconds.
pub(crate) struct RawSegment {
    pub t0: i64,
    pub t1: i64,
    pub text: String,
}

/// Per-inference working memory (KV caches, compute buffers) for a `Context`.
pub(crate) struct State {
    ptr: NonNull<sys::whisper_state>,
    ctx: Arc<Context>,
}

// SAFETY: a state is only used through &mut self, from one thread at a time.
unsafe impl Send for State {}

impl State {
    pub(crate) fn new(ctx: Arc<Context>) -> Result<Self, NativeError> {
        let raw_ctx = ctx.0.as_ptr();
        // SAFETY: `raw_ctx` is a live context, kept alive by `ctx`.
        let ptr = catch_exceptions(|| unsafe { whisper_init_state(raw_ctx) })?;
        NonNull::new(ptr)
            .map(|ptr| Self { ptr, ctx })
            .ok_or_else(|| NativeError::Failed("could not allocate inference state".to_string()))
    }

    /// Run full inference over `samples` (16 kHz mono f32), replacing the
    /// previous result.
    pub(crate) fn full(&mut self, params: &FullParams<'_>, samples: &[f32]) -> Result<(), NativeError> {
        let n_samples = c_int::try_from(samples.len())
            .map_err(|_| NativeError::Failed("audio buffer too long".to_string()))?;
        let (ctx, state, raw) = (self.ctx.0.as_ptr(), self.ptr.as_ptr(), params.raw);
        // SAFETY: live context/state; `samples` and the strings `raw` points
        // at outlive the call.
        let rc = catch_exceptions(|| unsafe {
            whisper_full_with_state(ctx, state, raw, samples.as_ptr(), n_samples)
        })?;
        if rc != 0 {
            return Err(NativeError::Failed(format!("whisper_full returned {rc}")));
        }
        Ok(())
    }

    /// Segments from the last successful `full`.
    pub(crate) fn segments(&self) -> Vec<RawSegment> {
        let state = self.ptr.as_ptr();
        // SAFETY (all calls below): live state, indices below n_segments;
        // these accessors don't throw.
        let n = unsafe { sys::whisper_full_n_segments_from_state(state) };
        (0..n)
            .map(|i| unsafe {
                let text = sys::whisper_full_get_segment_text_from_state(state, i);
                RawSegment {
                    t0: sys::whisper_full_get_segment_t0_from_state(state, i),
                    t1: sys::whisper_full_get_segment_t1_from_state(state, i),
                    text: if text.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(text).to_string_lossy().into_owned()
                    },
                }
            })
            .collect()
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let ptr = self.ptr.as_ptr();
        // SAFETY: owned pointer, freed once.
        if let Err(e) = catch_exceptions(|| unsafe { whisper_free_state(ptr) }) {
            log::error!("leaking whisper state: {e}");
        }
    }
}

/// Route whisper.cpp and ggml logs (one callback covers both) to `log`.
pub(crate) fn install_logging() {
    // SAFETY: the callback is a static fn and ignores user_data.
    unsafe { sys::whisper_log_set(Some(log_trampoline), std::ptr::null_mut()) }
}

unsafe extern "C" fn log_trampoline(level: sys::ggml_log_level, text: *const c_char, _: *mut c_void) {
    if text.is_null() {
        return;
    }
    // SAFETY: whisper.cpp passes a NUL-terminated message.
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    match level {
        sys::ggml_log_level_GGML_LOG_LEVEL_ERROR => log::error!("{text}"),
        sys::ggml_log_level_GGML_LOG_LEVEL_WARN => log::warn!("{text}"),
        sys::ggml_log_level_GGML_LOG_LEVEL_INFO => log::info!("{text}"),
        sys::ggml_log_level_GGML_LOG_LEVEL_DEBUG => log::debug!("{text}"),
        // NONE / CONT (continuation of the previous line) / unknown
        _ => log::trace!("{text}"),
    }
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
        let err = catch_exceptions(|| unsafe { lr_throw_for_test(c"device lost".as_ptr()) });
        assert!(matches!(err, Err(NativeError::Exception(msg)) if msg == "device lost"));
    }

    #[test]
    fn result_passes_through() {
        assert_eq!(catch_exceptions(|| 7).ok(), Some(7));
    }
}
