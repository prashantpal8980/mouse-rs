#!/usr/bin/env bash
# Build a public release: the Windows zip, the Linux tarball, the raw binaries the
# in-app updater downloads, and a signed latest.json, all in dist/<version>/.
#
#   scripts/release.sh 1.0.0 [owner/downloads-repo]
#
# The repo defaults to prashantpal8980/mouse-rs-downloads. When the product is renamed,
# pass the new downloads repo here: the update check and the "download it yourself" link
# are built into the binaries, so they must name the repo the release will live in.
#
# What it guarantees (and stops on if not):
#   * not a test build: no MOUSE_RS_TEST_BUILD, and no test address (192.168.137.x,
#     192.168.122.x, :8000) anywhere in either binary;
#   * log shipping is off by default (it is a config setting, empty unless set);
#   * every file in dist/<version>/ has its SHA-256 in SHA256SUMS.
#
# Nothing is uploaded. Publishing is a separate, deliberate step (gh release create),
# taken only on the owner's go.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
VERSION="${1:?usage: release.sh <version> [owner/downloads-repo]}"
REPO="${2:-prashantpal8980/mouse-rs-downloads}"
case "$VERSION" in *test*) echo "a release version may not contain 'test'" >&2; exit 1;; esac

unset MOUSE_RS_TEST_BUILD
export MOUSE_RS_CHANNEL=release   # signed into latest.json; release builds require it (B73)
export MOUSE_RS_VERSION="$VERSION"
export MOUSE_RS_UPDATE_URL="https://github.com/$REPO/releases/latest/download/latest.json"
export MOUSE_RS_DOWNLOAD_PAGE="https://github.com/$REPO/releases/latest"
ASSETS="https://github.com/$REPO/releases/download/v$VERSION"
[ -d "$REPO_ROOT/.xdo-shim" ] && export RUSTFLAGS="-L $REPO_ROOT/.xdo-shim"
# Its own target dir, so a test build lying in target/ can never be packaged by mistake.
export CARGO_TARGET_DIR="$REPO_ROOT/target/release-build"
WIN_TARGET="x86_64-pc-windows-gnu"
OUT="$REPO_ROOT/dist/$VERSION"
STAGE="$CARGO_TARGET_DIR/stage-$VERSION"

echo "==> mouse-rs $VERSION, updates from $REPO"
cargo test --workspace --quiet
cargo build --release -p mouse
cargo build --release -p mouse --target "$WIN_TARGET"
LIN_BIN="$CARGO_TARGET_DIR/release/mouse-rs"
WIN_BIN="$CARGO_TARGET_DIR/$WIN_TARGET/release/mouse-rs.exe"

echo "==> checking the binaries for test settings"
for f in "$LIN_BIN" "$WIN_BIN"; do
  if LC_ALL=C grep -a -n -E '192\.168\.137\.|192\.168\.122\.|:8000/release' "$f" >/dev/null; then
    echo "!! $f contains a test address; not a release build" >&2
    LC_ALL=C grep -a -o -E '.{0,40}(192\.168\.137\.|192\.168\.122\.|:8000/release).{0,40}' "$f" | head -5 >&2
    exit 1
  fi
  LC_ALL=C grep -a -q "$MOUSE_RS_UPDATE_URL" "$f" || { echo "!! $f does not check $MOUSE_RS_UPDATE_URL" >&2; exit 1; }
done
echo "    no test addresses; updates point at $REPO"

rm -rf "$OUT" "$STAGE"
mkdir -p "$OUT" "$STAGE"

# ---------------- the raw binaries, for the in-app updater ----------------
cp "$WIN_BIN" "$OUT/mouse-rs.exe"
cp "$LIN_BIN" "$OUT/mouse-rs"
chmod +x "$OUT/mouse-rs"

# ---------------- Windows zip ----------------
WX="$STAGE/mouse-rs-$VERSION-windows-x86_64"
mkdir -p "$WX"
cp "$WIN_BIN" "$WX/mouse-rs.exe"
cat > "$WX/README.txt" <<EOF
mouse-rs $VERSION for Windows 10 and 11

1. Double-click mouse-rs.exe.
   Windows may say "Windows protected your PC" the first time, because the program
   is new and not yet signed. Click "More info", then "Run anyway".
2. Choose what this computer does:
   "Shares its mouse" on the computer whose keyboard and mouse you use,
   "Is controlled" on the other one.
3. On the computer that shares its mouse, press "Pair a new device".
   Type the 6-digit code it shows into the other computer's mouse-rs window.

That is all. Push the pointer off the edge of the screen to move to the other
computer. Press Scroll Lock to take control back at any time.
Updates are offered inside the app: About > Check now > Update now.
EOF
( cd "$STAGE" && python3 -c "import shutil,sys; shutil.make_archive(sys.argv[1],'zip','.',sys.argv[2])" \
    "$OUT/mouse-rs-$VERSION-windows-x86_64" "mouse-rs-$VERSION-windows-x86_64" )

# ---------------- Linux tarball ----------------
LX="$STAGE/mouse-rs-$VERSION-linux-x86_64"
mkdir -p "$LX/lib"
cp "$LIN_BIN" "$LX/mouse-rs"
LIBXDO="$(ldconfig -p 2>/dev/null | sed -nE 's/.*=> (.*libxdo\.so\.[0-9]+)$/\1/p' | head -1 || true)"
[ -n "$LIBXDO" ] && cp -L "$LIBXDO" "$LX/lib/libxdo.so.3"
cat > "$LX/run.sh" <<'EOF'
#!/usr/bin/env bash
# Start mouse-rs with the libxdo bundled next to it, so the libxdo3 package is not needed.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export LD_LIBRARY_PATH="$DIR/lib:${LD_LIBRARY_PATH:-}"
exec "$DIR/mouse-rs" "$@"
EOF
chmod +x "$LX/run.sh" "$LX/mouse-rs"
cat > "$LX/README.txt" <<EOF
mouse-rs $VERSION for Linux (x86_64, X11 desktops)

Needs an X11 session (most desktops offer one at the login screen; Wayland is not
supported yet) and two common libraries:
    sudo apt install libgtk-3-0 libayatana-appindicator3-1

1. Run ./run.sh
2. Choose what this computer does:
   "Shares its mouse" on the computer whose keyboard and mouse you use,
   "Is controlled" on the other one.
3. On the computer that shares its mouse, press "Pair a new device".
   Type the 6-digit code it shows into the other computer's mouse-rs window.

Push the pointer off the edge of the screen to move to the other computer. Press
Scroll Lock to take control back at any time. Settings > Startup can start it at login.
Updates are offered inside the app: About > Check now > Update now.
EOF
tar -C "$STAGE" -czf "$OUT/mouse-rs-$VERSION-linux-x86_64.tar.gz" "mouse-rs-$VERSION-linux-x86_64"

# ---------------- signed latest.json + checksums ----------------
"$REPO_ROOT/scripts/sign-release.sh" "$OUT" "$VERSION" "$ASSETS" "mouse-rs $VERSION is available."
( cd "$OUT" && sha256sum mouse-rs mouse-rs.exe ./*.zip ./*.tar.gz latest.json latest.json.sig > SHA256SUMS )

echo "==> $VERSION is ready in $OUT (nothing uploaded):"
ls -lh "$OUT"
