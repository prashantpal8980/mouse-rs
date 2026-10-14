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
//!
//! **Readers that jump.** A paste reads in order, but a thumbnailer or a video player reads
//! the start, then the index at the end, then the start again, then wherever the user
//! seeks. With one buffer, every jump threw away what was held and restarted the stream:
//! 155 restarts in 30 s for one thumbnail (B24). So the pipe keeps up to [`MAX_REGIONS`]
//! buffered stretches of the file. A read inside one is answered from memory. A read just
//! past one moves the single stream there, continuing from where that stretch ends. Only
//! a read far from all of them opens a new stretch, and the least recently read one goes.
//! There is one stream at a time because the protocol cancels a file's transfer by hash.

use std::collections::VecDeque;

use sha2::{Digest as _, Sha256};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Most bytes held in memory for one file, across all its regions.
pub const WINDOW: usize = 64 * 1024 * 1024;
/// Buffered stretches of one file. Enough for a player's start, index and current place.
pub const MAX_REGIONS: usize = 4;
/// Bytes kept behind the reader, so a small step back is still served.
const KEEP_BEHIND: u64 = 4 * 1024 * 1024;
/// Read-ahead a region keeps beyond its reader when memory runs short and the stream has
/// moved elsewhere. Trimming that first keeps a region worth returning to.
const KEEP_AHEAD: u64 = 8 * 1024 * 1024;
/// A read this far past a region's end still belongs to it: the stream fills the gap.
const JOIN_AHEAD: u64 = (WINDOW / 2) as u64;
/// A reader waiting elsewhere takes the stream from a region someone is also waiting on
/// only after the stream has stayed there this long, so two readers take turns rather than
/// moving it back and forth faster than its bytes can arrive.
const MIN_SLICE: Duration = Duration::from_millis(500);
/// A region counts as waited on if a reader was waiting on it this recently.
const WAITING: Duration = Duration::from_millis(250);
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

/// One buffered stretch of the file.
#[derive(Debug)]
struct Region {
    /// File offset of `buf[0]`.
    start: u64,
    buf: VecDeque<u8>,
    /// Last time a read used it, for choosing which region to drop.
    used: Instant,
    /// The furthest any read has reached in it.
    read_to: u64,
    /// Last time a reader was waiting for bytes in it.
    waited_at: Option<Instant>,
}

impl Region {
    fn new(start: u64) -> Region {
        Region { start, buf: VecDeque::new(), used: Instant::now(), read_to: start, waited_at: None }
    }
    fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }
}

#[derive(Debug)]
struct State {
    regions: Vec<Region>,
    /// The region the stream feeds (or will feed once started).
    active: usize,
    /// A stream from the peer is feeding the active region.
    running: bool,
    want: Option<u64>,
    stop: bool,
    overfull: bool,
    /// The peer said it cannot send this file: fail the paste now, not after the wait.
    failed: bool,
    /// Every byte from 0 up to here has been handed to a reader. A paste is finished when
    /// this reaches the size. Reading only the tail, as a thumbnailer does, is not.
    served_to: u64,
    /// The last paste closed the file. The next open starts from a clean pipe.
    released: bool,
    grew_at: Instant,
    asked_at: Instant,
    /// When the stream was last moved to another region.
    switched_at: Instant,
    /// Running content hash, for a file the sender hashed. `None` for length-only files.
    hasher: Option<Sha256>,
    /// Every byte from 0 up to here has arrived, in order (and been hashed, if hashing).
    /// Bytes that arrive out of order are simply not hashed; a jump never spoils the check.
    inorder_to: u64,
    /// For a hashed file: whether the finished hash matched. `None` until it finishes.
    verified: Option<bool>,
}

