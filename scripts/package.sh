#!/usr/bin/env bash
# Package mouse-rs into distributable artifacts:
#   dist/mouse-rs-<ver>-windows-x86_64.zip   (GUI exe; also runs server/client)
#   dist/mouse-rs-<ver>-linux-x86_64.tar.gz  (GUI + slim headless + bundled libxdo)
#   dist/SHA256SUMS
#
# Run from anywhere; paths resolve to the repo root. Builds release for both the
# host (x86_64-unknown-linux-gnu) and Windows (x86_64-pc-windows-gnu).
#
# The Linux GUI build links libxdo (via the tray's libappindicator). If the linker
# can't find `libxdo.so` (no `libxdo-dev`), this script auto-creates a shim symlink
# to the runtime `libxdo.so.3` so the build still succeeds — no root needed.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# shellcheck disable=SC1090
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

WIN_TARGET="x86_64-pc-windows-gnu"
VER="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
DIST="$REPO_ROOT/dist"
STAGE="$REPO_ROOT/target/dist-staging"

echo "==> packaging mouse-rs v$VER"
rm -rf "$DIST" "$STAGE"
mkdir -p "$DIST" "$STAGE"

# --- locate + shim libxdo so the Linux GUI build links without libxdo-dev ---
LIBXDO="$(ldconfig -p 2>/dev/null | sed -nE 's/.*=> (.*libxdo\.so\.[0-9]+)$/\1/p' | head -1 || true)"
[ -z "$LIBXDO" ] && [ -f /usr/lib/x86_64-linux-gnu/libxdo.so.3 ] && LIBXDO=/usr/lib/x86_64-linux-gnu/libxdo.so.3
RUSTFLAGS_GUI=""
if [ -n "$LIBXDO" ]; then
  SHIM="$REPO_ROOT/.xdo-shim"
  mkdir -p "$SHIM"
  ln -sf "$LIBXDO" "$SHIM/libxdo.so"
  RUSTFLAGS_GUI="-L $SHIM"
  echo "==> using libxdo: $LIBXDO"
else
  echo "!! libxdo.so.3 not found — Linux GUI build may fail; install xdotool/libxdo3" >&2
fi

# --- build everything ---
# The two host variants share target/release/mouse-rs, so build each then immediately
# copy it to its own staged name before the next build overwrites it.
echo "==> building Linux headless (release, --no-default-features)"
cargo build --release -p mouse --no-default-features
HEADLESS_LINUX="$STAGE/mouse-rs-headless"
cp target/release/mouse-rs "$HEADLESS_LINUX"

echo "==> building Linux GUI (release)"
RUSTFLAGS="$RUSTFLAGS_GUI" cargo build --release -p mouse
cp target/release/mouse-rs "$STAGE/mouse-rs"

echo "==> building Windows GUI (release)"
cargo build --release -p mouse --target "$WIN_TARGET"
cp "target/$WIN_TARGET/release/mouse-rs.exe" "$STAGE/mouse-rs.exe"

# ---------------- Linux tarball ----------------
echo "==> staging Linux tarball"
LX="$STAGE/mouse-rs-$VER-linux-x86_64"
mkdir -p "$LX/lib"
cp "$STAGE/mouse-rs" "$LX/mouse-rs"
cp "$HEADLESS_LINUX" "$LX/mouse-rs-headless"
cp config.example.json "$LX/config.example.json"
[ -n "$LIBXDO" ] && cp -L "$LIBXDO" "$LX/lib/libxdo.so.3"

cat > "$LX/run-gui.sh" <<'EOF'
#!/usr/bin/env bash
# Launch the GUI using the bundled libxdo (so you don't need the libxdo3 package).
# gtk3 + libayatana-appindicator are still required from the system (see README).
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export LD_LIBRARY_PATH="$DIR/lib:${LD_LIBRARY_PATH:-}"
exec "$DIR/mouse-rs" "$@"
EOF
chmod +x "$LX/run-gui.sh" "$LX/mouse-rs" "$LX/mouse-rs-headless"

