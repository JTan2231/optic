#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
if command -v cargo >/dev/null 2>&1; then
    optic_cargo=cargo
elif [ -x "$HOME/.cargo/bin/cargo" ]; then
    optic_cargo="$HOME/.cargo/bin/cargo"
else
    echo "Rust is required. Install it from https://rustup.rs." >&2
    exit 1
fi
"$optic_cargo" build --release
optic_app="$PWD/dist/Optic.app"
mkdir -p "$optic_app/Contents/MacOS"
cp target/release/optic "$optic_app/Contents/MacOS/optic"
cp native/Info.plist "$optic_app/Contents/Info.plist"
codesign --force --sign - --identifier dev.joey.optic "$optic_app"
printf 'Built %s\nLaunch with: open "%s"\n' "$optic_app" "$optic_app"
