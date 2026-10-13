//! A large file passed straight from the network to a paste, through memory only.
//!
//! The first version of the streaming folder spooled every byte to the cache and served
//! the paste from that spool, so a 3 GB file briefly took 6 GB: the spool plus the copy
//! the paste was writing. The spool bought two things, resuming and verification, and
//! neither needs it:
//!
//! * **Resuming.** The paste reads in order, so the next byte it needs is always known. A
//!   dropped connection is resumed by asking the peer for the file from exactly that byte.
//! * **Verification.** Large files are identified by length, not by content hash (see
//!   `BlobCache::identify`), so the check is that the stream delivers exactly `size`
//!   bytes, which this tracks directly.
//!
//! So a pipe holds at most [`WINDOW`] bytes. If the paste falls behind and the window
//! fills, the stream is stopped and restarted from where the paste has got to once it
//! catches up: flow control by re-request, using the protocol's resume offset.

use std::collections::VecDeque;
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
    /// Cancel the running stream: the window is full, or the paste has gone.
    Stop,
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
    grew_at: Instant,
    asked_at: Instant,
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
    st: Mutex<State>,
    cv: Condvar,
}

impl StreamPipe {
    pub fn new(size: u64) -> StreamPipe {
        StreamPipe {
            size,
            st: Mutex::new(State {
                start: 0,
                buf: VecDeque::new(),
                running: false,
                want: None,
                stop: false,
                overfull: false,
                grew_at: Instant::now(),
                asked_at: Instant::now(),
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
            if st.running {
                st.running = false;
                return PipeCmd::Stop;
            }
            return PipeCmd::Nothing;
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
        st.buf.extend(&data[..data.len().min(room)]);
        st.grew_at = Instant::now();
        let ok = st.buf.len() < WINDOW;
        if !ok {
            st.overfull = true;
        }
        self.cv.notify_all();
        ok
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
        if offset >= self.size || len == 0 {
            return Some(Vec::new());
        }
        let end = (offset + len).min(self.size);
        let Ok(mut st) = self.st.lock() else { return None };
        loop {
            // A step back past what is kept, or a jump far ahead: start over from there.
            if offset < st.start || offset > st.have_end() + WINDOW as u64 / 2 {
                st.restart_at(offset);
            }
            if st.have_end() >= end {
                let from = (offset - st.start) as usize;
                let to = (end - st.start) as usize;
                let out: Vec<u8> = st.buf.range(from..to).copied().collect();
                // Drop what the paste has finished with, keeping a little behind it.
                let keep_from = end.saturating_sub(KEEP_BEHIND).max(st.start);
                let drop = (keep_from - st.start) as usize;
                st.buf.drain(..drop);
                st.start = keep_from;
                return Some(out);
            }
            // Missing bytes and nothing on its way, or no progress for a while (a dropped
            // connection, a stream stopped for flow control): ask from where we are.
            let quiet = st.grew_at.elapsed() > NUDGE && st.asked_at.elapsed() > NUDGE;
            if st.want.is_none() && (!st.running || quiet) {
                let at = st.have_end();
                st.want = Some(at);
                st.asked_at = Instant::now();
            }
            if st.grew_at.elapsed() > stall {
                return None;
            }
            st = match self.cv.wait_timeout(st, Duration::from_millis(100)) {
                Ok((g, _)) => g,
                Err(_) => return None,
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
        let p = Arc::new(StreamPipe::new(10));
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
        let p = StreamPipe::new(8);
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
        let p = StreamPipe::new((WINDOW * 2) as u64);
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
        let p = StreamPipe::new(100);
        p.st.lock().unwrap().want = Some(0);
        assert_eq!(p.take_command(), PipeCmd::Start(0));
        p.push(&[1u8; 50]);
        p.release();
        assert_eq!(p.take_command(), PipeCmd::Stop);
        assert!(p.st.lock().unwrap().buf.is_empty());
    }
}