cat > "$LX/README.txt" <<EOF
mouse-rs v$VER — Linux (x86_64)

Share one keyboard/mouse between this machine and another over the LAN.
Requires an X11 session (not Wayland) on Linux.

WHAT'S HERE
  mouse-rs-headless   Slim, fully self-contained binary (no GUI). RECOMMENDED for a
                      dedicated server/client. No extra packages needed.
  mouse-rs            The same tool plus the iced control panel + system tray.
  run-gui.sh          Launches mouse-rs with the bundled libxdo on LD_LIBRARY_PATH.
  lib/libxdo.so.3     Bundled (only the GUI needs it).
  config.example.json Sample config.

QUICK START (headless)
  1. Copy the sample config and edit it:
       mkdir -p ~/.mouse_share
       cp config.example.json ~/.mouse_share/config.json
       \$EDITOR ~/.mouse_share/config.json
     Set a long 'shared_secret' (IDENTICAL on both machines), the role, and on the
     client set 'server_host' to the server's LAN IP.
  2. On the machine with the keyboard/mouse:   ./mouse-rs-headless server
     On the other machine:                     ./mouse-rs-headless client

GUI
  ./run-gui.sh          (uses the bundled libxdo)
  It still needs gtk3 + the appindicator tray library from your distro:
       sudo apt install libgtk-3-0 libayatana-appindicator3-1
  (Or install libxdo3 too and just run ./mouse-rs directly.)

Move the mouse to the configured edge to cross to the other screen. Press the
return hotkey (default Scroll Lock, or the 'return_hotkey' combo) to come back.
EOF

tar -C "$STAGE" -czf "$DIST/mouse-rs-$VER-linux-x86_64.tar.gz" "mouse-rs-$VER-linux-x86_64"

# ---------------- Windows zip ----------------
echo "==> staging Windows zip"
WX="$STAGE/mouse-rs-$VER-windows-x86_64"
mkdir -p "$WX"
cp "$STAGE/mouse-rs.exe" "$WX/mouse-rs.exe"
cp config.example.json "$WX/config.example.json"

cat > "$WX/README.txt" <<EOF
mouse-rs v$VER — Windows (x86_64)

Share one keyboard/mouse between this machine and another over the LAN.
mouse-rs.exe is self-contained (only standard Win32 DLLs).

RUN
  Double-click mouse-rs.exe to open the control panel (edit config, Start/Stop).
  Or from a terminal (PowerShell / cmd):
       mouse-rs.exe server      (this machine has the keyboard/mouse)
       mouse-rs.exe client      (this machine receives input)

CONFIG  %USERPROFILE%\\.mouse_share\\config.json
  Copy config.example.json there and edit it. Set a long 'shared_secret' that is
  IDENTICAL on both machines; on the client set 'server_host' to the server's LAN IP.
  'return_hotkey' (e.g. ctrl+win+alt+esc) hands control back; it is matched on the
  SERVER, so set it in the server's config.

NOTES
  - SmartScreen may warn on first run (unsigned binary): More info -> Run anyway.
  - The client auto-reconnects if the server restarts ('reconnect': true).
EOF

if command -v zip >/dev/null 2>&1; then
  ( cd "$STAGE" && zip -qr "$DIST/mouse-rs-$VER-windows-x86_64.zip" "mouse-rs-$VER-windows-x86_64" )
else
  ( cd "$STAGE" && python3 -c "import shutil,sys; shutil.make_archive(sys.argv[1],'zip','.',sys.argv[2])" \
      "$DIST/mouse-rs-$VER-windows-x86_64" "mouse-rs-$VER-windows-x86_64" )
fi

# ---------------- checksums ----------------
( cd "$DIST" && sha256sum ./*.zip ./*.tar.gz > SHA256SUMS )

echo "==> done. artifacts in dist/:"
ls -lh "$DIST"
cat "$DIST/SHA256SUMS"
