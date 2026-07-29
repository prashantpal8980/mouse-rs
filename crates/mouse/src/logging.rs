//! Log destinations for the two ways this binary starts.
//!
//! `server`/`client` run in a terminal, so they keep the plain coloured stdout logs.
//!
//! A **GUI launch has no terminal to log to** — on Windows especially, `main` calls
//! `FreeConsole()` before tracing is initialised, so anything written to stdout goes
//! nowhere and a user reporting "it stopped working" has nothing to send. So the panel
//! logs to a **file next to the config**, `~/.mouse_share/mouse-rs.log`
//! (`%USERPROFILE%\.mouse_share\mouse-rs.log`), and the panel shows that path.
//!
//! The file is size-capped and keeps exactly one previous generation, because the
//! Windows capture path still emits a once-per-second diagnostic line while control is on
//! the peer — left unbounded that is a few MB a day in someone's profile directory.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// Rotate once the live file passes this, keeping one previous generation — so the logs
/// cost at most ~4 MB on disk while still covering the run before the one that broke.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Default filter when `RUST_LOG` is unset: our own crates at info, the graphics and
/// windowing stack at warn.
///
/// Without this, a single GUI launch writes ~4.5 KB of pretty-printed `WindowAttributes`,
/// adapter enumerations and surface-format lists before the app does anything. That buries
/// the two lines that actually matter to a user reporting a fault ("starting mouse-rs …",
/// "connected to server …"), which defeats the point of keeping a log at all. Adapter and
/// driver details still surface on failure, because those crates log warnings and errors.
const DEFAULT_FILTER: &str = "info,\
     wgpu_core=warn,wgpu_hal=warn,iced_wgpu=warn,iced_winit=warn,iced_graphics=warn,\
     winit=warn,naga=warn,cosmic_text=warn,zbus=warn,calloop=warn";

/// Where the GUI writes its log: beside `config.json`, so there is one folder to look in.
pub fn log_path() -> PathBuf {
    mouse_core::Config::default_path()
        .parent()
        .map(|d| d.join("mouse-rs.log"))
        .unwrap_or_else(|| PathBuf::from("mouse-rs.log"))
}

/// The previous generation, written when the live file is rotated out.
fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

struct Inner {
    file: File,
    /// Bytes in the live file. Tracked rather than `stat`-ed so an every-line size check
    /// costs nothing.
    written: u64,
    path: PathBuf,
}

