//! A large file passed straight from the network to a paste, through memory only.
//!
//! The first version of the streaming folder spooled every byte to the cache and served
//! the paste from that spool, so a 3 GB file briefly took 6 GB: the spool plus the copy
//! the paste was writing. The spool bought two things, resuming and verification, and
//! neither needs it:
//!
//! * **Resuming.** The paste reads in order, so the next byte it needs is always known. A
//!   dropped connection is resumed by asking the peer for the file from exactly that byte.
//! * **Verification.** A file the sender identified by length is checked by length,
//!   which this tracks directly. A file the sender hashed is hashed again here as the
//!   bytes pass through, and the paste's final read waits for that hash to match, so a
//!   damaged transfer fails the paste instead of completing it.
//!
//! So a pipe holds at most [`WINDOW`] bytes. If the paste falls behind and the window
//! fills, the stream is stopped and restarted from where the paste has got to once it
//! catches up: flow control by re-request, using the protocol's resume offset.

use std::collections::VecDeque;

use sha2::{Digest as _, Sha256};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Most bytes held in memory for one file.
pub const WINDOW: usize = 64 * 1024 * 1024;
/// Bytes kept behind the reader, so a small step back is still served.
const KEEP_BEHIND: u64 = 4 * 1024 * 1024;
/// A waiting read re-asks for the stream after this long without progress. That is what
/// resumes a transfer after the connection drops and comes back.
const NUDGE: Duration = Duration::from_secs(10);

/// What the clipboard thread should do for this pipe now.
#[derive(Debug, PartialEq, Eq)]
pub enum PipeCmd {
    Nothing,
    /// (Re)start the stream from this offset, cancelling any stream already running.
    Start(u64),
    /// Cancel the running stream: the window is full.
    Stop,
    /// The paste closed the file. `finished` says whether it had read it all; if not, the
    /// user cancelled, and whatever shows the transfer must stop saying it is running.
    Released { finished: bool },
}

#[derive(Debug)]
struct State {
    /// File offset of `buf[0]`.
    start: u64,
    buf: VecDeque<u8>,
    /// A stream from the peer is feeding this pipe.
    running: bool,
    want: Option<u64>,
    stop: bool,
    overfull: bool,
    /// The peer said it cannot send this file: fail the paste now, not after the wait.
    failed: bool,
    /// Every byte has been handed to a paste at least once.
    finished: bool,
    grew_at: Instant,
    asked_at: Instant,
    /// Running content hash, for a file the sender hashed. `None` for length-only files,
    /// or once the stream skipped bytes and can no longer be hashed in order.
    hasher: Option<Sha256>,
    /// Bytes fed to `hasher` so far, always a prefix of the file.
    hashed_to: u64,
    /// For a hashed file: whether the finished hash matched. `None` until it finishes.
    verified: Option<bool>,
}

impl State {
    fn have_end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }
    /// Forget everything and start over from `offset`.
    fn restart_at(&mut self, offset: u64) {
        self.buf.clear();
        self.start = offset;
        self.want = Some(offset);
        self.asked_at = Instant::now();
        self.grew_at = Instant::now();
    }
}

pub struct StreamPipe {
    size: u64,
    /// The content hash to check, for a file the sender hashed.
    expect: Option<String>,
    st: Mutex<State>,
    cv: Condvar,
}

