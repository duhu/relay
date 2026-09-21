#!/usr/bin/env bash
# Build the dev bundle, install it as /Applications/Relay-dev.app, sign it and
# launch it. The symlink in ~/.local/bin lets `relay ...` reach the same binary.
set -euo pipefail
cd "$(dirname "$0")/.."

pnpm tauri build --debug --config src-tauri/tauri.dev.conf.json

# Cargo puts the bundle under the workspace target dir, i.e. the repo root.
BUILT=target/debug/bundle/macos/Relay-dev.app
APP=/Applications/Relay-dev.app

pkill -x relay 2>/dev/null || true
rm -rf "$APP"
cp -R "$BUILT" "$APP"
# Relay bundles no helper binaries since M2, so `Resources/bin` is a layout
# that no longer exists; the directory test keeps the guard harmless either way.
if [ -d "$APP/Contents/Resources/bin" ]; then
  chmod +x "$APP"/Contents/Resources/bin/*
fi

ID=$(security find-identity -v -p codesigning | awk '/Apple Development/{print $2; exit}')
if [ -z "$ID" ]; then
  ID="-"
  echo "WARN: ad-hoc signing — Input Monitoring grant will not survive reinstall" >&2
fi
if [ -d "$APP/Contents/Resources/bin" ]; then
  codesign --force --sign "$ID" "$APP"/Contents/Resources/bin/*
fi
codesign --force --sign "$ID" --identifier work.bam.relay.dev "$APP"

mkdir -p ~/.local/bin
ln -sf "$APP/Contents/MacOS/relay" ~/.local/bin/relay

open -a "$APP"
