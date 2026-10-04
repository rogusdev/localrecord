// C++ exception barrier for the Rust engine (see src/native.rs). whisper.cpp
// and ggml's Vulkan backend throw (e.g. vk::Queue::submit: ErrorDeviceLost);
// Rust can't catch foreign exceptions, so throwing calls run inside this
// try/catch and come back as an error string instead of aborting the process.

#include <cstddef>
#include <cstdio>
#include <exception>
#include <stdexcept>

// Calls f(data). Returns 0, or 1 after writing the exception's message into
// err (NUL-terminated, truncated to err_len).
extern "C" int lr_catch_exceptions(void (*f)(void *), void *data, char *err, size_t err_len) {
    try {
        f(data);
        return 0;
    } catch (const std::exception &e) {
        std::snprintf(err, err_len, "%s", e.what());
    } catch (...) {
        std::snprintf(err, err_len, "unknown C++ exception");
    }
    return 1;
}

// Test hook for native.rs: throws std::runtime_error(msg). Unreferenced
// outside tests, so it never reaches the shipped library.
extern "C" void lr_throw_for_test(const char *msg) { throw std::runtime_error(msg); }
