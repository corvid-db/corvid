#!/usr/bin/env bash
# build-android.sh — cross-compile the corvid-ffi cdylib for Android and
# stage the SAME artifact-set shape the desktop release legs ship
# (corvid-ffi-<tag>-<target>.tar.gz holding libcorvid.so + corvid.h +
# golden/), for the two ABIs the corvid-android AAR bundles:
# aarch64-linux-android (arm64-v8a, devices + modern emulators) and
# x86_64-linux-android (emulators). 32-bit ABIs are deliberately out
# (Play has required 64-bit since 2019; x86_64 covers the emulator case).
#
# Host requirements: bash, the Android NDK (ANDROID_NDK_HOME — CI
# downloads r28b, matching the version this script was proven with
# locally), rustup with the two targets installed. The NDK's clang
# wrapper toolchain links (no cargo-ndk, no SDK — a cdylib needs no
# java side); the engine links against android24 (API 24), a superset
# of the corvid-android AAR's minSdk 26.
#
# Usage: scripts/release/build-android.sh   (from anywhere; cd's itself)
# Env:   ANDROID_NDK_HOME  path to the NDK root (the one with toolchains/)
#        CORVID_TAG       override the tag derived from the workspace version
# Output: corvid-ffi-<tag>-aarch64-linux-android.tar.gz
#         corvid-ffi-<tag>-x86_64-linux-android.tar.gz   (cwd)
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$(pwd)"

TAG="${CORVID_TAG:-v$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && /^version = /{gsub(/[";]/,"",$3); print $3; exit}' Cargo.toml)}"
case "$TAG" in
    v[0-9]*) ;;
    *) echo "build-android: cannot derive a v-tag from the workspace version (got '$TAG')" >&2; exit 1 ;;
esac

NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-${NDK_ROOT:-}}}"
if [ -z "$NDK" ] || [ ! -d "$NDK/toolchains" ]; then
    echo "build-android: ANDROID_NDK_HOME must point at an NDK root (one with toolchains/)" >&2
    exit 1
fi
PREBUILT="$NDK/toolchains/llvm/prebuilt"
case "$(uname -s)" in
    Darwin) HOST_WANT=darwin ;;
    Linux)  HOST_WANT=linux ;;
    *) echo "build-android: unsupported host $(uname -s)" >&2; exit 1 ;;
esac
# An NDK may ship only one host's prebuilt dir (r28b on macOS ships
# darwin-x86_64 alone and runs under Rosetta on arm64 hosts), so the
# preferred tag falls back to the first dir matching the host OS.
if [ -d "$PREBUILT/$HOST_WANT-arm64" ]; then
    HOST_TAG="$HOST_WANT-arm64"
elif [ -d "$PREBUILT/$HOST_WANT-x86_64" ]; then
    HOST_TAG="$HOST_WANT-x86_64"
else
    HOST_TAG="$(ls "$PREBUILT" 2>/dev/null | grep "^$HOST_WANT" | head -1 || true)"
    [ -n "$HOST_TAG" ] || { echo "build-android: $PREBUILT has no ${HOST_WANT}-* toolchain" >&2; exit 1; }
fi
TC="$PREBUILT/$HOST_TAG/bin"
API=24   # the minimum the engine .so targets; the AAR's minSdk is 26

export RUSTFLAGS="-C link-arg=-Wl,-soname,libcorvid.so"
for T in aarch64-linux-android x86_64-linux-android; do
    LINKER_VAR="CARGO_TARGET_$(echo "$T" | tr 'a-z-' 'A-Z_')_LINKER"
    export "$LINKER_VAR=$TC/$T$API-clang"
    [ -x "$TC/$T$API-clang" ] || { echo "build-android: $TC/$T$API-clang not found (NDK without prebuilt $T?)" >&2; exit 1; }
    echo "build-android: cargo build -p corvid-ffi --release --target $T"
    cargo build -p corvid-ffi --release --target "$T"
    SO="target/$T/release/libcorvid.so"
    [ -f "$SO" ] || { echo "build-android: $SO missing after build" >&2; exit 1; }
    # Relocatable-dylib hygiene, same as the desktop linux legs: the .so
    # carries a SONAME so the JNI shim's DT_NEEDED resolves on Android's
    # linker namespace (nativeLibraryDir).
    if command -v llvm-readelf >/dev/null 2>&1; then
        llvm-readelf -d "$SO" | grep -q 'SONAME.*libcorvid.so' || { echo "build-android: $SO lacks SONAME libcorvid.so" >&2; exit 1; }
    fi

    DIR="ffi-stage/corvid-ffi-$TAG-$T"
    rm -rf "$DIR"; mkdir -p "$DIR/golden"
    cp "$SO" "$DIR/"
    cp crates/corvid-ffi/corvid.h "$DIR/"
    cp crates/corvid-ffi/golden/*.txt "$DIR/golden/"
    tar -czf "corvid-ffi-$TAG-$T.tar.gz" -C ffi-stage "corvid-ffi-$TAG-$T"
    echo "build-android: corvid-ffi-$TAG-$T.tar.gz staged"
done
rm -rf ffi-stage
echo "build-android: done (tag $TAG, NDK $(basename "$NDK"), API $API)"