impl State {
    fn buffered(&self) -> usize {
        self.regions.iter().map(|r| r.buf.len()).sum()
    }
    fn active_end(&self) -> u64 {
        self.regions[self.active].end()
    }
    /// The region a read at `offset` belongs to, if any: of those it is in or just past,
    /// the one starting closest below it, so a read is never sent to wait on an earlier
    /// region to stream up to bytes a later one already holds.
    fn region_for(&self, offset: u64) -> Option<usize> {
        (0..self.regions.len())
            .filter(|&i| offset >= self.regions[i].start && offset <= self.regions[i].end() + JOIN_AHEAD)
            .max_by_key(|&i| self.regions[i].start)
    }
    /// Feed the stream to region `i`, from where it ends.
    fn activate(&mut self, i: usize) {
        self.active = i;
        self.switched_at = Instant::now();
        self.want = Some(self.regions[i].end());
        self.stop = false;
        self.asked_at = Instant::now();
        self.grew_at = Instant::now();
    }
    /// A read far from every region: open one at `offset` and point the stream at it.
    fn jump_to(&mut self, offset: u64) -> usize {
        if self.regions.len() >= MAX_REGIONS {
            // Drop the least recently read region, never the active one if avoidable.
            let victim = (0..self.regions.len())
                .filter(|&i| i != self.active || self.regions.len() == 1)
                .min_by_key(|&i| self.regions[i].used)
                .unwrap_or(0);
            self.remove(victim);
        }
        self.regions.push(Region::new(offset));
        let i = self.regions.len() - 1;
        self.activate(i);
        i
    }
    fn remove(&mut self, i: usize) {
        self.regions.remove(i);
        if self.regions.is_empty() {
            self.regions.push(Region::new(0));
        }
        if self.active > i || self.active >= self.regions.len() {
            self.active = self.active.saturating_sub(1);
        }
    }
    /// The active region grew into another: drop the other, whose bytes it now covers.
    fn absorb_overtaken(&mut self) {
        let (start, end) = (self.regions[self.active].start, self.active_end());
        let overtaken: Vec<usize> = (0..self.regions.len())
            .filter(|&i| i != self.active && self.regions[i].start > start && self.regions[i].start < end)
            .collect();
        for i in overtaken.into_iter().rev() {
            self.remove(i);
        }
    }
    /// Over the memory cap: drop regions nobody is reading, oldest first. Returns whether
    /// that was enough.
    fn trim(&mut self, reading: Option<usize>) -> bool {
        // First, read-ahead nobody is near: what a region holds well past its reader.
        if self.buffered() >= WINDOW {
            for i in 0..self.regions.len() {
                if i == self.active || Some(i) == reading {
                    continue;
                }
                let r = &mut self.regions[i];
                let keep = (r.read_to + KEEP_AHEAD).saturating_sub(r.start) as usize;
                if r.buf.len() > keep {
                    r.buf.truncate(keep);
                }
            }
        }
        while self.buffered() >= WINDOW && self.regions.len() > 1 {
            let victim = (0..self.regions.len())
                .filter(|&i| i != self.active && Some(i) != reading)
                .min_by_key(|&i| self.regions[i].used);
            match victim {
                Some(v) => self.remove(v),
                None => break,
            }
        }
        self.buffered() < WINDOW
    }
    fn reset(&mut self) {
        self.regions = vec![Region::new(0)];
        self.active = 0;
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
                regions: vec![Region::new(0)],
                active: 0,
                running: false,
                want: None,
                stop: false,
                overfull: false,
                failed: false,
                served_to: 0,
                released: false,
                grew_at: Instant::now(),
                asked_at: Instant::now(),
                switched_at: Instant::now(),
                hasher,
                inorder_to: 0,
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
            return PipeCmd::Released { finished: st.served_to >= self.size };
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

    /// A stream begins at `offset`. Accepted only where the active region ends, so the
    /// bytes can never land at the wrong place.
    pub fn begin(&self, offset: u64) -> bool {
        let Ok(st) = self.st.lock() else { return false };
        offset == st.active_end()
    }

    /// Bytes arrived, starting at file offset `at`. Returns false when the window is
    /// full: the caller stops the stream, and it is restarted from here once the paste has
    /// read some of it.
    ///
    /// Bytes are kept only when `at` is exactly where the active region ends. A read that
    /// jumps moves the stream at once, but the old stream goes on delivering until the
    /// clipboard thread cancels it. Appending those bytes would put them at the wrong
    /// offset, and the paste would then copy a corrupt file (B25).
    pub fn push(&self, at: u64, data: &[u8]) -> bool {
        let Ok(mut st) = self.st.lock() else { return false };
        if at != st.active_end() {
            return true; // a superseded stream: dropped, and cancelled on the next drive
        }
        let room = (self.size - at.min(self.size)) as usize;
        let data = &data[..data.len().min(room)];
        let a = st.active;
        st.regions[a].buf.extend(data);
        self.hash_through(&mut st, at, data);
        st.grew_at = Instant::now();
        st.absorb_overtaken();
        let ok = st.trim(None);
        if !ok {
            st.overfull = true;
        }
        self.cv.notify_all();
        ok
    }

    /// Note the in-order part of `data` (which starts at file offset `at`), hashing it
    /// for a hashed file. Bytes ahead of `inorder_to` are skipped, not held against the
    /// file: they will arrive again in order when the paste gets there.
    fn hash_through(&self, st: &mut State, at: u64, data: &[u8]) {
        let end = at + data.len() as u64;
        if at > st.inorder_to || end <= st.inorder_to {
            return;
        }
        let from = (st.inorder_to - at) as usize;
        if let Some(h) = st.hasher.as_mut() {
            h.update(&data[from..]);
        }
        st.inorder_to = end;
        if st.inorder_to >= self.size && st.verified.is_none() {
            if let (Some(expect), Some(h)) = (&self.expect, st.hasher.take()) {
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
    ///
    /// A pipe lives as long as its file is offered, so copying the same file again reuses
    /// it. After a release, everything from the last paste is reset: its "last grew" time
    /// would fail the first read as stalled, and its finished hash would let the new
    /// bytes through unchecked.
    pub fn reopen(&self) {
        if let Ok(mut st) = self.st.lock() {
            st.failed = false;
            st.stop = false;
            if st.released {
                st.released = false;
                st.reset();
                st.want = None;
                st.running = false;
                st.overfull = false;
                st.served_to = 0;
                st.hasher = self.expect.as_ref().map(|_| Sha256::new());
                st.inorder_to = 0;
                st.verified = None;
            }
            st.grew_at = Instant::now();
            st.asked_at = Instant::now();
        }
    }

    /// The stream ended or broke. A reader still waiting asks for it again.
    pub fn interrupted(&self) {
        if let Ok(mut st) = self.st.lock() {
            st.running = false;
        }
        self.cv.notify_all();
    }

    /// Every byte has arrived, in order from the start. A stream that ran from somewhere
    /// near the end, for a thumbnail, does not count.
    pub fn complete(&self) -> bool {
        self.st.lock().map(|st| st.inorder_to >= self.size).unwrap_or(false)
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
            let r = match st.region_for(offset) {
                Some(r) => r,
                None => st.jump_to(offset),
            };
            st.regions[r].used = Instant::now();
            let mut tail_pending = false;
            if st.regions[r].end() >= end {
                // The last bytes of a hashed file are only handed over once it verified.
                if end == self.size && self.expect.is_some() && st.verified != Some(true) {
                    if st.verified == Some(false) {
                        return Got::Fail;
                    }
                    tail_pending = true;
                } else {
                    let region = &mut st.regions[r];
                    region.read_to = region.read_to.max(end);
                    let from = (offset - region.start) as usize;
                    let to = (end - region.start) as usize;
                    let out: Vec<u8> = region.buf.range(from..to).copied().collect();
                    // Drop what the reader has finished with, keeping a little behind it.
                    let keep_from = end.saturating_sub(KEEP_BEHIND).max(region.start);
                    let drop = (keep_from - region.start) as usize;
                    region.buf.drain(..drop);
                    region.start = keep_from;
                    if offset <= st.served_to {
                        st.served_to = st.served_to.max(end);
                    }
                    return Got::Data(out);
                }
            }
            if !tail_pending {
                st.regions[r].waited_at = Some(Instant::now());
                if r != st.active {
                    // Another region is wanted: move the one stream there, from where it
                    // ends. If a reader is waiting on the active region too, take turns.
                    let a = st.active;
                    let contested = st.regions[a].waited_at.is_some_and(|t| t.elapsed() < WAITING);
                    if !contested || st.switched_at.elapsed() >= MIN_SLICE {
                        st.activate(r);
                    }
                } else {
                    // Missing bytes and nothing on its way, or no progress for a while (a
                    // dropped connection, a stream stopped for flow control): ask from here.
                    let quiet = st.grew_at.elapsed() > NUDGE && st.asked_at.elapsed() > NUDGE;
                    if st.want.is_none() && (!st.running || quiet) {
                        let at = st.active_end();
                        st.want = Some(at);
                        // A reader asking supersedes an earlier release: without this, a
                        // pending stop swallowed the request and the paste sat idle until
                        // the next nudge, ten seconds later.
                        st.stop = false;
                        st.asked_at = Instant::now();
                    }
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
            st.reset();
            st.want = None;
            st.stop = true;
            st.released = true;
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
                    p2.push(0, b"01234");
                    std::thread::sleep(Duration::from_millis(50));
                    p2.push(5, b"56789");
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
        p.push(0, b"abc");
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
        p.push(3, b"defgh");
        assert_eq!(t.join().unwrap().unwrap(), b"defgh");
    }

    #[test]
    fn a_full_window_stops_the_stream_and_memory_stays_bounded() {
        let p = StreamPipe::new((WINDOW * 2) as u64, None);
        p.st.lock().unwrap().want = Some(0);
        assert_eq!(p.take_command(), PipeCmd::Start(0));
        let chunk = vec![7u8; 1024 * 1024];
        let mut accepted = 0;
        while p.push((accepted * chunk.len()) as u64, &chunk) {
            accepted += 1;
        }
        assert!(accepted < WINDOW / chunk.len());
        assert_eq!(p.take_command(), PipeCmd::Stop, "the sender is told to pause");
        // The paste reads; once it needs more than is buffered, the stream restarts there.
        let got = p.read(0, WINDOW as u64, Duration::from_millis(1));
        assert!(got.is_some());
        assert!(p.st.lock().unwrap().buffered() <= KEEP_BEHIND as usize + chunk.len());
    }

    #[test]
    fn closing_the_paste_frees_the_memory_and_stops_the_stream() {
        let p = StreamPipe::new(100, None);
        p.st.lock().unwrap().want = Some(0);
        assert_eq!(p.take_command(), PipeCmd::Start(0));
        p.push(0, &[1u8; 50]);
        p.release();
        assert_eq!(p.take_command(), PipeCmd::Released { finished: false }, "a cancel, reported as one");
        assert_eq!(p.st.lock().unwrap().buffered(), 0);
    }

    #[test]
    fn a_hashed_file_is_verified_and_a_damaged_one_fails_the_paste() {
        let body = b"0123456789".to_vec();
        let good = StreamPipe::new(10, Some(crate::cache::hash_bytes(&body)));
        good.st.lock().unwrap().want = Some(0);
        let _ = good.take_command();
        good.push(0, &body[..4]);
        good.push(4, &body[4..]);
        assert_eq!(good.read(0, 10, Duration::from_secs(1)).unwrap(), body);

        let bad = StreamPipe::new(10, Some(crate::cache::hash_bytes(&body)));
        bad.st.lock().unwrap().want = Some(0);
        let _ = bad.take_command();
        bad.push(0, b"0123456780"); // one byte wrong
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
        p.push(0, b"abcd");
        assert_eq!(p.read(0, 4, Duration::from_secs(1)).unwrap(), b"abcd");
        p.release();
        assert_eq!(p.take_command(), PipeCmd::Released { finished: true });

        let q = StreamPipe::new(4, None);
        q.st.lock().unwrap().want = Some(0);
        let _ = q.take_command();
        q.push(0, b"ab");
        assert_eq!(q.read(0, 2, Duration::from_secs(1)).unwrap(), b"ab");
        q.release(); // cancelled in Thunar half-way
        assert_eq!(q.take_command(), PipeCmd::Released { finished: false });
    }

    #[test]
    fn bytes_from_a_superseded_stream_never_land_at_the_new_offset() {
        // A paste reads from 0 while a stream is running; then a seek far ahead restarts
        // the buffer before the clipboard thread has cancelled the old stream.
        let body: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let p = StreamPipe::new(body.len() as u64, None);
        p.st.lock().unwrap().want = Some(0);
        assert_eq!(p.take_command(), PipeCmd::Start(0));
        assert!(p.begin(0));
        p.push(0, &body[..10]);
        p.st.lock().unwrap().jump_to(150); // the seek
        p.push(10, &body[10..20]); // still in flight from the old stream
        {
            let st = p.st.lock().unwrap();
            assert_eq!(st.regions[st.active].buf.len(), 0, "the stale bytes were dropped");
            assert_eq!(st.buffered(), 10, "and the first 10 bytes are still held");
        }
        assert!(p.begin(150));
        p.push(150, &body[150..160]);
        assert_eq!(p.read(150, 10, Duration::from_secs(1)).unwrap(), &body[150..160]);
    }

    #[test]
    fn copying_the_same_file_again_gets_a_clean_pipe() {
        let body = b"0123456789".to_vec();
        let p = StreamPipe::new(10, Some(crate::cache::hash_bytes(&body)));
        p.st.lock().unwrap().want = Some(0);
        let _ = p.take_command();
        p.push(0, &body);
        assert_eq!(p.read(0, 10, Duration::from_secs(1)).unwrap(), body);
        p.release();
        let _ = p.take_command();
        // Minutes pass before the same file is copied and pasted again.
        p.st.lock().unwrap().grew_at = Instant::now() - Duration::from_secs(600);
        p.reopen();
        assert_eq!(
            p.read_slice(0, 5, Duration::from_secs(180), Duration::from_millis(20)),
            crate::remote_read::Got::Wait,
            "not failed as stalled before anything was asked"
        );
        assert_eq!(p.take_command(), PipeCmd::Start(0), "the new paste asks from the start");
        assert!(p.begin(0));
        p.push(0, b"0123456780"); // damaged this time
        assert!(p.read(4, 6, Duration::from_secs(1)).is_none(), "checked again, not waved through");
    }

    fn byte_at(i: u64) -> u8 {
        (i.wrapping_mul(7) % 251) as u8
    }

    /// The peer, reduced to its essentials: each Start streams from that offset, a newer
    /// Start or a Stop ends the old stream. Returns the offsets it was asked to start at.
    fn fake_peer(p: Arc<StreamPipe>, done: Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<Vec<u64>> {
        std::thread::spawn(move || {
            let mut starts = Vec::new();
            let mut pos: Option<u64> = None;
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                match p.take_command() {
                    PipeCmd::Start(off) => {
                        starts.push(off);
                        pos = p.begin(off).then_some(off);
                    }
                    PipeCmd::Stop | PipeCmd::Released { .. } => pos = None,
                    PipeCmd::Nothing => {}
                }
                match pos {
                    Some(at) if at < p.size() => {
                        let n = (256 * 1024).min(p.size() - at);
                        let chunk: Vec<u8> = (at..at + n).map(byte_at).collect();
                        pos = if p.push(at, &chunk) { Some(at + n) } else { None };
                    }
                    Some(_) => {
                        p.interrupted();
                        pos = None;
                    }
                    None => std::thread::sleep(Duration::from_millis(1)),
                }
            }
            starts
        })
    }

    fn expect_bytes(got: &[u8], from: u64) {
        assert!(got.iter().enumerate().all(|(i, b)| *b == byte_at(from + i as u64)), "wrong bytes at {from}");
    }

    /// B24: a thumbnailer reads the start, the index at the end, the start again, then on.
    /// Going back must be answered from memory, and going on must continue the first
    /// stretch, not start the file over.
    #[test]
    fn a_reader_that_jumps_to_the_end_and_back_does_not_restart_the_file() {
        const MB: u64 = 1024 * 1024;
        let size = 100 * MB;
        let p = Arc::new(StreamPipe::new(size, None));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peer = fake_peer(p.clone(), done.clone());
        let stall = Duration::from_secs(10);

        expect_bytes(&p.read(0, MB, stall).unwrap(), 0);
        let tail = size - 64 * 1024;
        expect_bytes(&p.read(tail, 64 * 1024, stall).unwrap(), tail);
        expect_bytes(&p.read(0, MB, stall).unwrap(), 0); // back to the start
        expect_bytes(&p.read(MB, MB, stall).unwrap(), MB);
        expect_bytes(&p.read(20 * MB, MB, stall).unwrap(), 20 * MB); // and on

        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let starts = peer.join().unwrap();
        assert_eq!(starts[0], 0);
        assert_eq!(starts[1], tail, "the index is fetched on its own");
        assert!(starts.iter().filter(|&&s| s == 0).count() == 1, "the file is never started over: {starts:?}");
        assert!(starts.len() <= 3, "at most one more start, continuing the first stretch: {starts:?}");
        assert!(p.st.lock().unwrap().buffered() <= WINDOW, "memory stays capped");
    }

    /// Reading only the tail is not a finished paste, and does not spoil the hash check
    /// for the paste that reads it all in order afterwards.
    #[test]
    fn a_tail_read_neither_finishes_the_paste_nor_spoils_its_check() {
        let body: Vec<u8> = (0..100u32).map(|i| (i * 3) as u8).collect();
        let p = StreamPipe::new(100, Some(crate::cache::hash_bytes(&body)));
        // A thumbnailer jumps to byte 90; the stream for it delivers the tail.
        p.st.lock().unwrap().jump_to(90);
        let _ = p.take_command();
        assert!(p.begin(90));
        p.push(90, &body[90..]);
        assert!(!p.complete(), "a tail stream is not every byte");
        // Then the real paste, from the start.
        p.st.lock().unwrap().jump_to(0);
        let _ = p.take_command();
        assert!(p.begin(0));
        p.push(0, &body);
        assert!(p.complete());
        assert_eq!(p.read(0, 100, Duration::from_secs(1)).unwrap(), body, "verified and served");
        p.release();
        assert_eq!(p.take_command(), PipeCmd::Released { finished: true });

        let q = StreamPipe::new(100, None);
        q.st.lock().unwrap().jump_to(90);
        let _ = q.take_command();
        assert!(q.begin(90));
        q.push(90, &body[90..]);
        assert_eq!(q.read(90, 10, Duration::from_secs(1)).unwrap(), &body[90..]);
        q.release();
        assert_eq!(q.take_command(), PipeCmd::Released { finished: false }, "only the tail was read");
    }

    /// The peer with a network in between: a Start reaches it, and its first bytes
    /// arrive back, a few milliseconds later. Bytes for a region the pipe no longer feeds
    /// are dropped, as the real receiver does.
    fn slow_peer(p: Arc<StreamPipe>, done: Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<Vec<u64>> {
        std::thread::spawn(move || {
            let mut starts = Vec::new();
            let mut pos: Option<u64> = None;
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                match p.take_command() {
                    PipeCmd::Start(off) => {
                        starts.push(off);
                        std::thread::sleep(Duration::from_millis(3)); // the round trip
                        pos = p.begin(off).then_some(off);
                    }
                    PipeCmd::Stop | PipeCmd::Released { .. } => pos = None,
                    PipeCmd::Nothing => {}
                }
                match pos {
                    Some(at) if at < p.size() => {
                        let n = (256 * 1024).min(p.size() - at);
                        let chunk: Vec<u8> = (at..at + n).map(byte_at).collect();
                        pos = if p.push(at, &chunk) { Some(at + n) } else { None };
                        std::thread::sleep(Duration::from_micros(500));
                    }
                    Some(_) => {
                        p.interrupted();
                        pos = None;
                    }
                    None => std::thread::sleep(Duration::from_millis(1)),
                }
            }
            starts
        })
    }

    /// 2026-10-14 18:43: VLC read the video from the start while Thunar's thumbnailer read
    /// its end. Each moved the one stream to its own region, and the stream answering one
    /// arrived after the other had moved it back, so every byte was dropped and neither
    /// read finished. Both readers must make progress.
    #[test]
    fn two_readers_in_different_places_both_make_progress() {
        const MB: u64 = 1024 * 1024;
        let size = 200 * MB;
        let p = Arc::new(StreamPipe::new(size, None));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peer = slow_peer(p.clone(), done.clone());
        let stall = Duration::from_secs(5);
        let (p1, p2) = (p.clone(), p.clone());
        let player = std::thread::spawn(move || {
            let mut at = 0;
            while at < 16 * MB {
                let got = p1.read(at, 128 * 1024, stall).expect("the player stalled");
                expect_bytes(&got, at);
                at += got.len() as u64;
            }
        });
        let thumbnailer = std::thread::spawn(move || {
            for i in 0..40u64 {
                let at = size - 64 * 1024 - (i % 4) * 4096;
                let got = p2.read(at, 4096, stall).expect("the thumbnailer stalled");
                expect_bytes(&got, at);
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();
        player.join().expect("player");
        thumbnailer.join().expect("thumbnailer");
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let starts = peer.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
        assert!(starts.len() < 10, "the stream was moved {} times: {starts:?}", starts.len());
    }
}
