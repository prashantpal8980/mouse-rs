#!/usr/bin/env bash
# Rehearse the update notice (R36) against a release served from this machine, because
# the public downloads repo does not exist until launch.
#
#   scripts/fake-release.sh install 1.0.0-test1
#       Builds Linux + Windows as 1.0.0-test1, looking for updates on this machine's
#       :8000 server, in the normal target dirs. Then: restart mouse-rs on Kali from
#       target/release, and serve the exe to the laptop with serve-windows.sh --no-build.
#
#   scripts/fake-release.sh publish 1.0.0-test2
#       Builds 1.0.0-test2 into a separate target dir (so the copy Kali runs is not
#       touched), puts both binaries in ~/mouse-serve/release/ and signs latest.json.
#       Both machines then show "Version 1.0.0-test2 is available".
#
# Never ship these builds: they look for updates at a test address. The release build
# (Sat 31) is made without MOUSE_RS_UPDATE_URL, and is checked for 192.168.137.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
MODE="${1:?install|publish}"
VERSION="${2:?version}"
BASE="http://192.168.137.2:8000/release"
export MOUSE_RS_VERSION="$VERSION"
export MOUSE_RS_UPDATE_URL="$BASE/latest.json"
# Checks every minute instead of only at start (update::TEST_BUILD).
export MOUSE_RS_TEST_BUILD=1
# Signed as a rehearsal: release builds refuse it (B73).
export MOUSE_RS_CHANNEL=test
[ -d "$REPO_ROOT/.xdo-shim" ] && export RUSTFLAGS="-L $REPO_ROOT/.xdo-shim"

case "$MODE" in
  install)
    cargo build --release -p mouse
    cargo build --release --target x86_64-pc-windows-gnu -p mouse
    echo "==> built $VERSION. Restart mouse-rs on Kali, then: scripts/serve-windows.sh --no-build"
    ;;
  publish)
    export CARGO_TARGET_DIR="$REPO_ROOT/target/fake-release"
    cargo build --release -p mouse
    cargo build --release --target x86_64-pc-windows-gnu -p mouse
    OUT="$HOME/mouse-serve/release"
    mkdir -p "$OUT"
    cp -f "$CARGO_TARGET_DIR/release/mouse-rs" "$OUT/mouse-rs"
    cp -f "$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/mouse-rs.exe" "$OUT/mouse-rs.exe"
    "$REPO_ROOT/scripts/sign-release.sh" "$OUT" "$VERSION" "$BASE" "A test release, to try updating."
    echo "==> $VERSION is published at $BASE/latest.json (the :8000 server must be running)"
    ;;
  *)
    echo "usage: $0 install|publish <version>" >&2
    exit 1
    ;;
esac
