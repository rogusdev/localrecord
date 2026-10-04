#!/usr/bin/env bash
# Builds the Rust engine for Android and regenerates the uniffi Kotlin
# bindings. Run before ./gradlew assembleDebug whenever rust-engine changes.
#
#   scripts/build-rust.sh            # arm64 only (OnePlus 15)
#   ABIS="arm64-v8a armeabi-v7a" scripts/build-rust.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ABIS="${ABIS:-arm64-v8a}"
# Matches minSdk in app/build.gradle.kts (NDK libvulkan.so stubs start at 24).
export ANDROID_PLATFORM="31"

source "$REPO_ROOT/scripts/env.sh"

echo "==> Host build (for uniffi bindings generation)"
cargo build --release --manifest-path "$REPO_ROOT/rust-engine/Cargo.toml" --lib --features cli

echo "==> Generating Kotlin bindings"
# run from the crate dir: uniffi-bindgen's library mode calls `cargo metadata`
(cd "$REPO_ROOT/rust-engine" && cargo run --release --features cli --bin uniffi-bindgen -- \
    generate \
    --library target/release/libwhisper_engine.so \
    --language kotlin \
    --out-dir "$REPO_ROOT/app/src/main/java")

echo "==> Android build (Vulkan) for: $ABIS"
# The NDK's clang is x86_64-only; on aarch64 hosts build with a stand-in NDK
# that runs the host's clang against the real NDK's sysroot.
if [ "$(uname -m)" = aarch64 ]; then
    ANDROID_NDK_HOME="$("$REPO_ROOT/scripts/host-ndk.sh" "$ANDROID_NDK_HOME")"
    export ANDROID_NDK_HOME ANDROID_NDK_ROOT="$ANDROID_NDK_HOME"
fi
ndk_targets=()
for abi in $ABIS; do
    ndk_targets+=(-t "$abi")
done
# whisper-rs-sys links libc++_shared.so, so it has to ship in jniLibs too.
(cd "$REPO_ROOT/rust-engine" && \
    CMAKE_TOOLCHAIN_FILE="$REPO_ROOT/scripts/android-toolchain.cmake" \
    cargo ndk "${ndk_targets[@]}" -P "$ANDROID_PLATFORM" --link-libcxx-shared \
    -o "$REPO_ROOT/app/src/main/jniLibs" \
    build --release --features vulkan)

echo "==> Done. jniLibs:"
find "$REPO_ROOT/app/src/main/jniLibs" -name "*.so"
