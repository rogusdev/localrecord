#!/usr/bin/env bash
# Builds the Rust engine for Android and regenerates the uniffi Kotlin
# bindings. Run before ./gradlew assembleDebug whenever rust-engine changes.
#
#   scripts/build-rust.sh            # arm64 only (OnePlus 15)
#   ABIS="arm64-v8a armeabi-v7a" scripts/build-rust.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ABIS="${ABIS:-arm64-v8a}"

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
ndk_targets=()
for abi in $ABIS; do
    ndk_targets+=(-t "$abi")
done
(cd "$REPO_ROOT/rust-engine" && cargo ndk "${ndk_targets[@]}" \
    -o "$REPO_ROOT/app/src/main/jniLibs" \
    build --release --features vulkan)

echo "==> Done. jniLibs:"
find "$REPO_ROOT/app/src/main/jniLibs" -name "*.so"