impl StreamPipe {
    /// `expect` is the content hash to verify, or `None` for a file identified by length.
    pub fn new(size: u64, expect: Option<String>) -> StreamPipe {
        let hasher = expect.as_ref().map(|_| Sha256::new());
        StreamPipe {
            size,
            expect,
            st: Mutex::new(State {
                start: 0,
                buf: VecDeque::new(),
                running: false,
                want: None,
                stop: false,
                overfull: false,
                failed: false,
                finished: false,
                grew_at: Instant::now(),
                asked_at: Instant::now(),
                hasher,
                hashed_to: 0,
                verified: None,
            }),
            cv: Condvar::new(),
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    // ----- the clipboard thread's side -----

    /// The next thing to do for this pipe. Taking a command clears it.
    pub fn take_command(&self) -> PipeCmd {
        let Ok(mut st) = self.st.lock() else { return PipeCmd::Nothing };
        if st.stop {
            st.stop = false;
            st.want = None;
            st.running = false;
            return PipeCmd::Released { finished: st.finished };
        }
        if let Some(offset) = st.want.take() {
            st.running = true;
            st.overfull = false;
            return PipeCmd::Start(offset);
        }
        if st.overfull && st.running {
            st.overfull = false;
            st.running = false;
            return PipeCmd::Stop;
        }
        PipeCmd::Nothing
    }

    /// A stream begins at `offset`. Accepted only where the buffered bytes end, so the
    /// bytes can never land at the wrong place.
    pub fn begin(&self, offset: u64) -> bool {
        let Ok(st) = self.st.lock() else { return false };
        offset == st.have_end()
    }

    /// Bytes arrived. Returns false when the window is full: the caller stops the stream,
    /// and it is restarted from here once the paste has read some of it.
    pub fn push(&self, data: &[u8]) -> bool {
        let Ok(mut st) = self.st.lock() else { return false };
        let room = (self.size - st.have_end().min(self.size)) as usize;
        let at = st.have_end();
        let data = &data[..data.len().min(room)];
        st.buf.extend(data);
        self.hash_through(&mut st, at, data);
        st.grew_at = Instant::now();
        let ok = st.buf.len() < WINDOW;
        if !ok {
            st.overfull = true;
        }
        self.cv.notify_all();
        ok
    }

    /// Feed the in-order part of `data` (which starts at file offset `at`) to the hash.
    fn hash_through(&self, st: &mut State, at: u64, data: &[u8]) {
        let Some(expect) = &self.expect else { return };
        let end = at + data.len() as u64;
        if st.hasher.is_some() && end > st.hashed_to {
            if at > st.hashed_to {
                // Bytes were skipped (a jump ahead), so the hash can no longer be computed.
                st.hasher = None;
                st.verified = Some(false);
                tracing::warn!(at, hashed_to = st.hashed_to, "streamed file skipped ahead; it cannot be verified, so the paste will fail");
                return;
            }
            let from = (st.hashed_to - at) as usize;
            if let Some(h) = st.hasher.as_mut() {
                h.update(&data[from..]);
            }
            st.hashed_to = end;
        }
        if st.hashed_to >= self.size && st.verified.is_none() {
            if let Some(h) = st.hasher.take() {
                let ok = crate::cache::hex(&h.finalize()) == *expect;
                st.verified = Some(ok);
                if ok {
                    tracing::info!(bytes = self.size, "streamed file verified by content hash");
                } else {
                    tracing::warn!(bytes = self.size, "streamed file does NOT match its hash; failing the paste");
                }
            }
        }
    }

    /// The peer cannot send this file. A waiting paste fails at once, with the reason in
    /// the log, rather than waiting out the whole "dropped connection" limit.
    pub fn fail(&self) {
        if let Ok(mut st) = self.st.lock() {
            st.failed = true;
            st.running = false;
            st.want = None;
        }
        self.cv.notify_all();
    }

    /// A new paste opened the file: forget an earlier failure or release, so it tries
    /// again from scratch.
    pub fn reopen(&self) {
        if let Ok(mut st) = self.st.lock() {
            st.failed = false;
            st.stop = false;
        }
    }

    /// The stream ended or broke. A reader still waiting asks for it again.
    pub fn interrupted(&self) {
        if let Ok(mut st) = self.st.lock() {
            st.running = false;
        }
        self.cv.notify_all();
    }

    /// Every byte has arrived.
    pub fn complete(&self) -> bool {
        self.st.lock().map(|st| st.have_end() >= self.size).unwrap_or(false)
    }

    // ----- the paste's side -----

    /// Bytes `offset..offset+len`, waiting for them to arrive. `None` after `stall`
    /// without any progress, which ends the paste with an I/O error rather than a frozen
    /// progress dialog.
    pub fn read(&self, offset: u64, len: u64, stall: Duration) -> Option<Vec<u8>> {
        loop {
            match self.read_slice(offset, len, stall, stall) {
                crate::remote_read::Got::Data(b) => return Some(b),
                crate::remote_read::Got::Fail => return None,
                crate::remote_read::Got::Wait => continue,
            }
        }
    }

    /// As [`StreamPipe::read`], but waits at most `slice` before answering `Wait`, so a
    /// caller that must keep doing other things (a Windows thread pumping messages) can.
    pub fn read_slice(&self, offset: u64, len: u64, stall: Duration, slice: Duration) -> crate::remote_read::Got {
        use crate::remote_read::Got;
        if offset >= self.size || len == 0 {
            return Got::Data(Vec::new());
        }
        let started = Instant::now();
        let end = (offset + len).min(self.size);
        let Ok(mut st) = self.st.lock() else { return Got::Fail };
        loop {
            if st.failed {
                return Got::Fail;
            }
            // A step back past what is kept, or a jump far ahead: start over from there.
            if offset < st.start || offset > st.have_end() + WINDOW as u64 / 2 {
                st.restart_at(offset);
            }
            let mut tail_pending = false;
            if st.have_end() >= end {
                // The last bytes of a hashed file are only handed over once it verified.
                if end == self.size && self.expect.is_some() && st.verified != Some(true) {
                    if st.verified == Some(false) {
                        return Got::Fail;
                    }
                    tail_pending = true;
                } else {
                    let from = (offset - st.start) as usize;
                    let to = (end - st.start) as usize;
                    let out: Vec<u8> = st.buf.range(from..to).copied().collect();
                    if end == self.size {
                        st.finished = true;
                    }
                    // Drop what the paste has finished with, keeping a little behind it.
                    let keep_from = end.saturating_sub(KEEP_BEHIND).max(st.start);
                    let drop = (keep_from - st.start) as usize;
                    st.buf.drain(..drop);
                    st.start = keep_from;
                    return Got::Data(out);
                }
            }
            if !tail_pending {
                // Missing bytes and nothing on its way, or no progress for a while (a
                // dropped connection, a stream stopped for flow control): ask from here.
                let quiet = st.grew_at.elapsed() > NUDGE && st.asked_at.elapsed() > NUDGE;
                if st.want.is_none() && (!st.running || quiet) {
                    let at = st.have_end();
                    st.want = Some(at);
                    // A reader asking supersedes an earlier release: without this, a
                    // pending stop swallowed the request and the paste sat idle until the
                    // next nudge, ten seconds later.
                    st.stop = false;
                    st.asked_at = Instant::now();
                }
            }
            if st.grew_at.elapsed() > stall {
                return Got::Fail;
            }
            let left = slice.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Got::Wait;
            }
            st = match self.cv.wait_timeout(st, left.min(Duration::from_millis(100))) {
                Ok((g, _)) => g,
                Err(_) => return Got::Fail,
            };
        }
    }

    /// The paste closed the file. Stop streaming and free the memory.
    pub fn release(&self) {
        if let Ok(mut st) = self.st.lock() {
            st.buf.clear();
            st.buf.shrink_to_fit();
            st.start = 0;
            st.want = None;
            st.stop = true;
        }
        self.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_read_asks_for_the_stream_then_gets_the_bytes() {
        let p = Arc::new(StreamPipe::new(10, None));
        let p2 = p.clone();
        let feeder = std::thread::spawn(move || {
            // The clipboard thread's loop, reduced: wait to be asked, then deliver.
            loop {
                if let PipeCmd::Start(off) = p2.take_command() {
                    assert_eq!(off, 0);
                    assert!(p2.begin(0));
                    p2.push(b"01234");
                    std::thread::sleep(Duration::from_millis(50));
                    p2.push(b"56789");
                    p2.interrupted();
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        assert_eq!(p.read(0, 4, Duration::from_secs(5)).unwrap(), b"0123");
        assert_eq!(p.read(4, 100, Duration::from_secs(5)).unwrap(), b"456789");
        assert!(p.complete());
        feeder.join().unwrap();
    }

    #[test]
    fn a_broken_stream_resumes_from_the_byte_the_paste_reached() {
        let p = StreamPipe::new(8, None);
        assert_eq!(p.take_command(), PipeCmd::Nothing, "nothing is fetched until read");
        {
            let mut st = p.st.lock().unwrap();
            st.want = Some(0);
        }
        assert_eq!(p.take_command(), PipeCmd::Start(0));
        p.push(b"abc");
        p.interrupted(); // the connection dropped
        // The paste reads what arrived; the next read asks again from byte 3.
        assert_eq!(p.read(0, 3, Duration::from_secs(1)).unwrap(), b"abc");
        let p = Arc::new(p);
        let p2 = p.clone();
        let t = std::thread::spawn(move || p2.read(3, 5, Duration::from_secs(5)));
        let mut asked = None;
        for _ in 0..100 {
            if let PipeCmd::Start(off) = p.take_command() {
                asked = Some(off);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(asked, Some(3), "resumed exactly where the paste got to");
        assert!(p.begin(3));
        assert!(!p.begin(0), "a stream from the wrong offset is refused");
        p.push(b"defgh");
        assert_eq!(t.join().unwrap().unwrap(), b"defgh");
    }

    #[test]
    fn a_full_window_stops_the_stream_and_memory_stays_bounded() {
        let p = StreamPipe::new((WINDOW * 2) as u64, None);
        p.st.lock().unwrap().want = Some(0);
        assert_eq!(p.take_command(), PipeCmd::Start(0));
        let chunk = vec![7u8; 1024 * 1024];
        let mut accepted = 0;
        while p.push(&chunk) {
            accepted += 1;
        }
        assert!(accepted < WINDOW / chunk.len());
        assert_eq!(p.take_command(), PipeCmd::Stop, "the sender is told to pause");
        // The paste reads; once it needs more than is buffered, the stream restarts there.
        let got = p.read(0, WINDOW as u64, Duration::from_millis(1));
        assert!(got.is_some());
        assert!(p.st.lock().unwrap().buf.len() <= KEEP_BEHIND as usize + chunk.len());
    }

    #[test]
    fn closing_the_paste_frees_the_memory_and_stops_the_stream() {
        let p = StreamPipe::new(100, None);
        p.st.lock().unwrap().want = Some(0);
        assert_eq!(p.take_command(), PipeCmd::Start(0));
        p.push(&[1u8; 50]);
        p.release();
        assert_eq!(p.take_command(), PipeCmd::Released { finished: false }, "a cancel, reported as one");
        assert!(p.st.lock().unwrap().buf.is_empty());
    }

    #[test]
    fn a_hashed_file_is_verified_and_a_damaged_one_fails_the_paste() {
        let body = b"0123456789".to_vec();
        let good = StreamPipe::new(10, Some(crate::cache::hash_bytes(&body)));
        good.st.lock().unwrap().want = Some(0);
        let _ = good.take_command();
        good.push(&body[..4]);
        good.push(&body[4..]);
        assert_eq!(good.read(0, 10, Duration::from_secs(1)).unwrap(), body);

        let bad = StreamPipe::new(10, Some(crate::cache::hash_bytes(&body)));
        bad.st.lock().unwrap().want = Some(0);
        let _ = bad.take_command();
        bad.push(b"0123456780"); // one byte wrong
        assert!(bad.read(0, 4, Duration::from_secs(1)).is_some(), "early bytes still flow");
        assert!(bad.read(4, 6, Duration::from_secs(1)).is_none(), "the end is refused");
    }

    #[test]
    fn a_peer_that_cannot_send_fails_the_paste_at_once_and_a_new_paste_retries() {
        let p = StreamPipe::new(10, None);
        p.fail();
        let t = Instant::now();
        assert!(p.read(0, 5, Duration::from_secs(30)).is_none());
        assert!(t.elapsed() < Duration::from_secs(1), "no waiting out the limit");
        p.reopen();
        assert_eq!(p.read_slice(0, 5, Duration::from_secs(30), Duration::from_millis(20)), crate::remote_read::Got::Wait);
        assert_eq!(p.take_command(), PipeCmd::Start(0), "the new paste asks again");
    }

    #[test]
    fn a_new_paste_after_a_release_asks_at_once() {
        let p = StreamPipe::new(10, None);
        p.release(); // the previous paste closed the file
        let _ = p.read_slice(0, 5, Duration::from_secs(30), Duration::from_millis(20));
        assert_eq!(p.take_command(), PipeCmd::Start(0), "not swallowed by the earlier stop");
    }

    #[test]
    fn a_completed_paste_is_not_reported_as_cancelled() {
        let p = StreamPipe::new(4, None);
        p.st.lock().unwrap().want = Some(0);
        let _ = p.take_command();
        p.push(b"abcd");
        assert_eq!(p.read(0, 4, Duration::from_secs(1)).unwrap(), b"abcd");
        p.release();
        assert_eq!(p.take_command(), PipeCmd::Released { finished: true });

        let q = StreamPipe::new(4, None);
        q.st.lock().unwrap().want = Some(0);
        let _ = q.take_command();
        q.push(b"ab");
        assert_eq!(q.read(0, 2, Duration::from_secs(1)).unwrap(), b"ab");
        q.release(); // cancelled in Thunar half-way
        assert_eq!(q.take_command(), PipeCmd::Released { finished: false });
    }
}