impl Inner {
    /// Rename the live file aside and start a new one. A failure here must not lose the
    /// log line that triggered it, so the old handle is kept if the rename fails.
    fn rotate(&mut self) -> io::Result<()> {
        let rotated = rotated_path(&self.path);
        let _ = std::fs::remove_file(&rotated);
        std::fs::rename(&self.path, &rotated)?;
        self.file = open_log(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

/// Writes tracing output to the rotating file, and mirrors it to stdout when there might
/// be a terminal attached.
pub struct FileLogger {
    inner: Arc<Mutex<Inner>>,
    /// Mirror to stdout as well. On Windows after `FreeConsole` those writes fail; the
    /// error is ignored, which is why this can stay unconditional on the GUI path.
    tee_stdout: bool,
}

pub struct Handle<'a> {
    logger: &'a FileLogger,
}

impl Write for Handle<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.logger.tee_stdout {
            // Best-effort: no console (Windows GUI) or a closed pipe must not break logging.
            let _ = io::stdout().write_all(buf);
        }
        let Ok(mut inner) = self.logger.inner.lock() else {
            // A poisoned lock means another thread panicked mid-write. Dropping the line
            // is better than propagating a panic out of the logger.
            return Ok(buf.len());
        };
        if inner.written + buf.len() as u64 > MAX_BYTES {
            let _ = inner.rotate();
        }
        inner.file.write_all(buf)?;
        inner.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Ok(mut inner) = self.logger.inner.lock() {
            inner.file.flush()?;
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for FileLogger {
    type Writer = Handle<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        Handle { logger: self }
    }
}

/// Open the log for append, owner-only on Unix — it records hostnames, addresses and the
/// config path, which other local users have no business reading.
fn open_log(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

impl FileLogger {
    fn open(path: &Path, tee_stdout: bool) -> io::Result<Self> {
        let file = open_log(path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(FileLogger {
            inner: Arc::new(Mutex::new(Inner {
                file,
                written,
                path: path.to_path_buf(),
            })),
            tee_stdout,
        })
    }
}

/// Initialise tracing for this process.
///
/// `gui` selects the file destination described in the module docs; headless runs keep the
/// existing coloured stdout logs. Returns the log path when one is in use, so the panel can
/// show the user where to look. Falls back to stdout if the file can't be opened — a
/// read-only home directory should degrade the logs, not stop the app from starting.
pub fn init(gui: bool) -> Option<PathBuf> {
    // `RUST_LOG` still wins, so a verbose graphics trace is one env var away.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| DEFAULT_FILTER.into());

    if !gui {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return None;
    }

    let path = log_path();
    match FileLogger::open(&path, true) {
        Ok(logger) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                // No colour: this is a file first, and escape codes make it unreadable in
                // Notepad — which is what a Windows user will open it with.
                .with_ansi(false)
                .with_writer(logger)
                .init();
            tracing::info!("logging to {}", path.display());
            Some(path)
        }
        Err(e) => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            tracing::warn!("could not open {} ({e}); logging to stdout only", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mouse-rs-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The Windows capture path logs once a second while suppressing, so an uncapped file
    /// would grow by megabytes a day inside the user's profile. Rotation must bound it
    /// *and* keep the previous generation, since the run before the breakage is usually
    /// the interesting one.
    #[test]
    fn rotates_at_the_cap_and_keeps_one_previous_generation() {
        let dir = temp_dir("rot");
        let path = dir.join("mouse-rs.log");
        let logger = FileLogger::open(&path, false).unwrap();

        // Write past the cap in chunks, as line-by-line logging would.
        let chunk = vec![b'x'; 64 * 1024];
        let mut w = logger.make_writer();
        for _ in 0..40 {
            w.write_all(&chunk).unwrap();
        }
        w.flush().unwrap();

        let live = std::fs::metadata(&path).unwrap().len();
        assert!(live <= MAX_BYTES, "live log exceeded the cap: {live} bytes");

        let rotated = rotated_path(&path);
        assert!(rotated.exists(), "previous generation was not kept");
        let total = live + std::fs::metadata(&rotated).unwrap().len();
        assert!(
            total <= MAX_BYTES * 2 + chunk.len() as u64,
            "logs grew past two generations: {total} bytes"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Appending rather than truncating matters: the panel is opened and closed often, and
    /// truncating on each launch would throw away the session that just misbehaved.
    #[test]
    fn reopening_appends_and_creates_the_directory() {
        let dir = temp_dir("append");
        let path = dir.join("nested").join("mouse-rs.log");

        {
            let l = FileLogger::open(&path, false).unwrap();
            l.make_writer().write_all(b"first\n").unwrap();
        }
        {
            let l = FileLogger::open(&path, false).unwrap();
            l.make_writer().write_all(b"second\n").unwrap();
        }

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "first\nsecond\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The log sits beside config.json so there is a single folder to ask a user for.
    #[test]
    fn log_lives_next_to_the_config() {
        let log = log_path();
        let cfg = mouse_core::Config::default_path();
        assert_eq!(log.parent(), cfg.parent());
        assert_eq!(log.file_name().unwrap(), "mouse-rs.log");
    }

    /// A log records addresses and paths; other local users shouldn't be able to read it.
    #[cfg(unix)]
    #[test]
    fn log_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perm");
        let path = dir.join("mouse-rs.log");
        let l = FileLogger::open(&path, false).unwrap();
        l.make_writer().write_all(b"x\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "log must be 0600, got {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
