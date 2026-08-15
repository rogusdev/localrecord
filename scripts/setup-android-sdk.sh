#!/usr/bin/env bash
# Installs Android SDK cmdline-tools, platform, build-tools, platform-tools and NDK
# into $ANDROID_HOME (default: ~/Android/Sdk). Idempotent — safe to re-run.
set -euo pipefail

ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
CMDLINE_TOOLS_URL="https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip"
PLATFORM="android-36"
BUILD_TOOLS="36.0.0"
NDK_VERSION="28.2.13676358"

mkdir -p "$ANDROID_HOME"

if [ ! -x "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" ]; then
    echo "==> Downloading cmdline-tools"
    tmp=$(mktemp -d)
    curl -fsSL "$CMDLINE_TOOLS_URL" -o "$tmp/tools.zip"
    unzip -q "$tmp/tools.zip" -d "$tmp"
    mkdir -p "$ANDROID_HOME/cmdline-tools"
    rm -rf "$ANDROID_HOME/cmdline-tools/latest"
    mv "$tmp/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest"
    rm -rf "$tmp"
fi

SDKMANAGER="$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager"

echo "==> Accepting licenses"
yes | "$SDKMANAGER" --licenses > /dev/null || true

echo "==> Installing SDK packages (platform $PLATFORM, build-tools $BUILD_TOOLS, NDK $NDK_VERSION)"
"$SDKMANAGER" \
    "platform-tools" \
    "platforms;$PLATFORM" \
    "build-tools;$BUILD_TOOLS" \
    "ndk;$NDK_VERSION"

echo "==> Done. SDK at $ANDROID_HOME, NDK at $ANDROID_HOME/ndk/$NDK_VERSION"
