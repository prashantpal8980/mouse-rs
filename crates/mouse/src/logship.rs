//! Ship this machine's log to a collection point, so a two-machine fault can be
//! diagnosed from one place.
//!
//! ## Why this exists
//!
//! A bug in this program is almost never visible from one side. "The file never
//! arrived" is a sentence about two machines: one decided what to advertise, the other
//! decided whether to ask for it, and the answer lives in whichever log nobody is
//! looking at. Asking a human to fetch the other log turns a five-second question into
//! a five-minute round trip, and in practice it means the question does not get asked
//! and the diagnosis becomes a guess.
//!
//! So each machine pushes its own log, unprompted, and both end up side by side.
//!
//! ## Why raw TCP rather than an HTTP crate
//!
//! This is one `PUT` to a fixed host on a direct cable during development. A full HTTP
//! client would add a dependency tree — and a TLS stack — to send about forty bytes of
//! header. The request is written by hand instead.
//!
//! ## Scope
//!
//! Plain HTTP, no authentication, and off unless a URL is configured. It is a debugging
//! aid pointed at a machine the owner controls, not a telemetry channel: nothing is sent
//! anywhere unless the owner names the destination themselves.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Most log we will send in one go. A long-running session's log can reach tens of
/// megabytes; the tail is where a fault being investigated now actually is, and sending
/// the rest would cost seconds for no benefit.
const MAX_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// How long to wait on the collector before giving up. It is on the same cable, so a
/// slow answer means it is gone rather than busy — and a hung upload must never be able
/// to delay shutdown.
const NET_TIMEOUT: Duration = Duration::from_secs(5);

/// Consecutive failed uploads before saying so out loud.
///
/// Not the first: a collector that is briefly down, or a machine that boots before it,
/// is ordinary and must not cry wolf. Three in a row at the default interval is a minute
/// and a half of the logs going nowhere, which is worth knowing about.
const FAILURES_BEFORE_WARNING: u32 = 3;
/// How often to repeat the warning after that, in cycles.
const WARN_EVERY: u32 = 20;

pub struct LogShipper {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Kept so shutdown can send one final copy — the most valuable one, since it holds
    /// whatever happened last.
    target: Target,
}

#[derive(Clone)]
struct Target {
    host: String,
    port: u16,
    path: String,
    file: PathBuf,
    /// Shown to the collector, which requires it from anywhere but the cable.
    token: String,
}

impl LogShipper {
    /// Start shipping `log_path` to `base_url` every `secs` seconds.
    ///
    /// Returns `None` when shipping is switched off or the URL cannot be understood —
    /// a malformed URL is reported once here rather than failing silently every cycle.
    pub fn spawn(
        base_url: &str,
        log_path: &Path,
        machine: &str,
        secs: u32,
        token: String,
    ) -> Option<LogShipper> {
        let base = base_url.trim();
        if base.is_empty() {
            return None;
        }
        let (host, port, prefix) = parse_base(base)?;
        // Named by machine so the two sides land beside each other rather than
        // overwriting one another.
        let safe: String = machine
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let path = format!("{}/{}-mouse-rs.log", prefix.trim_end_matches('/'), safe);
        let target = Target { host, port, path, file: log_path.to_path_buf(), token };

        tracing::info!(
            url = %format!("http://{}:{}{}", target.host, target.port, target.path),
            every_secs = secs,
            "shipping this machine's log for diagnosis"
        );

        let stop = Arc::new(AtomicBool::new(false));
        let t = target.clone();
        let s = stop.clone();
        let every = Duration::from_secs(u64::from(secs.max(5)));
        let thread = std::thread::Builder::new()
            .name("log-shipper".into())
            .spawn(move || {
                // A first upload straight away, so a machine that starts and immediately
                // misbehaves is still represented.
                let _ = put(&t);
                // Consecutive failures. Silence here once cost a full day of logs from
                // both machines — the collector had died and nothing said so, on either
                // side, so the evidence simply stopped existing without anyone noticing.
                let mut failures: u32 = 0;
                loop {
                    // Slept in short slices so stopping is prompt even with a long
                    // interval — shutdown must not wait out a full cycle.
                    let mut waited = Duration::ZERO;
                    while waited < every {
                        if s.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                        waited += Duration::from_millis(250);
                    }
                    match put(&t) {
                        Ok(()) => {
                            if failures >= FAILURES_BEFORE_WARNING {
                                tracing::warn!(
                                    after_failures = failures,
                                    "log upload is working again"
                                );
                            }
                            failures = 0;
                        }
                        Err(e) => {
                            failures = failures.saturating_add(1);
                            // Warn on the threshold and then at intervals — loud enough
                            // to be noticed, quiet enough not to fill the log it ships.
                            if failures == FAILURES_BEFORE_WARNING
                                || (failures > FAILURES_BEFORE_WARNING
                                    && failures % WARN_EVERY == 0)
                            {
                                tracing::warn!(
                                    consecutive_failures = failures,
                                    url = %format!("http://{}:{}{}", t.host, t.port, t.path),
                                    "LOG UPLOAD IS FAILING — this machine's logs are not \
                                     reaching the collector, so a fault here will leave no \
                                     evidence anywhere else. Is the collector running? \
                                     Last error: {e}"
                                );
                            } else {
                                tracing::debug!("log upload failed: {e}");
                            }
                        }
                    }
                }
            })
            .ok()?;

        Some(LogShipper { stop, thread: Some(thread), target })
    }

}

