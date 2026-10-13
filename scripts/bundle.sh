#!/bin/sh
# Builds target/Boom3D.app: the release binary, Info.plist and a signature.
# macOS only asks for the System Audio Recording permission when the app runs
# from a bundle that declares NSAudioCaptureUsageDescription.
set -eu
cd "$(dirname "$0")/.."

# Homebrew's rustup is keg-only, so cargo may not be on PATH
command -v cargo >/dev/null || PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cargo build --release

app=target/Boom3D.app
mkdir -p "$app/Contents/MacOS"
cp bundle/Info.plist "$app/Contents/Info.plist"
cp target/release/boom3d "$app/Contents/MacOS/boom3d"

# Signed with the identity from scripts/make-signing-identity.sh, rebuilds
# keep their permissions; ad hoc, macOS asks again after every one
identity="Boom3D Local Signing"
if security find-identity -v -p codesigning | grep -q "\"$identity\""; then
	codesign --force --sign "$identity" "$app"
	echo "built $app, signed with \"$identity\""
else
	codesign --force --sign - "$app"
	echo "built $app, signed ad hoc"
fi
