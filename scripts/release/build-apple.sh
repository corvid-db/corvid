#!/usr/bin/env bash
# build-apple.sh — build CorvidEngine.xcframework, the binary artifact the
# corvid-swift SPM package pins (binaryTarget(url:checksum:)) for Apple
# platforms, and zip it with the top-level directory SPM requires.
#
# Slices (staticlib — bare dylibs outside frameworks are not supported on
# iOS; static linking is the Rust-on-Apple distribution norm):
#   ios-arm64            aarch64-apple-ios            (device)
#   ios-arm64_x86_64-sim aarch64-apple-ios-sim lipo'd  (simulator, both archs)
#   macos-arm64_x86_64   aarch64/x86_64-apple-darwin lipo'd
# watchOS/visionOS/tvOS slices are deliberately absent (no demand; the
# xcframework grows a slice the same way when one arrives).
#
# Headers: each slice carries corvid.h plus an umbrella CorvidEngine.h, so
# SwiftPM forms the `CorvidEngine` clang module from the binary target
# (import CorvidEngine -> all corvid_* symbols).
#
# Host requirements: macOS with Xcode (xcodebuild + lipo), rustup with the
# five targets. Usage: scripts/release/build-apple.sh (from anywhere).
# Env: CORVID_TAG override. Output (cwd): CorvidEngine.xcframework/,
# corvid-swift-<tag>.zip, and the zip's sha256 on stdout.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$(pwd)"

TAG="${CORVID_TAG:-v$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && /^version = /{gsub(/[";]/,"",$3); print $3; exit}' Cargo.toml)}"
case "$TAG" in
    v[0-9]*) ;;
    *) echo "build-apple: cannot derive a v-tag from the workspace version (got '$TAG')" >&2; exit 1 ;;
esac

OUT="${CORVID_APPLE_OUT:-target/apple}"
STAGE="$OUT/CorvidEngine.xcframework"
rm -rf "$OUT"; mkdir -p "$OUT/slices" "$OUT/headers"

echo "build-apple: building staticlibs (5 targets)"
for T in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build -p corvid-ffi --release --target "$T"
    [ -f "target/$T/release/libcorvid.a" ] || { echo "build-apple: target/$T/release/libcorvid.a missing" >&2; exit 1; }
done

echo "build-apple: lipo (macos fat, ios-sim fat; device slice stays thin)"
lipo -create -output "$OUT/slices/libcorvid-macos.a" \
    target/aarch64-apple-darwin/release/libcorvid.a \
    target/x86_64-apple-darwin/release/libcorvid.a
lipo -create -output "$OUT/slices/libcorvid-ios-sim.a" \
    target/aarch64-apple-ios-sim/release/libcorvid.a \
    target/x86_64-apple-ios/release/libcorvid.a
cp target/aarch64-apple-ios/release/libcorvid.a "$OUT/slices/libcorvid-ios.a"
lipo -info "$OUT/slices/libcorvid-macos.a" "$OUT/slices/libcorvid-ios-sim.a"

echo "build-apple: headers (umbrella + module map + generated corvid.h)"
cp crates/corvid-ffi/corvid.h "$OUT/headers/"
printf '/* Umbrella for the CorvidEngine clang module (SwiftPM binary target). */\n#include "corvid.h"\n' > "$OUT/headers/CorvidEngine.h"
# SwiftPM forms the clang module for a BINARY library target only when the
# slice's Headers/ carries an explicit module map (verified empirically:
# umbrella + header alone -> "no such module 'CorvidEngine'").
printf 'module CorvidEngine {\n    umbrella header "CorvidEngine.h"\n    export *\n}\n' > "$OUT/headers/module.modulemap"

echo "build-apple: xcodebuild -create-xcframework"
rm -rf "$STAGE"
xcodebuild -create-xcframework \
    -library "$OUT/slices/libcorvid-ios.a"     -headers "$OUT/headers" \
    -library "$OUT/slices/libcorvid-ios-sim.a" -headers "$OUT/headers" \
    -library "$OUT/slices/libcorvid-macos.a"   -headers "$OUT/headers" \
    -output "$STAGE"

ZIP="corvid-swift-$TAG.zip"
echo "build-apple: zipping ($ZIP) — top-level dir must be the xcframework"
ditto -c -k --sequesterRsrc --keepParent "$STAGE" "$ZIP"
echo "build-apple: sha256"
shasum -a 256 "$ZIP"
echo "build-apple: done (tag $TAG; slices: ios-arm64, ios sim fat, macos fat)"
