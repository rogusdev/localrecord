# Source this before building: `source scripts/env.sh`
# Sets Android SDK/NDK env vars for Gradle and cargo-ndk (WSL2/Linux).

export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export ANDROID_SDK_ROOT="$ANDROID_HOME"

NDK_VERSION="28.2.13676358"
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/$NDK_VERSION"
export ANDROID_NDK_ROOT="$ANDROID_NDK_HOME"

# glslc (Vulkan shader compiler) ships with the NDK; ggml's Vulkan backend
# needs it on PATH at build time to compile compute shaders.
export PATH="$ANDROID_NDK_HOME/shader-tools/linux-x86_64:$ANDROID_HOME/platform-tools:$PATH"

# aarch64 hosts: the SDK/NDK's x86_64 binaries (aapt2, glslc) run under
# qemu-user via binfmt and need an x86_64 glibc/libstdc++ root to load from.
if [ "$(uname -m)" = aarch64 ]; then
    export QEMU_LD_PREFIX="${QEMU_LD_PREFIX:-$HOME/.cache/localrecord/x86root}"
fi
