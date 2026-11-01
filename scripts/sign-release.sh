#!/usr/bin/env bash
# Write and sign latest.json for a release (R36, the update notice).
#
# Usage:  scripts/sign-release.sh <dir> <version> <base-url> [notes]
#
#   <dir>       holds the release binaries: mouse-rs.exe (Windows) and/or mouse-rs (Linux)
#   <version>   e.g. 1.0.0, or 1.0.0-test2 for a rehearsal
#   <base-url>  where those files will be downloadable, e.g.
#               https://github.com/prashantpal8980/mouse-rs-downloads/releases/download/v1.0.0
#   [notes]     one or two sentences for the banner
#
# Writes <dir>/latest.json and <dir>/latest.json.sig. Upload both next to the binaries.
# The app believes latest.json only if latest.json.sig is a valid Ed25519 signature
# from the key below, and a download only if its size and SHA-256 match latest.json.
#
# The key never goes into the repo. LOSING IT means no installed copy can ever accept
# an update again, so keep a backup of it somewhere safe (not on GitHub).
set -euo pipefail

KEY="${MOUSE_RS_SIGNING_KEY:-$HOME/.config/mouse-rs-release/update-signing.pem}"
DIR="${1:?release dir}"
VERSION="${2:?version}"
BASE="${3:?base url}"
NOTES="${4:-}"
# Signed into the description. Release builds accept only "release" (B73); rehearsals
# (fake-release.sh) set "test", so their descriptions can never be replayed to customers.
CHANNEL="${MOUSE_RS_CHANNEL:-release}"
case "$CHANNEL" in release|test) ;; *) echo "MOUSE_RS_CHANNEL must be release or test" >&2; exit 1;; esac

[ -f "$KEY" ] || { echo "no signing key at $KEY" >&2; exit 1; }
[ "$(uname)" = "Linux" ] || { echo "run this on the Linux release machine" >&2; exit 1; }

entry() {   # <platform key> <file>
  local f="$DIR/$2"
  [ -f "$f" ] || return 0
  printf '    "%s": {"url": "%s/%s", "size": %s, "sha256": "%s"}' \
    "$1" "${BASE%/}" "$2" "$(stat -c%s "$f")" "$(sha256sum "$f" | cut -d' ' -f1)"
}

{
  entries=()
  e=$(entry windows-x86_64 mouse-rs.exe); [ -n "$e" ] && entries+=("$e")
  e=$(entry linux-x86_64 mouse-rs);       [ -n "$e" ] && entries+=("$e")
  [ ${#entries[@]} -gt 0 ] || { echo "no mouse-rs or mouse-rs.exe in $DIR" >&2; exit 1; }
  printf '{\n  "version": "%s",\n  "channel": "%s",\n  "notes": %s,\n  "files": {\n' "$VERSION" "$CHANNEL" \
    "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$NOTES")"
  ( IFS=$'\n'; printf '%s' "${entries[0]}"; for x in "${entries[@]:1}"; do printf ',\n%s' "$x"; done )
  printf '\n  }\n}\n'
} > "$DIR/latest.json"

openssl pkeyutl -sign -rawin -inkey "$KEY" -in "$DIR/latest.json" | xxd -p | tr -d '\n' > "$DIR/latest.json.sig"
echo >> "$DIR/latest.json.sig"

# Check our own work with the public key, the way the app will.
openssl pkey -in "$KEY" -pubout -out "$DIR/.pub.pem"
xxd -r -p "$DIR/latest.json.sig" > "$DIR/.sig.bin"
openssl pkeyutl -verify -rawin -pubin -inkey "$DIR/.pub.pem" -in "$DIR/latest.json" -sigfile "$DIR/.sig.bin" >/dev/null
rm -f "$DIR/.pub.pem" "$DIR/.sig.bin"

echo "==> signed $DIR/latest.json"
cat "$DIR/latest.json"
