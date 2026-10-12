#!/usr/bin/env bash
# Ship this Mac's installed /Applications/Relay-dev.app to another Mac over ssh
# and swap it in there: zip it, scp it, then on the remote stop relay, replace
# the bundle, verify the signature, re-link ~/.local/bin/relay and launch it.
#
# It ships the *installed* app, already built and signed by dev-install.sh, so
# run scripts/dev-install.sh first. Nothing is built here.
#
# Safety (AGENTS.md): reinstall a Mac only while the person is on a different
# one. If the remote's `relay --devices` sees the keyboard or mouse, someone is
# sitting there and the script refuses unless --force is given.
#
# The remote config (~/Library/Application Support/Relay/config.json) is never
# read or written: only the bundle and the symlink change.
set -euo pipefail

usage() {
  echo "Usage: scripts/install-remote.sh <user@host> [--force]" >&2
  exit 2
}

HOST=""
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    -*) usage ;;
    *)
      [ -z "$HOST" ] || usage
      HOST="$arg"
      ;;
  esac
done
[ -n "$HOST" ] || usage

APP=/Applications/Relay-dev.app
RELAY="$APP/Contents/MacOS/relay"

if [ ! -d "$APP" ]; then
  echo "relay: $APP is not installed here; run scripts/dev-install.sh first" >&2
  exit 2
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

ditto -c -k --keepParent "$APP" "$TMP/Relay-dev.app.zip"

# A remote with no Relay installed yet prints nothing and passes.
if ! seen=$(ssh -o BatchMode=yes -o ConnectTimeout=8 "$HOST" \
  '/Applications/Relay-dev.app/Contents/MacOS/relay --devices 2>/dev/null || true'); then
  echo "relay: cannot reach $HOST over ssh" >&2
  exit 1
fi
if [ -n "$seen" ] && [ "$FORCE" -ne 1 ]; then
  echo "relay: $HOST sees: $seen" >&2
  echo "the keyboard or mouse is on that Mac — someone is sitting there; run this from that Mac, or pass --force" >&2
  exit 1
fi

REMOTE_TMP=$(ssh "$HOST" 'mktemp -d')
scp -q "$TMP/Relay-dev.app.zip" "$HOST:$REMOTE_TMP/"

# The heredoc is quoted, so everything in it expands on the remote ($HOME
# included); REMOTE_TMP arrives as the first positional argument. Its stdout
# is the remote `relay --status`, captured for the comparison below.
rc=0
status=$(ssh "$HOST" "bash -s -- $(printf '%q' "$REMOTE_TMP")" <<'REMOTE'
set -euo pipefail
REMOTE_TMP=$1
trap 'rm -rf "$REMOTE_TMP"' EXIT
pkill -x relay 2>/dev/null || true
rm -rf /Applications/Relay-dev.app
ditto -x -k "$REMOTE_TMP/Relay-dev.app.zip" /Applications/
xattr -dr com.apple.quarantine /Applications/Relay-dev.app 2>/dev/null || true
codesign --verify --deep --strict /Applications/Relay-dev.app
mkdir -p "$HOME/.local/bin"
ln -sf /Applications/Relay-dev.app/Contents/MacOS/relay "$HOME/.local/bin/relay"
open -a /Applications/Relay-dev.app
sleep 4
/Applications/Relay-dev.app/Contents/MacOS/relay --status
REMOTE
) || rc=$?
[ -z "$status" ] || printf '%s\n' "$status"
if [ "$rc" -ne 0 ]; then
  echo "relay: the remote install failed (exit $rc)" >&2
  exit 1
fi

here=$("$RELAY" --version)
here=${here#relay }
remote=$(printf '%s\n' "$status" | grep -m1 '^version:' || true)

echo
echo "here:   $here"
if [ -n "$remote" ]; then
  echo "remote: ${remote#version: }"
else
  echo "remote: status unavailable"
fi

# Defensive: catches a stale app that pkill missed and that still answers.
if [ "${remote#version: }" != "$here" ]; then
  echo "the remote is running a different build" >&2
  exit 1
fi
