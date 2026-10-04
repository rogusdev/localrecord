# CMake toolchain for whisper-rs-sys's whisper.cpp build (CMAKE_TOOLCHAIN_FILE,
# set by scripts/build-rust.sh). The cmake crate passes no ANDROID_ABI or
# ANDROID_PLATFORM, so derive them from cargo's TARGET (inherited from the build
# script) and ANDROID_PLATFORM, then defer to the NDK's own toolchain file.

set(ANDROID_NDK "$ENV{ANDROID_NDK_HOME}")
set(ANDROID_PLATFORM "$ENV{ANDROID_PLATFORM}")

if("$ENV{TARGET}" MATCHES "^aarch64-linux-android")
    set(ANDROID_ABI arm64-v8a)
elseif("$ENV{TARGET}" MATCHES "^armv7-linux-androideabi")
    set(ANDROID_ABI armeabi-v7a)
elseif("$ENV{TARGET}" MATCHES "^x86_64-linux-android")
    set(ANDROID_ABI x86_64)
elseif("$ENV{TARGET}" MATCHES "^i686-linux-android")
    set(ANDROID_ABI x86)
else()
    message(FATAL_ERROR "android-toolchain.cmake: unsupported TARGET '$ENV{TARGET}'")
endif()

include("${ANDROID_NDK}/build/cmake/android.toolchain.cmake")