impl Drop for LogShipper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // The final state is the interesting one, so it is worth one last attempt even
        // on the way out.
        let _ = put(&self.target);
    }
}

/// `http://host:port/prefix` → `(host, port, prefix)`. Only plain HTTP: this points at
/// a machine on the owner's own cable, and adding TLS here would mean a certificate
/// story for a debugging aid.
fn parse_base(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port, path.to_string()))
}

/// Upload to the configured collector; when that is unreachable (the cable is out), to the
/// same port on the linked peer, which is where the collector is when testing on Wi-Fi.
fn put(t: &Target) -> std::io::Result<()> {
    let first = put_to(t, &t.host);
    let Err(e) = first else { return Ok(()) };
    match mouse_core::net::last_peer_ip() {
        Some(ip @ std::net::IpAddr::V4(_)) if ip.to_string() != t.host => put_to(t, &ip.to_string()),
        _ => Err(e),
    }
}

fn put_to(t: &Target, host: &str) -> std::io::Result<()> {
    let body = tail_of(&t.file, MAX_TAIL_BYTES)?;

    let addr = format!("{}:{}", host, t.port);
    let mut sock = None;
    // Resolve and connect with a timeout: a collector that has gone away must fail
    // fast rather than park this thread.
    for a in std::net::ToSocketAddrs::to_socket_addrs(&addr)? {
        if let Ok(s) = TcpStream::connect_timeout(&a, NET_TIMEOUT) {
            sock = Some(s);
            break;
        }
    }
    let mut sock = sock.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotConnected, "no route to the log collector")
    })?;
    sock.set_write_timeout(Some(NET_TIMEOUT))?;
    sock.set_read_timeout(Some(NET_TIMEOUT))?;

    let head = format!(
        "PUT {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nX-Mouse-Token: {}\r\nConnection: close\r\n\r\n",
        t.path,
        host,
        body.len(),
        t.token
    );
    sock.write_all(head.as_bytes())?;
    sock.write_all(&body)?;
    sock.flush()?;

    // Read enough to learn the status. Not reading at all would leave the collector
    // writing into a closed socket and reporting an error for a successful upload.
    let mut buf = [0u8; 64];
    let n = sock.read(&mut buf).unwrap_or(0);
    // A collector that closes without answering has still taken the bytes, so silence
    // is not treated as failure — only an explicit non-2xx is.
    let status = String::from_utf8_lossy(&buf[..n]);
    if n == 0 || status.contains(" 2") {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "collector said: {}",
            status.lines().next().unwrap_or("(nothing)")
        )))
    }
}

/// The last `max` bytes of a file, starting at a line boundary so the upload never
/// begins mid-record.
fn tail_of(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len <= max {
        let mut v = Vec::with_capacity(len as usize);
        f.read_to_end(&mut v)?;
        return Ok(v);
    }
    f.seek(SeekFrom::Start(len - max))?;
    let mut v = Vec::with_capacity(max as usize);
    f.read_to_end(&mut v)?;
    // Drop the partial first line.
    if let Some(i) = v.iter().position(|b| *b == b'\n') {
        v.drain(..=i);
    }
    let mut out = format!("=== tail of {} ({} bytes total) ===\n", path.display(), len).into_bytes();
    out.extend_from_slice(&v);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_url_splits_into_host_port_and_prefix() {
        assert_eq!(
            parse_base("http://192.168.137.2:8001/logs"),
            Some(("192.168.137.2".into(), 8001, "/logs".into()))
        );
        assert_eq!(
            parse_base("http://192.168.137.2:8001"),
            Some(("192.168.137.2".into(), 8001, "".into()))
        );
        assert_eq!(parse_base("http://box"), Some(("box".into(), 80, "".into())));
    }

    #[test]
    fn anything_that_is_not_plain_http_is_refused_rather_than_guessed_at() {
        assert_eq!(parse_base("https://example.com/logs"), None);
        assert_eq!(parse_base("192.168.137.2:8001"), None);
        assert_eq!(parse_base("http://:8001"), None);
    }

    #[test]
    fn a_long_log_is_tailed_at_a_line_boundary() {
        let dir = std::env::temp_dir().join("mouse-rs-logship-test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("big.log");
        let mut s = String::new();
        for i in 0..5000 {
            s.push_str(&format!("line {i} with some padding to make it long enough\n"));
        }
        std::fs::write(&p, &s).unwrap();

        let out = tail_of(&p, 1024).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("=== tail of"), "the tail says it is a tail");
        // Everything after the header must be whole lines.
        let body = text.split_once('\n').unwrap().1;
        assert!(body.starts_with("line "), "started mid-record: {:?}", &body[..20.min(body.len())]);
        assert!(text.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_short_log_is_sent_whole_and_unannotated() {
        let dir = std::env::temp_dir().join("mouse-rs-logship-small");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("small.log");
        std::fs::write(&p, b"one\ntwo\n").unwrap();
        assert_eq!(tail_of(&p, 1024).unwrap(), b"one\ntwo\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
