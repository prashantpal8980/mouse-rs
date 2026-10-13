//! Reading a file that is still arriving from the peer, for whatever is pasting it.
//!
//! Shared by the Linux streaming folder (a FUSE read) and the Windows virtual clipboard
//! file (an `IStream::Read`), so both behave the same: bytes are handed over as they
//! arrive, a stalled transfer is nudged and eventually given up on, and the last bytes of
//! a file are only handed over once it has been verified.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cache::BlobCache;
use crate::files::PasteSignal;
use crate::pipe::StreamPipe;

/// How often a waiting read looks for new bytes.
const POLL: Duration = Duration::from_millis(50);
/// While waiting, re-ask for the file this often, so a transfer that dropped (a reconnect,
/// a cancelled fetch) is resumed rather than waited on.
const NUDGE: Duration = Duration::from_secs(10);

/// One file a paste can read.
#[derive(Clone)]
pub struct RemoteSource {
    pub hash: String,
    pub size: u64,
    /// Raised to start (or restart) a fetch into the cache.
    pub signal: Arc<PasteSignal>,
    /// Large files stream through memory instead of the cache; see [`crate::pipe`].
    pub pipe: Option<Arc<StreamPipe>>,
}

/// The outcome of one attempt to read.
#[derive(Debug, PartialEq, Eq)]
pub enum Got {
    Data(Vec<u8>),
    /// Not there yet, and still worth waiting for. Ask again.
    Wait,
    /// The transfer failed, stalled past the limit, or the file did not verify.
    Fail,
}

/// Per-open reading state: how long since the file last grew, for the stall limit.
pub struct Reader {
    src: RemoteSource,
    grew_at: Instant,
    nudged_at: Instant,
    seen: u64,
}

impl Reader {
    pub fn new(src: RemoteSource) -> Reader {
        Reader { src, grew_at: Instant::now(), nudged_at: Instant::now(), seen: u64::MAX }
    }

    pub fn source(&self) -> &RemoteSource {
        &self.src
    }

    /// Bytes `offset..offset+len`, waiting at most `slice` for them. Fails once nothing has
    /// arrived for `stall`.
    pub fn read(&mut self, cache: &BlobCache, offset: u64, len: u64, stall: Duration, slice: Duration) -> Got {
        if offset >= self.src.size || len == 0 {
            return Got::Data(Vec::new());
        }
        if let Some(pipe) = &self.src.pipe {
            return pipe.read_slice(offset, len, stall, slice);
        }
        let end = (offset + len).min(self.src.size);
        // The tail is only served once the whole file is verified, so a corrupt transfer
        // fails the copy instead of completing it.
        let tail = end == self.src.size;
        let started = Instant::now();
        loop {
            if cache.contains(&self.src.hash) {
                return match read_range(&cache.blob_path(&self.src.hash), offset, end) {
                    Some(b) => Got::Data(b),
                    None => Got::Fail,
                };
            }
            let have = cache.partial_len(&self.src.hash);
            if !tail && have >= end {
                // The partial can be renamed into place between the check and the read;
                // the next pass then reads the finished blob instead.
                if let Some(bytes) = read_range(&cache.part_path(&self.src.hash), offset, end) {
                    return Got::Data(bytes);
                }
                continue;
            }
            if self.src.signal.failed.load(Ordering::SeqCst) {
                tracing::warn!(bytes = self.src.size, "the transfer behind a paste gave up; failing the paste");
                return Got::Fail;
            }
            if have != self.seen {
                self.seen = have;
                self.grew_at = Instant::now();
            }
            if self.grew_at.elapsed() > stall {
                tracing::warn!(have, size = self.src.size, "no progress for {stall:?}; failing the paste");
                return Got::Fail;
            }
            if self.nudged_at.elapsed() > NUDGE && self.grew_at.elapsed() > NUDGE {
                self.src.signal.wanted.store(true, Ordering::SeqCst);
                self.nudged_at = Instant::now();
            }
            if started.elapsed() >= slice {
                return Got::Wait;
            }
            std::thread::sleep(POLL.min(slice));
        }
    }
}

/// Read `offset..end` of a file, or `None`.
fn read_range(path: &std::path::Path, offset: u64, end: u64) -> Option<Vec<u8>> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; (end - offset) as usize];
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt as _;
        f.read_exact_at(&mut buf, offset).ok()?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt as _;
        let mut done = 0usize;
        while done < buf.len() {
            match f.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) | Err(_) => return None,
                Ok(n) => done += n,
            }
        }
    }
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache;

    #[test]
    fn a_read_waits_for_bytes_and_withholds_the_tail_until_verified() {
        let dir = std::env::temp_dir().join(format!("mouse-rs-rr-{}", std::process::id()));
        let cache = Arc::new(BlobCache::new(dir.clone()).unwrap());
        let body = b"0123456789".to_vec();
        let hash = cache::hash_bytes(&body);
        let src = RemoteSource { hash: hash.clone(), size: 10, signal: Arc::new(PasteSignal::default()), pipe: None };
        let long = Duration::from_secs(30);

        {
            use std::io::Write as _;
            let mut p = cache.open_partial(&hash).unwrap();
            p.write_all(&body[..6]).unwrap();
        }
        let mut r = Reader::new(src.clone());
        assert_eq!(r.read(&cache, 0, 4, long, long), Got::Data(b"0123".to_vec()));
        // The tail is not there yet: a short slice says "wait", not "fail".
        assert_eq!(r.read(&cache, 6, 100, long, Duration::from_millis(60)), Got::Wait);

        let c2 = cache.clone();
        let h2 = hash.clone();
        let rest = body[6..].to_vec();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            use std::io::Write as _;
            let mut p = c2.open_partial(&h2).unwrap();
            p.write_all(&rest).unwrap();
            drop(p);
            c2.finish(&h2).unwrap();
        });
        assert_eq!(r.read(&cache, 6, 100, long, long), Got::Data(b"6789".to_vec()));
        t.join().unwrap();

        // A transfer marked failed ends a waiting read.
        let missing = RemoteSource {
            hash: cache::hash_bytes(b"never"),
            size: 5,
            signal: Arc::new(PasteSignal::default()),
            pipe: None,
        };
        missing.signal.failed.store(true, Ordering::SeqCst);
        assert_eq!(Reader::new(missing).read(&cache, 0, 5, long, long), Got::Fail);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
