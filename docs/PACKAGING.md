# Packaging mouse-rs

`scripts/package.sh` builds release artifacts for both platforms and assembles them
into `dist/`. Run it from anywhere (paths resolve to the repo root):

```bash
bash scripts/package.sh
```

Output:

| Artifact | Contents |
|---|---|
| `dist/mouse-rs-<ver>-windows-x86_64.zip` | `mouse-rs.exe` (GUI; also runs `server`/`client`), `config.example.json`, `README.txt` |
| `dist/mouse-rs-<ver>-linux-x86_64.tar.gz` | `mouse-rs` (GUI), `mouse-rs-headless` (slim), bundled `lib/libxdo.so.3`, `run-gui.sh`, `config.example.json`, `README.txt` |
| `dist/SHA256SUMS` | checksums of both archives |

## Build variants & runtime deps

- **Slim headless** (`cargo build --release -p mouse --no-default-features`) — no GUI,
  no tray. **Fully self-contained**: Linux ≈ 3.3 MB (only libc + libX11/libXtst, which
  any X11 session has); Windows ≈ 2.9 MB. This is the recommended build for a dedicated
  server/client. The `gui` cargo feature is default-on, so `--no-default-features` opts out.
- **GUI** (default features) — adds the iced control panel + the system tray.
  Linux ≈ 14.2 MB, Windows ≈ 10.5 MB. (Both grew when the GUI moved from egui to iced,
  which brings the wgpu renderer with a tiny-skia software fallback.)
  - **Windows GUI exe is self-contained** (only standard Win32 DLLs).
  - **Linux GUI** links `libxdo.so.3` (via the tray's libappindicator) plus needs gtk3 +
    libayatana-appindicator at runtime. The tarball **bundles `libxdo.so.3`** and ships
    `run-gui.sh`, which puts it on `LD_LIBRARY_PATH` — so users only need the distro's
    gtk3 + appindicator (`sudo apt install libgtk-3-0 libayatana-appindicator3-1`), not
    `libxdo3`.

## Build-time note (Linux GUI, libxdo)

Linking the Linux GUI needs `libxdo.so` at build time. On a clean box: `sudo apt install
libxdo-dev`. The script doesn't require it — if only the runtime `libxdo.so.3` is present,
it auto-creates a `.xdo-shim/libxdo.so` symlink and passes `-L` via `RUSTFLAGS` so the
build still links (no root needed). The headless build never touches libxdo.

## Cross-compile prerequisites

The Windows target uses `x86_64-pc-windows-gnu` (mingw-w64 + a libx11 import lib already
present on the Kali box). Install the target with `rustup target add x86_64-pc-windows-gnu`.
