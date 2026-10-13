#!/bin/sh
# Builds target/Boom3D.app: the release binary, Info.plist and an ad-hoc
# signature. macOS only asks for the System Audio Recording permission when
# the app runs from a bundle that declares NSAudioCaptureUsageDescription.
set -eu
cd "$(dirname "$0")/.."

# Homebrew's rustup is keg-only, so cargo may not be on PATH
command -v cargo >/dev/null || PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cargo build --release

app=target/Boom3D.app
mkdir -p "$app/Contents/MacOS"
cp bundle/Info.plist "$app/Contents/Info.plist"
cp target/release/boom3d "$app/Contents/MacOS/boom3d"
codesign --force --sign - "$app"
echo "built $app"
