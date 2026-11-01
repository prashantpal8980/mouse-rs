//! Moving file contents between machines.
//!
//! ## The shape of it
//!
//! Copying a file sends only a **description** — name, size, and a hash of the contents
//! — on the input lane. That is a few hundred bytes whether the file is a text note or
//! a 4 GB ISO, so the entry appears in the other machine's history immediately and
//! costs nothing until someone asks for it.
//!
//! The **receiver** then decides, because it is the receiver's disk and the receiver's
//! network:
//!
//! * already in its cache (same hash copied before) → nothing to transfer at all;
//! * under the user's auto-transfer limit → fetch straight away;
//! * over it → wait, and ask.
//!
//! A fetch is a `BlobRequest` on the input lane; the answer streams back on the bulk
//! lane, paced so it cannot disturb the mouse. Contents are verified against the hash
//! before they are accepted, so a corrupt transfer is refused rather than pasted.
//!
//! ## Resume
//!
//! A request carries an offset, and the partial file in the cache says how far the last
//! attempt got. A dropped link costs the remainder of one file, not a 4 GB restart.
//!
//! ## Why serving runs on its own thread
//!
//! Reading a file is far faster than sending one, so a sender that simply loops
//! `read` → `queue` turns the send queue into a copy of the file: 4 GB on disk becomes
//! 4 GB in memory. The fix is backpressure — stop reading while the lane is backed up
//! (see [`mouse_core::bulk::BulkQueue`]) — but waiting must not happen on the clipboard
//! thread, which has to stay responsive to keep polling the clipboard and answering the
//! panel. So each blob is served by a short-lived thread that waits on the queue, and
//! carries a cancel flag so a transfer can be stopped from either end.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use mouse_core::bulk::{BulkFrame, BulkQueue, CHUNK_BYTES};
use mouse_core::protocol::{FileRef, Message};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::cache::{self, BlobCache};
use crate::files::FileList;
use crate::{ClipStatus, ClipboardHistory, FileSet};

/// Files this process is streaming to the peer right now. The panel asks before quitting
/// while this is non-zero, because quitting stops the other computer's paste.
static SENDING: AtomicUsize = AtomicUsize::new(0);

/// How many files are being sent to the peer at this moment.
pub fn sending_now() -> usize {
    SENDING.load(Ordering::Relaxed)
}

/// Files this process is waiting to receive right now: a paste streaming in, or a
/// fetch into the cache. Refreshed by the clipboard thread on every pass.
static RECEIVING: AtomicUsize = AtomicUsize::new(0);

/// How many files are on their way in from the peer at this moment.
pub fn receiving_now() -> usize {
    RECEIVING.load(Ordering::Relaxed)
}

/// Clear the receive count once the clipboard thread has stopped, so a stale count
/// cannot make the next quit ask about a transfer that no longer exists.
pub fn clear_receiving() {
    RECEIVING.store(0, Ordering::Relaxed);
}

/// Counts one send for as long as it lives, so a send that panics is still uncounted.
struct Sending;

impl Sending {
    fn start() -> Sending {
        SENDING.fetch_add(1, Ordering::Relaxed);
        Sending
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        SENDING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The "don't ask if it'll be quick" rule, with the measurement it depends on.
#[derive(Clone, Copy)]
pub struct QuickSend {
    /// Threshold in seconds. 0 disables the shortcut.
    pub secs: f64,
    /// Measured bytes per second, or 0 if nothing has finished yet.
    pub bps: u64,
}

impl QuickSend {
    fn estimate_secs(&self, bytes: u64) -> Option<f64> {
        (self.bps > 0).then(|| bytes as f64 / self.bps as f64)
    }
}

/// What a bulk transfer is carrying, sent once in the `Begin` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobMeta {
    pub hash: String,
    pub name: String,
    /// Byte offset this stream starts at, so a resumed transfer appends correctly
    /// instead of silently duplicating the prefix.
    #[serde(default)]
    pub offset: u64,
}

/// Receiving side of one in-flight blob.
struct Incoming {
    hash: String,
    /// Where the bytes go: the cache's partial file, or straight to a paste's pipe.
    file: Option<std::fs::File>,
    pipe: Option<Arc<crate::pipe::StreamPipe>>,
    got: u64,
    total: u64,
    entry: Option<u64>,
}

/// Aggregate progress for one history entry, across every file in it.
///
/// A folder is requested as many blobs at once, and they all stream together — 23
/// episodes were genuinely in flight simultaneously in the case that exposed this. Each
/// arriving chunk belongs to one of them, so reporting *that blob's* progress made the
/// entry's percentage jump between whichever files happened to be furthest and least far
/// along: 40%, 25%, 55%, 29%. Nothing was wrong with the transfer; the number was
/// answering a different question every time it was asked.
///
/// What a paste's request for a set actually started.
///
/// The distinction that matters is the middle one: "nothing was asked for" and "nothing
/// is coming" are not the same thing, and collapsing them into one `false` made a paste
/// report failure over a transfer that was running perfectly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStart {
    /// New requests went out to the peer.
    Asked,
    /// Nothing new to ask for: every file still missing is already on its way.
    AlreadyInFlight,
    /// Nothing was asked for and nothing is coming.
    Idle,
}

/// Tracked incrementally rather than recomputed: at ~500 chunks a second, summing the
/// set and stat-ing the cache per chunk would cost more than the transfer.
#[derive(Debug, Clone, Copy)]
struct EntryProgress {
    /// Bytes of this entry we hold or have received, across all its files.
    done: u64,
    /// Bytes the whole entry will be when finished.
    total: u64,
}

/// Sending side of one in-flight blob: just the handle needed to stop it.
struct Serving {
    /// Set to stop the serving thread at the next chunk boundary.
    cancel: Arc<AtomicBool>,
    /// Set by the thread as it exits, so finished serves can be pruned without
    /// joining (a join would block the clipboard thread for the whole transfer).
    done: Arc<AtomicBool>,
}

/// Tracks transfers in both directions.
///
/// Receiving is driven entirely by the clipboard thread, which is the only thread
/// allowed to touch the OS clipboard — so there is never a second opinion about what
/// the clipboard holds. Sending is the exception: it runs on a thread per blob, purely
/// so it can *wait* for the network without stalling everything else. Those threads
/// only read files and push frames; they never touch the clipboard or the history.
pub struct Transfers {
    cache: Arc<BlobCache>,
    incoming: HashMap<u64, Incoming>,
    next_id: u64,
    /// Hashes we have already asked for, so a repeated poll or a second click does not
    /// start the same download twice.
    requested: HashMap<String, u64>,
    /// Blobs we are currently streaming out, keyed by hash.
    serving: HashMap<String, Serving>,
    /// Whole-entry progress, so a multi-file transfer reports one number that only ever
    /// goes up. Keyed by history entry id.
    entry_progress: HashMap<u64, EntryProgress>,
    /// Hashes that are clipboard *images* rather than files, with their dimensions.
    /// They take the same transfer path but are applied to the clipboard as pixels
    /// rather than published as a path.
    pending_images: HashMap<String, (usize, usize)>,
    /// The hash of the most recently completed transfer, so an image can be applied
    /// without the caller having to track transfer ids.
    last_completed: Option<String>,
    /// Large files a paste reads straight from the network, by hash, with their entry.
    /// These never touch the cache: see [`crate::pipe`].
    pipes: HashMap<String, (Arc<crate::pipe::StreamPipe>, u64)>,
    /// Files that have arrived in full this session: streamed to a paste to the last byte,
    /// or received and verified into the cache. A streamed file never enters the cache,
    /// and a pasted cache file is dropped from it, so the cache alone cannot say whether a
    /// set that mixes the two has been delivered.
    arrived: std::collections::HashSet<String>,
    /// Entries whose paste just read a streamed file to the end and closed it.
    pasted: Vec<u64>,
}

impl Transfers {
    pub fn new(cache: Arc<BlobCache>) -> Transfers {
        Transfers {
            cache,
            incoming: HashMap::new(),
            next_id: 0,
            requested: HashMap::new(),
            serving: HashMap::new(),
            entry_progress: HashMap::new(),
            pending_images: HashMap::new(),
            last_completed: None,
            pipes: HashMap::new(),
            arrived: Default::default(),
            pasted: Vec::new(),
        }
    }

    /// The pipe a paste reads `hash` through, created on first use.
    /// `content_hashed` says whether `hash` is a real content hash to verify on the way.
    ///
    /// Copying the same file again reuses the pipe but makes a new history entry, so the
    /// pipe is moved to the newest one. Otherwise a cancelled paste reset the entry the
    /// file was first copied as, and the one on screen kept counting up.
    pub fn pipe_for(&mut self, hash: &str, size: u64, content_hashed: bool, entry: u64) -> Arc<crate::pipe::StreamPipe> {
        let slot = self.pipes.entry(hash.to_string()).or_insert_with(|| {
            let expect = content_hashed.then(|| hash.to_string());
            (Arc::new(crate::pipe::StreamPipe::new(size, expect)), entry)
        });
        slot.1 = entry;
        slot.0.clone()
    }

    /// Every file of `set` not yet held here streams into a paste, and none is running:
    /// nothing moves until the user pastes.
    pub fn waits_for_paste(&self, set: &FileSet) -> bool {
        let mut any = false;
        for f in &set.files {
            if self.cache.contains(&f.hash) {
                continue;
            }
            if !self.pipes.contains_key(&f.hash) || self.requested.contains_key(&f.hash) {
                return false;
            }
            any = true;
        }
        any
    }

    /// Act on what the pipes' readers asked for: start, resume or pause a stream.
    /// Returns the entries whose paste was cancelled part-way, so their status can stop
    /// claiming a transfer is running.
    pub fn drive_pipes(&mut self, to_peer: &UnboundedSender<Message>) -> Vec<u64> {
        let mut cancelled = Vec::new();
        use crate::pipe::PipeCmd;
        let cmds: Vec<(String, u64, u64, PipeCmd)> = self
            .pipes
            .iter()
            .map(|(hash, (pipe, entry))| (hash.clone(), *entry, pipe.size(), pipe.take_command()))
            .collect();
        for (hash, entry, size, cmd) in cmds {
            if cmd == PipeCmd::Nothing {
                continue;
            }
            // Whatever comes next, a stream already running for this file ends first.
            // Cancelling *after* asking again would cancel the new stream too, since the
            // sender stops by hash. Frames still in flight for the old one carry a transfer
            // id forgotten here, so they are dropped.
            if self.requested.remove(&hash).is_some() {
                let _ = to_peer.send(Message::BlobCancel { hash: hash.clone() });
                self.incoming.retain(|_, inc| inc.hash != hash);
            }
            if let PipeCmd::Released { finished } = cmd {
                if finished {
                    // The paste read every byte. The end-of-stream frame can arrive after
                    // the paste has closed the file and freed the buffer, and then cannot
                    // tell that it completed, so this is where a streamed file arrives.
                    self.arrived.insert(hash.clone());
                    self.pasted.push(entry);
                } else {
                    self.entry_progress.remove(&entry);
                    cancelled.push(entry);
                }
                continue;
            }
            if let PipeCmd::Start(offset) = cmd {
                let _ = to_peer.send(Message::BlobRequest { hash: hash.clone(), offset });
                self.requested.insert(hash.clone(), entry);
                tracing::info!(
                    hash = %hash,
                    offset,
                    size,
                    "streaming to the paste: asked the peer for the file from this byte (no copy kept on disk)"
                );
            }
        }
        cancelled
    }

    /// Publish how many requests are still outstanding, for [`receiving_now`].
    pub fn publish_receiving(&self) {
        RECEIVING.store(self.requested.len(), Ordering::Relaxed);
    }

    /// Entries whose paste has read a streamed file to the end since the last call.
    pub fn take_pasted(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.pasted)
    }

    /// Take the hash of the transfer that just finished, if any.
    pub fn take_last_completed(&mut self) -> Option<String> {
        self.last_completed.take()
    }

    /// Put an encoded image into the cache so it can be fetched over the bulk lane.
    /// Returns its hash.
    pub fn stage_image(&self, png: &[u8]) -> Option<String> {
        let hash = cache::hash_bytes(png);
        if !self.cache.contains(&hash) {
            let mut f = self.cache.open_partial(&hash).ok()?;
            f.write_all(png).ok()?;
            drop(f);
            self.cache.finish(&hash).ok()?;
        }
        Some(hash)
    }

    /// Note that an incoming hash is an image, and ask for it unless we hold it.
    /// Returns the bytes immediately when they are already cached.
    pub fn want_image(
        &mut self,
        hash: &str,
        w: usize,
        h: usize,
        to_peer: &UnboundedSender<Message>,
    ) -> Option<Vec<u8>> {
        if self.cache.contains(hash) {
            return std::fs::read(self.cache.blob_path(hash)).ok();
        }
        if !self.requested.contains_key(hash) {
            self.pending_images.insert(hash.to_string(), (w, h));
            self.requested.insert(hash.to_string(), 0);
            let offset = self.cache.partial_len(hash);
            let _ = to_peer.send(Message::BlobRequest { hash: hash.to_string(), offset });
            tracing::info!(hash, w, h, "requested a large clipboard image over the bulk lane");
        }
        None
    }

    /// If this completed hash was an image, hand back its encoded bytes and size.
    pub fn take_completed_image(&mut self, hash: &str) -> Option<(Vec<u8>, usize, usize)> {
        let (w, h) = self.pending_images.remove(hash)?;
        let bytes = std::fs::read(self.cache.blob_path(hash)).ok()?;
        Some((bytes, w, h))
    }



    pub fn cache(&self) -> &Arc<BlobCache> {
        &self.cache
    }

    /// Turn a freshly copied local file list into the description that crosses the
    /// link, ingesting each file into the cache on the way.
    ///
    /// Hashing reads every byte, so this is slow for large files — but it happens on the
    /// clipboard thread, never on an input thread, and it buys deduplication, integrity
    /// and survival of the original being deleted.
    pub fn describe_local(
        &self,
        list: &FileList,
        origin: &str,
        max_files: usize,
        follow_symlinks: bool,
        cache_budget: u64,
        hash_limit: u64,
    ) -> Option<FileSet> {
        let mut files = Vec::new();
        let mut dirs: Vec<String> = Vec::new();
        let mut budget = max_files;
        for path in &list.paths {
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "skipping a copied file: {e}");
                    continue;
                }
            };
            // A folder is just many files that remember where they sat inside it —
            // plus the directory list, which is the only record of the empty ones.
            let members: Vec<(PathBuf, String)> = if meta.is_dir() {
                match cache::walk_dir(path, budget, follow_symlinks) {
                    Ok(v) => {
                        tracing::info!(
                            path = %path.display(),
                            count = v.files.len(),
                            dirs = v.dirs.len(),
                            "walking a copied folder"
                        );
                        dirs.extend(v.dirs);
                        v.files
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), "could not read a copied folder: {e}");
                        continue;
                    }
                }
            } else {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into());
                vec![(path.clone(), name)]
            };
            budget = budget.saturating_sub(members.len());
            for (member, rel) in members {
                match self.cache.identify(&member, cache_budget, hash_limit) {
                    Ok((hash, size, content_hashed)) => files.push(FileRef {
                        name: member
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "file".into()),
                        size,
                        hash,
                        content_hashed,
                        source_path: Some(member.to_string_lossy().into_owned()),
                        rel,
                    }),
                    Err(e) => {
                        tracing::warn!(path = %member.display(), "could not read a copied file: {e}")
                    }
                }
            }
            if budget == 0 {
                break;
            }
        }
        // An entirely empty folder still has directories to recreate, so emptiness of
        // `files` alone is not "nothing was copied".
        if files.is_empty() && dirs.is_empty() {
            return None;
        }
        dirs.sort();
        dirs.dedup();
        Some(FileSet {
            files,
            dirs,
            origin: origin.to_string(),
        })
    }

    /// What a newly-described remote file set should start as.
    ///
    /// `Ready` when we already hold every hash — which is the dedup win: copying
    /// something back that came from here costs nothing at all.
    pub fn initial_status(
        &self,
        set: &FileSet,
        auto_limit: u64,
        quick: Option<QuickSend>,
    ) -> ClipStatus {
        // Every branch below says why it chose what it chose. "The file never arrived"
        // is otherwise unanswerable from a log: waiting for consent, fetching, and
        // silently doing nothing all look identical from outside.
        if set.files.iter().all(|f| self.cache.contains(&f.hash)) {
            tracing::info!(
                files = set.files.len(),
                "DECISION: ready immediately — every file is already in this machine's cache, nothing to transfer"
            );
            return ClipStatus::Ready;
        }
        let total = set.total_bytes();
        if total <= auto_limit {
            tracing::info!(
                bytes = total,
                auto_limit,
                "DECISION: fetching now — under the ask-first limit, so no prompt"
            );
            return ClipStatus::Remote;
        }
        // Over the size limit, but if we can *measure* that it will be done in a few
        // seconds, asking is noise. On a gigabit cable a 100 MB file lands in about a
        // second; on a hotspot the same file does not, and the prompt still appears.
        if let Some(q) = quick {
            if q.secs > 0.0 {
                if let Some(est) = q.estimate_secs(total) {
                    if est <= q.secs {
                        tracing::info!(
                            bytes = total,
                            est_secs = est,
                            "DECISION: fetching now — over the ask limit, but measured as quick on this link"
                        );
                        return ClipStatus::Remote;
                    }
                }
            }
        }
        // The state the owner hit and could not see. Say plainly what is being waited
        // for and what would change it, because nothing else in the system will.
        tracing::warn!(
            bytes = total,
            auto_limit,
            files = set.files.len(),
            measured_bps = quick.map(|q| q.bps).unwrap_or(0),
            "DECISION: NOT FETCHED YET — {} is over the {} limit, so nothing is transferred until it is \
             wanted. Pasting it starts the transfer (the paste itself works at once), as does \
             'Bring it over' in the panel.",
            human(total),
            human(auto_limit),
        );
        ClipStatus::NeedsConsent
    }

    /// Ask the peer for everything in `set` that we do not already hold.
    ///
    /// Returns false if there was nothing to ask for, which means the entry is already
    /// complete and the caller can publish it.
    pub fn request_missing(
        &mut self,
        set: &FileSet,
        entry: u64,
        to_peer: &UnboundedSender<Message>,
        cache_budget: u64,
    ) -> FetchStart {
        // A set larger than the cache budget is allowed. It is the owner's disk and
        // their decision; the job here is to say what it will cost, not to refuse it.
        // What must not happen is the silent version of the failure: completed files
        // being evicted to stay under the budget *while the rest of the set is still
        // arriving*, so the set never completes and re-requests itself forever. That is
        // prevented by protecting in-flight hashes from eviction, not by blocking.
        let total = set.total_bytes();
        if cache_budget > 0 && total > cache_budget {
            tracing::warn!(
                bytes = total,
                cache_budget,
                files = set.files.len(),
                "this transfer is larger than the disk budget — {} against {}. It will still \
                 run, and older cached items are evicted to make room; nothing already \
                 transferred for *this* set is touched while it is in flight.",
                human(total),
                human(cache_budget),
            );
        }

        // Whole-entry baseline, computed once here rather than per chunk: everything we
        // already hold counts as done, because it does — the user is waiting for the set,
        // not for one file.
        // Partly-received files count too. Excluding them meant a paste retried during a
        // download recomputed the baseline as "nothing yet" and reset the progress bar to
        // zero, over and over, on a transfer that was gigabytes in.
        let already: u64 = set
            .files
            .iter()
            .map(|f| {
                if self.cache.contains(&f.hash) {
                    f.size
                } else {
                    self.cache.partial_len(&f.hash).min(f.size)
                }
            })
            .sum();
        let done = self
            .entry_progress
            .get(&entry)
            .map_or(already, |p| p.done.max(already));
        self.entry_progress
            .insert(entry, EntryProgress { done, total: set.total_bytes() });

        let mut asked = false;
        let mut in_flight = false;
        for f in &set.files {
            if self.cache.contains(&f.hash) {
                continue;
            }
            // Fetched by the paste reading it, straight through memory.
            if self.pipes.contains_key(&f.hash) {
                in_flight = true;
                continue;
            }
            if self.requested.contains_key(&f.hash) {
                in_flight = true;
                continue;
            }
            // Resume from whatever a previous attempt already wrote.
            let offset = self.cache.partial_len(&f.hash);
            self.requested.insert(f.hash.clone(), entry);
            let _ = to_peer.send(Message::BlobRequest {
                hash: f.hash.clone(),
                offset,
            });
            tracing::info!(name = %f.name, size = f.size, offset, "requested a file from the peer");
            asked = true;
        }
        match (asked, in_flight) {
            (true, _) => FetchStart::Asked,
            (false, true) => FetchStart::AlreadyInFlight,
            (false, false) => FetchStart::Idle,
        }
    }

    /// Bytes received and bytes expected for an entry currently being fetched.
    ///
    /// The receiving side's own count, which is the only one that survives a paste being
    /// retried mid-transfer.
    pub fn entry_progress(&self, entry: u64) -> Option<(u64, u64)> {
        self.entry_progress.get(&entry).map(|p| (p.done, p.total))
    }

    /// Stop fetching everything belonging to entry `entry`.
    ///
    /// Partial files are deliberately **kept**: the user asked to stop, not to throw
    /// away twenty minutes of a download they may resume. The next request picks up
    /// from the offset already on disk, and the disk budget reclaims it if they never
    /// do. Returns whether anything was actually in flight.
    pub fn cancel_entry(&mut self, entry: u64, to_peer: &UnboundedSender<Message>) -> bool {
        let hashes: Vec<String> = self
            .requested
            .iter()
            .filter(|(_, e)| **e == entry)
            .map(|(h, _)| h.clone())
            .collect();
        if hashes.is_empty() {
            return false;
        }
        for hash in &hashes {
            self.requested.remove(hash);
            self.pending_images.remove(hash);
            // Drop the receiving side, but leave the partial file on disk to resume from.
            self.incoming.retain(|_, inc| &inc.hash != hash);
            // Only the sender can actually stop the bytes.
            let _ = to_peer.send(Message::BlobCancel { hash: hash.clone() });
        }
        // Dropped rather than kept: a resumed request recomputes the baseline from what
        // is actually on disk, which is the only number that stays true across a pause.
        self.entry_progress.remove(&entry);
        tracing::info!(entry, files = hashes.len(), "transfer cancelled by the user");
        true
    }

    /// Serve a peer's request by streaming the blob over the bulk lane.
    ///
    /// The bytes are read from the cache if we hold them, and from the original path if
    /// we do not — which is how a file copied before the cache had it still works. If
    /// neither can produce it, the peer is told why rather than left waiting.
    pub fn serve_request(
        &mut self,
        hash: &str,
        offset: u64,
        history: &ClipboardHistory,
        to_peer: &UnboundedSender<Message>,
        to_bulk: &UnboundedSender<BulkFrame>,
        queue: &Arc<BulkQueue>,
    ) {
        // Forget serves that have finished, so the guard below only ever sees live ones.
        self.serving.retain(|_, s| !s.done.load(Ordering::Relaxed));
        // A second request for a blob already streaming would interleave two chunk
        // sequences under one transfer id and corrupt the result. The first serve is
        // still running and will finish the job.
        if self.serving.contains_key(hash) {
            // Info, not debug. A request that is silently ignored looks identical to one
            // that never arrived, and telling those apart from the far end is impossible.
            tracing::info!(hash, "already streaming this file; ignoring the repeat request");
            return;
        }

        let (mut reader, total, name) = match self.open_for_serving(hash, history) {
            Some(v) => v,
            None => {
                let _ = to_peer.send(Message::BlobUnavailable {
                    hash: hash.to_string(),
                    reason: "no longer at the recorded path on the other machine".into(),
                });
                tracing::warn!(hash, "asked for a file we can no longer produce");
                return;
            }
        };
        if offset > 0 && reader.seek(SeekFrom::Start(offset)).is_err() {
            let _ = to_peer.send(Message::BlobUnavailable {
                hash: hash.to_string(),
                reason: "could not resume from that offset".into(),
            });
            return;
        }

        let transfer = stable_transfer_id(hash);
        let meta = serde_json::to_vec(&BlobMeta {
            hash: hash.to_string(),
            name,
            offset,
        })
        .unwrap_or_default();

        // Logged when it *starts*, not only when it finishes. A 4 GB file takes minutes,
        // and until it completed there was nothing in the log to say the request had even
        // been accepted — so a transfer that was working and one that never began looked
        // exactly the same from the other machine.
        tracing::info!(hash, bytes = total, offset, "serving a file to the peer");
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        self.serving.insert(
            hash.to_string(),
            Serving {
                cancel: cancel.clone(),
                done: done.clone(),
            },
        );

        let to_bulk = to_bulk.clone();
        let queue = queue.clone();
        let hash_owned = hash.to_string();
        let spawned = std::thread::Builder::new()
            .name("clip-serve".into())
            .spawn(move || {
                let _counted = Sending::start();
                stream_blob(
                    &mut reader,
                    transfer,
                    total,
                    meta,
                    &hash_owned,
                    &to_bulk,
                    &queue,
                    &cancel,
                );
                done.store(true, Ordering::Relaxed);
            });
        if let Err(e) = spawned {
            // Out of threads: say so rather than leaving the peer waiting forever.
            tracing::warn!(hash, "could not start sending: {e}");
            self.serving.remove(hash);
            let _ = to_peer.send(Message::BlobUnavailable {
                hash: hash.to_string(),
                reason: "the other machine could not start the transfer".into(),
            });
        }
    }

    /// Stop streaming a blob out, if we are streaming it. Returns whether we were.
    pub fn stop_serving(&mut self, hash: &str) -> bool {
        match self.serving.remove(hash) {
            Some(s) => {
                s.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Stop every outbound stream — used when the whole clipboard thread is shutting
    /// down, so no serving thread outlives it.
    pub fn stop_all_serving(&mut self) {
        for (_, s) in self.serving.drain() {
            s.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Where to read a requested blob from: the cache first, then the original path.
    fn open_for_serving(
        &self,
        hash: &str,
        history: &ClipboardHistory,
    ) -> Option<(std::fs::File, u64, String)> {
        if self.cache.contains(hash) {
            let path = self.cache.blob_path(hash);
            let len = std::fs::metadata(&path).ok()?.len();
            let name = self.name_for(hash, history).unwrap_or_else(|| "file".into());
            return Some((std::fs::File::open(&path).ok()?, len, name));
        }
        // Not cached: fall back to the source path, but only if it is still the file
        // that was advertised. A path that now holds *different* content is not what the
        // peer asked for, and sending it anyway would silently paste the wrong version.
        //
        // **The check has to match the kind of id.** A large file is identified by path,
        // size and modification time rather than by hashing it, and comparing that
        // against a content hash can only ever fail — which made every file over the
        // hash limit unservable, reported to the peer as "no longer at the recorded
        // path" while it sat exactly where it always had.
        let (path, name) = self.source_for(hash, history)?;
        let content_hashed = history.local_file_ref(hash).map(|f| f.content_hashed).unwrap_or(true);
        let p = PathBuf::from(&path);
        if content_hashed {
            let (actual, len) = cache::hash_file(&p).ok()?;
            if actual != hash {
                tracing::warn!(path = %path, "the file at the recorded path has changed since it was copied");
                return None;
            }
            return Some((std::fs::File::open(&p).ok()?, len, name));
        }
        let meta = std::fs::metadata(&p).ok()?;
        if cache::quick_id(&p, &meta) != hash {
            tracing::warn!(path = %path, "the file at the recorded path has changed size or timestamp since it was copied");
            return None;
        }
        Some((std::fs::File::open(&p).ok()?, meta.len(), name))
    }

    fn name_for(&self, hash: &str, history: &ClipboardHistory) -> Option<String> {
        history.file_ref(hash).map(|f| f.name)
    }

    /// Where to re-read a blob from: only a file this computer offered (B66).
    fn source_for(&self, hash: &str, history: &ClipboardHistory) -> Option<(String, String)> {
        let f = history.local_file_ref(hash)?;
        Some((f.source_path?, f.name))
    }

    /// Handle one frame arriving on the bulk lane. Returns the entry id whose status
    /// changed, if any, so the caller can act on a completed set.
    pub fn on_bulk_frame(&mut self, frame: BulkFrame, history: &ClipboardHistory) -> Option<u64> {
        match frame {
            BulkFrame::Begin { transfer, total, meta } => {
                let meta: BlobMeta = serde_json::from_slice(&meta).ok()?;
                // And no bigger than the file the peer described when it offered it.
                if let Some(size) = history.file_ref(&meta.hash).map(|f| f.size) {
                    if total != size {
                        tracing::warn!(hash = %meta.hash, total, size, "ignored a file stream whose length differs from the offer");
                        return None;
                    }
                }
                // Arriving again, so not arrived until this stream ends. Left in, a file
                // pasted earlier made a new paste of it count as done while it was still
                // streaming.
                self.arrived.remove(&meta.hash);
                // Headed straight to a paste: nothing is written to disk.
                if let Some((pipe, _)) = self.pipes.get(&meta.hash) {
                    if !pipe.begin(meta.offset) {
                        tracing::debug!(hash = %meta.hash, offset = meta.offset, "ignored a stream for an offset the paste no longer wants");
                        return None;
                    }
                    let pipe = pipe.clone();
                    let entry = history.find_by_hash(&meta.hash);
                    self.incoming.insert(
                        transfer,
                        Incoming { hash: meta.hash, file: None, pipe: Some(pipe), got: meta.offset, total, entry },
                    );
                    return None;
                }
                // Only what this computer asked for is written to disk (B67, B69). An
                // unasked-for stream could replace a cached file or fill the disk, past both
                // the ask-first limit and the cache budget. (A paste's stream, above, goes
                // to memory and only once that paste has started it.)
                if !self.requested.contains_key(&meta.hash) {
                    tracing::warn!(hash = %meta.hash, "ignored a file stream this computer did not ask for");
                    return None;
                }
                let file = self.cache.open_partial(&meta.hash).ok()?;
                let got = self.cache.partial_len(&meta.hash);
                // The sender starts where we asked it to; if those disagree the stream
                // would be written at the wrong offset, so refuse rather than corrupt.
                if got != meta.offset {
                    tracing::warn!(
                        hash = %meta.hash,
                        expected = got,
                        got = meta.offset,
                        "resume offset disagreement; starting this file over"
                    );
                    self.cache.abandon(&meta.hash);
                    let file = self.cache.open_partial(&meta.hash).ok()?;
                    let entry = history.find_by_hash(&meta.hash);
                    self.incoming.insert(
                        transfer,
                        Incoming { hash: meta.hash, file: Some(file), pipe: None, got: 0, total, entry },
                    );
                    return None;
                }
                let entry = history.find_by_hash(&meta.hash);
                // A resumed file arrives with bytes already on disk. They are part of
                // what the user is waiting for, so they count towards the entry — and
                // they are not in the baseline, which only counted *complete* files.
                if got > 0 {
                    if let Some(agg) = entry.and_then(|id| self.entry_progress.get_mut(&id)) {
                        agg.done = agg.done.saturating_add(got).min(agg.total);
                    }
                }
                self.incoming.insert(
                    transfer,
                    Incoming { hash: meta.hash, file: Some(file), pipe: None, got, total, entry },
                );
                self.next_id = self.next_id.wrapping_add(1);
                None
            }
            BulkFrame::Chunk { transfer, data, .. } => {
                let inc = self.incoming.get_mut(&transfer)?;
                // Never past the declared length (B69): the overrun used to be noticed only
                // at the end, after it was already on disk.
                if inc.got.saturating_add(data.len() as u64) > inc.total {
                    tracing::warn!(hash = %inc.hash, got = inc.got, total = inc.total, "a file stream ran past its length; stopped it");
                    let hash = inc.hash.clone();
                    let entry = inc.entry;
                    let was_pipe = inc.pipe.is_some();
                    self.incoming.remove(&transfer);
                    self.requested.remove(&hash);
                    if !was_pipe {
                        self.cache.abandon(&hash);
                    }
                    if let Some(id) = entry {
                        history.set_status(id, ClipStatus::Unavailable("arrived damaged; try again".into()));
                        return Some(id);
                    }
                    return None;
                }
                if let Some(pipe) = &inc.pipe {
                    // A full window is handled by `drive_pipes`, which pauses the stream.
                    pipe.push(inc.got, &data);
                } else if let Some(Err(e)) = inc.file.as_mut().map(|f| f.write_all(&data)) {
                    tracing::warn!(hash = %inc.hash, "could not write a chunk: {e}");
                    let hash = inc.hash.clone();
                    let entry = inc.entry;
                    self.incoming.remove(&transfer);
                    self.cache.abandon(&hash);
                    self.requested.remove(&hash);
                    if let Some(id) = entry {
                        history.set_status(id, ClipStatus::Unavailable("could not be saved".into()));
                        return Some(id);
                    }
                    return None;
                }
                inc.got += data.len() as u64;
                let n = data.len() as u64;
                let entry = inc.entry;
                if let Some(id) = entry {
                    // The entry's number, not this blob's. With every file in a folder
                    // streaming at once, reporting the blob that happened to deliver the
                    // last chunk is what made the percentage jump backwards.
                    let (done, total) = match self.entry_progress.get_mut(&id) {
                        Some(agg) => {
                            agg.done = agg.done.saturating_add(n).min(agg.total);
                            (agg.done, agg.total)
                        }
                        // No baseline (a resumed session, say): the blob's own numbers are
                        // still better than nothing, and the next request re-establishes it.
                        None => (inc.got, inc.total),
                    };
                    history.set_status(id, ClipStatus::Fetching { done, total });
                }
                entry
            }
            BulkFrame::End { transfer } => {
                let inc = self.incoming.remove(&transfer)?;
                self.requested.remove(&inc.hash);
                if let Some(pipe) = inc.pipe {
                    pipe.interrupted();
                    if pipe.complete() {
                        tracing::info!(hash = %inc.hash, bytes = pipe.size(), "streamed to the paste in full; length verified");
                        self.arrived.insert(inc.hash.clone());
                    }
                    return inc.entry;
                }
                drop(inc.file);
                self.last_completed = Some(inc.hash.clone());
                // Verified by content when the sender hashed it, by length when it did
                // not — see `BlobCache::identify` for why a large file is not hashed.
                // Checked by content unless no description of this hash anywhere says it
                // is a content hash (B67). The newest description alone was the peer's to
                // choose, and saying "not hashed" skipped the check.
                let content_hashed = history.any_content_hashed(&inc.hash).unwrap_or(true);
                match self.cache.finish_checked(&inc.hash, content_hashed, inc.total) {
                    Ok(()) => {
                        tracing::info!(hash = %inc.hash, bytes = inc.got, "content received and verified");
                        self.arrived.insert(inc.hash.clone());
                        inc.entry
                    }
                    Err(e) => {
                        tracing::warn!(hash = %inc.hash, "rejected a transfer: {e}");
                        if let Some(id) = inc.entry {
                            history.set_status(
                                id,
                                ClipStatus::Unavailable("arrived damaged; try again".into()),
                            );
                        }
                        inc.entry
                    }
                }
            }
            BulkFrame::Abort { transfer } => {
                let inc = self.incoming.remove(&transfer)?;
                self.requested.remove(&inc.hash);
                if let Some(pipe) = &inc.pipe {
                    pipe.interrupted(); // the waiting paste asks again from where it got to
                    return None;
                }
                // The partial is kept: the sender gave up, but what arrived is still a
                // valid prefix and a retry can resume from it.
                if let Some(id) = inc.entry {
                    history.set_status(id, ClipStatus::Unavailable("the transfer stopped".into()));
                    return Some(id);
                }
                None
            }
        }
    }

    /// The peer says it cannot produce a hash. Mark every entry waiting on it.
    pub fn on_unavailable(&mut self, hash: &str, reason: &str, history: &ClipboardHistory) -> Option<u64> {
        self.requested.remove(hash);
        if let Some((pipe, _)) = self.pipes.get(hash) {
            pipe.fail();
        }
        let id = history.find_by_hash(hash)?;
        history.set_status(id, ClipStatus::Unavailable(reason.to_string()));
        Some(id)
    }

    /// Do we hold every file in this set?
    pub fn is_complete(&self, set: &FileSet) -> bool {
        set.files.iter().all(|f| self.cache.contains(&f.hash))
    }

    /// Has every file in this set arrived, whether it is held here or went straight into
    /// a paste? This is what ends an entry's "fetching". [`Transfers::is_complete`] alone
    /// never did for a set mixing streamed videos with a small file: it sat at "0% of
    /// 4.9 GB" after every file had been pasted.
    pub fn has_arrived(&self, set: &FileSet) -> bool {
        set.files
            .iter()
            .all(|f| self.cache.contains(&f.hash) || self.arrived.contains(&f.hash) || self.cache.was_pasted(&f.hash))
    }

    /// Materialise a complete set under its real names, ready for the clipboard.
    ///
    /// A folder is rebuilt as a folder — pasting gives you the directory, not its
    /// contents scattered loose — and a plainly-copied file comes back as itself.
    /// Hand a completed set to a folder the user chose, under real names and structure.
    ///
    /// Returns the delivered paths. Hard-linked from the cache where the filesystem
    /// allows it, which is why this costs nothing: the file exists once, under two
    /// names. See [`crate::cache::BlobCache::deliver`].
    pub fn deliver(&self, set: &FileSet, dest: &std::path::Path) -> Option<FileList> {
        let items = self.tree_items(set);
        match self.cache.deliver(dest, &items, &set.dirs) {
            Ok(paths) => {
                tracing::info!(
                    count = paths.len(),
                    dir = %dest.display(),
                    "DELIVERED files straight into your folder — nothing to paste"
                );
                Some(FileList { paths })
            }
            Err(e) => {
                tracing::warn!(dir = %dest.display(), "could not deliver the files: {e}");
                None
            }
        }
    }

    /// `(hash, relative path)` for every file in a set, with a plain file's own name
    /// standing in when it has no relative path.
    fn tree_items(&self, set: &FileSet) -> Vec<(String, String)> {
        set.files
            .iter()
            .map(|f| {
                let rel = if f.rel.is_empty() { f.name.clone() } else { f.rel.clone() };
                (f.hash.clone(), rel)
            })
            .collect()
    }

    /// `(published path, blob path)` for a set, so a paste that *moved* our link can
    /// have it rebuilt before the next one.
    pub fn relink_pairs(&self, list: &FileList, set: &FileSet) -> Vec<(PathBuf, PathBuf)> {
        // Paired positionally: `materialize` walks the set in order, so the nth path
        // published is the nth file. Matching by name would break on a folder holding
        // two files with the same name in different subdirectories.
        list.paths
            .iter()
            .zip(set.files.iter())
            .map(|(p, f)| (p.clone(), self.cache.blob_path(&f.hash)))
            .collect()
    }

    /// The paths a set *will* occupy once fetched, without fetching anything.
    ///
    /// This is what makes copy cheap and paste the moment of truth: the clipboard can
    /// carry real, final paths straight away, and the bytes only have to exist by the
    /// time a paste asks for them.
    ///
    /// **Nothing is created here, and that is load-bearing.** The paste waits by testing
    /// whether the published paths exist yet. For a folder the published path is the
    /// folder itself, so pre-creating it made that test pass immediately: the wait was
    /// skipped, the transfer never started, and the file manager cheerfully copied an
    /// empty directory. A single file happened to work only because a file is not
    /// created by `create_dir_all`.
    pub fn placeholder_paths(&self, set: &FileSet) -> Option<FileList> {
        let items = self.tree_items(set);
        let root = self.cache.tree_root(&set_key(set));
        let mut tops: Vec<PathBuf> = Vec::new();
        for (_, rel) in &items {
            let parts = cache::safe_components(rel);
            if parts.is_empty() {
                continue;
            }
            let mut dest = root.clone();
            for part in &parts {
                dest.push(part);
            }
            let top = root.join(&parts[0]);
            if !tops.contains(&top) {
                tops.push(top);
            }
        }
        (!tops.is_empty()).then_some(FileList { paths: tops })
    }

    pub fn materialize(&self, set: &FileSet) -> Option<FileList> {
        let items: Vec<(String, String)> = set
            .files
            .iter()
            .map(|f| {
                let rel = if f.rel.is_empty() { f.name.clone() } else { f.rel.clone() };
                (f.hash.clone(), rel)
            })
            .collect();
        match self.cache.materialize_tree(&set_key(set), &items, &set.dirs) {
            Ok(paths) => Some(FileList { paths }),
            Err(e) => {
                tracing::warn!("could not prepare the copied files for pasting: {e}");
                None
            }
        }
    }

    /// Keep the cache inside the user's disk budget, never dropping pinned content.
    pub fn enforce_disk_budget(&self, budget: u64, history: &ClipboardHistory) {
        self.cache.evict_to(budget, &history.referenced_hashes());
    }
}

/// A byte count in the largest unit that stays readable, for log lines a human reads.
fn human(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= 1024.0 * MB {
        format!("{:.1} GB", b / (1024.0 * MB))
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// A stable key for a whole selection, so pasting the same copy twice reuses one tree
/// instead of building a fresh one each time.
fn set_key(set: &FileSet) -> String {
    let mut buf = String::new();
    for f in &set.files {
        buf.push_str(&f.hash);
        buf.push('\0');
        buf.push_str(&f.rel);
        buf.push('\n');
    }
    cache::hash_bytes(buf.as_bytes())
}

/// How long to wait before re-checking a full send queue.
///
/// Short enough that the lane never sits idle waiting on us, long enough that a slow
/// link does not turn this thread into a spin loop. At the 8 MB default limit and a
/// 40 Mbps hotspot the queue drains a chunk roughly every 50 ms, so this wakes a few
/// times per chunk and costs nothing.
const QUEUE_WAIT: std::time::Duration = std::time::Duration::from_millis(5);

/// Stream one blob onto the bulk lane, reading only as fast as the lane drains.
///
/// The waiting loop is the entire point: without it this reads the whole file into the
/// send queue as fast as the disk allows, and a 4 GB ISO becomes 4 GB of memory.
#[allow(clippy::too_many_arguments)] // all seven are genuinely per-transfer state
fn stream_blob(
    reader: &mut std::fs::File,
    transfer: u64,
    total: u64,
    meta: Vec<u8>,
    hash: &str,
    to_bulk: &UnboundedSender<BulkFrame>,
    queue: &Arc<BulkQueue>,
    cancel: &AtomicBool,
) {
    if to_bulk
        .send(BulkFrame::Begin {
            transfer,
            total,
            meta,
        })
        .is_err()
    {
        return;
    }

    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut seq = 0u32;
    let mut sent: u64 = 0;
    loop {
        // Backpressure. Checked before reading, so a cancelled or dead transfer stops
        // without first pulling another chunk off the disk.
        while queue.is_full() {
            if cancel.load(Ordering::Relaxed) || to_bulk.is_closed() {
                break;
            }
            std::thread::sleep(QUEUE_WAIT);
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = to_bulk.send(BulkFrame::Abort { transfer });
            tracing::info!(hash, sent, "sending cancelled");
            return;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Account first: the lane credits it back only once the bytes are
                // actually written, and crediting a frame we never queued would let
                // the counter drift downward and defeat the limit.
                queue.add(n as u64);
                if to_bulk
                    .send(BulkFrame::Chunk {
                        transfer,
                        seq,
                        data: buf[..n].to_vec(),
                    })
                    .is_err()
                {
                    queue.take(n as u64);
                    return; // lane gone; the receiver resumes from its partial file
                }
                seq = seq.wrapping_add(1);
                sent += n as u64;
            }
            Err(e) => {
                tracing::warn!(hash, "read failed mid-transfer: {e}");
                let _ = to_bulk.send(BulkFrame::Abort { transfer });
                return;
            }
        }
    }
    let _ = to_bulk.send(BulkFrame::End { transfer });
    tracing::info!(hash, bytes = total, "streamed a file to the peer");
}

/// A transfer id derived from the hash, so the two ends agree without negotiating one —
/// and so a resumed transfer reuses the same id rather than looking like a new stream.
fn stable_transfer_id(hash: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    hash.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B67: a peer re-offers the hash of the user's own copied file, claims it is not
    /// content-hashed, and pushes other bytes. The user's file must stay as it was.
    #[test]
    fn a_peer_cannot_replace_a_cached_file() {
        let (root, mut t, hist) = setup("b67");
        let genuine = root.join("report.txt");
        fs::write(&genuine, b"GENUINE-CONTENT").unwrap();
        let local = t
            .describe_local(&FileList { paths: vec![genuine] }, "kali", 10_000, false, u64::MAX, u64::MAX)
            .unwrap();
        let h = local.files[0].hash.clone();
        hist.record_files(Arc::new(local), false, ClipStatus::Ready).unwrap();
        let evil = FileSet {
            files: vec![FileRef { name: "x".into(), size: 4, hash: h.clone(), content_hashed: false, source_path: None, rel: String::new() }],
            dirs: Vec::new(),
            origin: "peer".into(),
        };
        hist.record_files(Arc::new(evil), true, ClipStatus::Ready).unwrap();
        let push = |t: &mut Transfers, id: u64| {
            let meta = serde_json::to_vec(&BlobMeta { hash: h.clone(), name: "x".into(), offset: 0 }).unwrap();
            t.on_bulk_frame(BulkFrame::Begin { transfer: id, total: 4, meta }, &hist);
            t.on_bulk_frame(BulkFrame::Chunk { transfer: id, seq: 0, data: b"EVIL".to_vec() }, &hist);
            t.on_bulk_frame(BulkFrame::End { transfer: id }, &hist);
        };
        // Unasked for: ignored outright.
        push(&mut t, 7);
        assert_eq!(fs::read(t.cache.blob_path(&h)).unwrap(), b"GENUINE-CONTENT");
        // Even if it had been asked for, the local description says "content hash", so
        // the bytes are checked, and a blob already held is never replaced by length.
        t.requested.insert(h.clone(), 0);
        push(&mut t, 8);
        assert_eq!(fs::read(t.cache.blob_path(&h)).unwrap(), b"GENUINE-CONTENT");
        let _ = fs::remove_dir_all(&root);
    }

    /// B69: nothing unasked-for reaches the disk, and an asked-for stream stops at the
    /// length it announced.
    #[test]
    fn a_peer_cannot_fill_the_disk_with_partials() {
        let (root, mut t, hist) = setup("b69");
        let h = "ab".repeat(32);
        let meta = |h: &str| serde_json::to_vec(&BlobMeta { hash: h.to_string(), name: "y".into(), offset: 0 }).unwrap();
        t.on_bulk_frame(BulkFrame::Begin { transfer: 1, total: 1, meta: meta(&h) }, &hist);
        t.on_bulk_frame(BulkFrame::Chunk { transfer: 1, seq: 0, data: vec![0u8; 1 << 20] }, &hist);
        assert!(!t.cache.part_path(&h).exists(), "an unasked-for stream writes nothing");

        // Asked for, as a 4-byte file: a stream claiming another length is refused...
        let set = FileSet {
            files: vec![FileRef { name: "y".into(), size: 4, hash: h.clone(), content_hashed: false, source_path: None, rel: String::new() }],
            dirs: Vec::new(),
            origin: "peer".into(),
        };
        let entry = hist.record_files(Arc::new(set), true, ClipStatus::Remote).unwrap();
        t.requested.insert(h.clone(), entry);
        t.on_bulk_frame(BulkFrame::Begin { transfer: 2, total: 1 << 30, meta: meta(&h) }, &hist);
        t.on_bulk_frame(BulkFrame::Chunk { transfer: 2, seq: 0, data: vec![0u8; 1 << 20] }, &hist);
        assert!(!t.cache.part_path(&h).exists(), "a length unlike the offer's is refused");
        // ...and one that runs past its length is stopped and its partial removed.
        t.on_bulk_frame(BulkFrame::Begin { transfer: 3, total: 4, meta: meta(&h) }, &hist);
        t.on_bulk_frame(BulkFrame::Chunk { transfer: 3, seq: 0, data: vec![0u8; 1 << 20] }, &hist);
        assert!(!t.cache.part_path(&h).exists(), "an overrun is not kept");
        assert!(matches!(hist.status(entry), Some(ClipStatus::Unavailable(_))));
        let _ = fs::remove_dir_all(&root);
    }

    /// Sizes come from the peer; a wrapped sum slipped a huge set under ask-first.
    #[test]
    fn a_set_total_never_wraps() {
        let f = |size| FileRef { name: "f".into(), size, hash: "a".repeat(64), content_hashed: false, source_path: None, rel: String::new() };
        let set = FileSet { files: vec![f(u64::MAX), f(2)], dirs: Vec::new(), origin: "peer".into() };
        assert_eq!(set.total_bytes(), u64::MAX);
    }

    /// B66: a peer's own file list names a path on this computer and the exact quick id
    /// of the file there. It was never offered here, so it must not be served.
    #[test]
    fn a_file_named_only_by_the_peer_is_never_served() {
        let root = std::env::temp_dir().join(format!("mouse-rs-b66-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut victim = Transfers::new(Arc::new(BlobCache::new(root.join("cache")).unwrap()));
        let hist = ClipboardHistory::new();
        let secret = root.join("never-offered.txt");
        std::fs::write(&secret, b"PRIVATE").unwrap();
        let hash = cache::quick_id(&secret, &std::fs::metadata(&secret).unwrap());
        let peer_ref = FileRef {
            name: "x".into(),
            size: 7,
            hash: hash.clone(),
            content_hashed: false,
            source_path: Some(secret.to_string_lossy().into_owned()),
            rel: String::new(),
        };
        hist.record_files(
            Arc::new(FileSet { files: vec![peer_ref.clone()], dirs: Vec::new(), origin: "peer".into() }),
            true,
            ClipStatus::Remote,
        );
        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (to_bulk, mut bulk_rx) = tokio::sync::mpsc::unbounded_channel();
        victim.serve_request(&hash, 0, &hist, &to_peer, &to_bulk, &test_queue());
        assert!(matches!(peer_rx.try_recv(), Ok(Message::BlobUnavailable { .. })));
        assert!(drain_bulk(&mut bulk_rx).is_empty(), "no byte of the file may be sent");

        // The same file offered by this computer is served as before.
        hist.record_files(
            Arc::new(FileSet { files: vec![peer_ref], dirs: Vec::new(), origin: "me".into() }),
            false,
            ClipStatus::Ready,
        );
        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (to_bulk, mut bulk_rx) = tokio::sync::mpsc::unbounded_channel();
        victim.serve_request(&hash, 0, &hist, &to_peer, &to_bulk, &test_queue());
        assert!(peer_rx.try_recv().is_err(), "a local offer is not refused");
        let mut got = Vec::new();
        for f in drain_bulk(&mut bulk_rx) {
            if let BulkFrame::Chunk { data, .. } = f {
                got.extend_from_slice(&data);
            }
        }
        assert_eq!(got, b"PRIVATE");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R33: the quit warning relies on this count, so a finished or panicked send must
    /// never leave it raised.
    #[test]
    fn a_send_is_counted_only_while_it_runs() {
        let before = sending_now();
        let a = Sending::start();
        let b = Sending::start();
        assert_eq!(sending_now(), before + 2);
        drop(a);
        assert_eq!(sending_now(), before + 1);
        let _ = std::panic::catch_unwind(move || {
            let _held = b;
            panic!("a send that dies");
        });
        assert_eq!(sending_now(), before);
    }
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "mouse-rs-xfer-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn setup(tag: &str) -> (PathBuf, Transfers, ClipboardHistory) {
        let root = tmp(tag);
        let cache = Arc::new(BlobCache::new(root.join("cache")).unwrap());
        (root, Transfers::new(cache), ClipboardHistory::new())
    }

    /// The Linux → Windows case from 2026-10-14: a video pasted in full, copied again,
    /// then pasted and cancelled at 30%. The cancel must be reported, for the new entry.
    #[test]
    fn a_cancelled_paste_of_a_recopied_file_is_reported_for_the_new_entry() {
        use crate::pipe::PipeCmd;
        use std::time::Duration;
        let (root, mut t, _h) = setup("recopy-cancel");
        let (to_peer, _rx) = tokio::sync::mpsc::unbounded_channel();
        let hash = "h".repeat(64);

        // First copy, entry 15: pasted to the end.
        let p = t.pipe_for(&hash, 10, false, 15);
        let _ = p.read_slice(0, 10, Duration::from_secs(5), Duration::from_millis(1));
        assert!(t.drive_pipes(&to_peer).is_empty());
        assert!(p.begin(0));
        p.push(0, b"0123456789");
        assert_eq!(p.read(0, 10, Duration::from_secs(1)).unwrap(), b"0123456789");
        p.release();
        assert!(t.drive_pipes(&to_peer).is_empty(), "a finished paste is not a cancel");
        assert_eq!(t.take_pasted(), vec![15], "it is a completed paste, even with no end frame yet");

        // Copied again, entry 16: the same pipe, reopened by the new paste.
        let p2 = t.pipe_for(&hash, 10, false, 16);
        assert!(Arc::ptr_eq(&p, &p2));
        p2.reopen();
        let _ = p2.read_slice(0, 3, Duration::from_secs(5), Duration::from_millis(1));
        assert!(t.drive_pipes(&to_peer).is_empty());
        assert!(p2.begin(0));
        p2.push(0, b"012");
        assert_eq!(p2.read(0, 3, Duration::from_secs(1)).unwrap(), b"012");
        p2.release(); // cancelled in Explorer at 30%
        assert_eq!(t.drive_pipes(&to_peer), vec![16], "reported as a cancel, for the entry on screen");
        assert_eq!(p2.take_command(), PipeCmd::Nothing);
        let _ = fs::remove_dir_all(root);
    }

    fn test_queue() -> Arc<BulkQueue> {
        Arc::new(BulkQueue::new(8 * 1024 * 1024))
    }

    /// Collect frames until the transfer ends.
    ///
    /// Serving runs on its own thread so it can wait for the network without stalling
    /// the clipboard, which means a test can no longer assume the frames are already
    /// queued the moment `serve_request` returns.
    pub(super) fn drain_bulk(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<BulkFrame>,
    ) -> Vec<BulkFrame> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut out = Vec::new();
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(frame) => {
                    let last = matches!(
                        frame,
                        BulkFrame::End { .. } | BulkFrame::Abort { .. }
                    );
                    out.push(frame);
                    if last {
                        break;
                    }
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }
        out
    }

    #[test]
    fn describing_local_files_hashes_and_caches_them() {
        let (root, t, _h) = setup("describe");
        let a = root.join("one.txt");
        fs::write(&a, b"hello").unwrap();
        let set = t
            .describe_local(&FileList { paths: vec![a.clone()] }, "kali", 10_000, false, u64::MAX, u64::MAX)
            .expect("a file list");
        assert_eq!(set.files.len(), 1);
        assert_eq!(set.files[0].name, "one.txt");
        assert_eq!(set.files[0].size, 5);
        assert_eq!(set.origin, "kali");
        assert!(t.cache.contains(&set.files[0].hash), "the content is now ours");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_set_we_already_hold_needs_no_transfer_at_all() {
        // The dedup win: copying back something that came from here is instant.
        let (root, t, _h) = setup("dedup");
        let a = root.join("known.bin");
        fs::write(&a, vec![7u8; 4096]).unwrap();
        let set = t.describe_local(&FileList { paths: vec![a] }, "kali", 10_000, false, u64::MAX, u64::MAX).unwrap();
        // Tiny auto-limit, so only the cache hit can explain a Ready verdict.
        assert_eq!(t.initial_status(&set, 1, None), ClipStatus::Ready);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_auto_limit_decides_between_fetching_and_asking() {
        let (root, t, _h) = setup("limit");
        let set = FileSet {
            files: vec![FileRef {
                name: "big.iso".into(),
                size: 100 * 1024 * 1024,
                hash: "b".repeat(64),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        assert_eq!(t.initial_status(&set, 1024 * 1024, None), ClipStatus::NeedsConsent);
        assert_eq!(t.initial_status(&set, 200 * 1024 * 1024, None), ClipStatus::Remote);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_survives_a_full_round_trip_and_is_verified() {
        let (root, mut sender, s_hist) = setup("roundtrip-send");
        let (root2, mut receiver, r_hist) = setup("roundtrip-recv");

        // Sender copies a file.
        let src = root.join("report.pdf");
        let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &content).unwrap();
        let set = sender
            .describe_local(&FileList { paths: vec![src] }, "laptop", 10_000, false, u64::MAX, u64::MAX)
            .unwrap();
        s_hist.record_files(Arc::new(set.clone()), false, ClipStatus::Ready);

        // Receiver records it and asks for what it lacks.
        let entry = r_hist
            .record_files(Arc::new(set.clone()), true, ClipStatus::Remote)
            .unwrap();
        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(receiver.request_missing(&set, entry, &to_peer, u64::MAX), FetchStart::Asked);
        let hash = match peer_rx.try_recv().unwrap() {
            Message::BlobRequest { hash, offset } => {
                assert_eq!(offset, 0);
                hash
            }
            other => panic!("expected a BlobRequest, got {other:?}"),
        };

        // Sender streams it on the bulk lane.
        let (to_bulk, mut bulk_rx) = tokio::sync::mpsc::unbounded_channel();
        let (sender_peer, _sp_rx) = tokio::sync::mpsc::unbounded_channel();
        sender.serve_request(&hash, 0, &s_hist, &sender_peer, &to_bulk, &test_queue());

        // Receiver consumes every frame.
        let mut finished = None;
        for frame in drain_bulk(&mut bulk_rx) {
            if let Some(id) = receiver.on_bulk_frame(frame, &r_hist) {
                finished = Some(id);
            }
        }
        assert_eq!(finished, Some(entry));
        assert!(receiver.is_complete(&set), "every file should now be held locally");

        // And it is byte-identical, under its real name.
        let list = receiver.materialize(&set).expect("materialised");
        assert_eq!(list.paths.len(), 1);
        assert_eq!(list.paths[0].file_name().unwrap(), "report.pdf");
        assert_eq!(fs::read(&list.paths[0]).unwrap(), content);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&root2);
    }

    #[test]
    fn a_copied_folder_keeps_its_shape() {
        let (root, t, _h) = setup("folder");
        // A small tree, including a nested directory.
        let proj = root.join("myproject");
        fs::create_dir_all(proj.join("src").join("deep")).unwrap();
        fs::write(proj.join("README.md"), b"readme").unwrap();
        fs::write(proj.join("src").join("main.rs"), b"fn main() {}").unwrap();
        fs::write(proj.join("src").join("deep").join("x.txt"), b"deep").unwrap();

        let set = t
            .describe_local(&FileList { paths: vec![proj.clone()] }, "laptop", 10_000, false, u64::MAX, u64::MAX)
            .expect("a folder should describe");
        assert_eq!(set.files.len(), 3, "every file in the tree");

        let mut rels: Vec<_> = set.files.iter().map(|f| f.rel.clone()).collect();
        rels.sort();
        assert_eq!(
            rels,
            vec!["myproject/README.md", "myproject/src/deep/x.txt", "myproject/src/main.rs"],
            "paths are relative to the folder's parent, so the folder name leads"
        );

        // And it rebuilds as a folder, not as loose files.
        let list = t.materialize(&set).expect("materialised");
        assert_eq!(list.paths.len(), 1, "pasting gives you the folder itself");
        assert_eq!(list.paths[0].file_name().unwrap(), "myproject");
        assert_eq!(
            fs::read(list.paths[0].join("src").join("deep").join("x.txt")).unwrap(),
            b"deep"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_and_a_loose_file_copied_together_both_arrive() {
        let (root, t, _h) = setup("mixed");
        let dir = root.join("docs");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"a").unwrap();
        let loose = root.join("loose.txt");
        fs::write(&loose, b"loose").unwrap();

        let set = t
            .describe_local(&FileList { paths: vec![dir, loose] }, "laptop", 10_000, false, u64::MAX, u64::MAX)
            .unwrap();
        let list = t.materialize(&set).unwrap();
        let names: std::collections::HashSet<_> = list
            .paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains("docs") && names.contains("loose.txt"), "got {names:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_file_count_limit_is_honoured() {
        let (root, t, _h) = setup("many");
        let dir = root.join("lots");
        fs::create_dir_all(&dir).unwrap();
        for i in 0..20 {
            fs::write(dir.join(format!("f{i}.txt")), format!("{i}")).unwrap();
        }
        let set = t
            .describe_local(&FileList { paths: vec![dir] }, "laptop", 5, false, u64::MAX, u64::MAX)
            .unwrap();
        assert_eq!(set.files.len(), 5, "a stray click on a huge folder must be bounded");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_hostile_relative_path_cannot_escape_the_tree() {
        // `rel` is peer-controlled. Taken literally this writes into the user's
        // autostart directory.
        let (root, t, _h) = setup("evil-rel");
        let content = b"payload";
        let hash = cache::hash_bytes(content);
        {
            let mut f = t.cache.open_partial(&hash).unwrap();
            f.write_all(content).unwrap();
        }
        t.cache.finish(&hash).unwrap();

        let set = FileSet {
            files: vec![FileRef {
                name: "evil.desktop".into(),
                size: content.len() as u64,
                hash,
                content_hashed: true,
                source_path: None,
                rel: "../../../.config/autostart/evil.desktop".into(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let list = t.materialize(&set).expect("it should still land, just not there");
        for p in &list.paths {
            let s = p.to_string_lossy();
            assert!(!s.contains(".."), "no traversal survives: {s}");
            assert!(s.starts_with(&root.to_string_lossy().to_string()), "stayed in the cache: {s}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_source_is_reported_rather_than_left_hanging() {
        // The exact question the owner asked: what happens when the file is gone.
        let (root, mut sender, s_hist) = setup("deleted");
        let src = root.join("temporary.txt");
        fs::write(&src, b"here for now").unwrap();
        let set = sender
            .describe_local(&FileList { paths: vec![src.clone()] }, "laptop", 10_000, false, u64::MAX, u64::MAX)
            .unwrap();
        let hash = set.files[0].hash.clone();
        s_hist.record_files(Arc::new(set), false, ClipStatus::Ready);

        // Delete both the original and our cached copy, then serve a request for it.
        fs::remove_file(&src).unwrap();
        fs::remove_file(sender.cache.blob_path(&hash)).unwrap();

        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (to_bulk, mut bulk_rx) = tokio::sync::mpsc::unbounded_channel();
        sender.serve_request(&hash, 0, &s_hist, &to_peer, &to_bulk, &test_queue());

        match peer_rx.try_recv().unwrap() {
            Message::BlobUnavailable { reason, .. } => {
                assert!(reason.contains("no longer at the recorded path"), "got: {reason}");
            }
            other => panic!("expected BlobUnavailable, got {other:?}"),
        }
        assert!(bulk_rx.try_recv().is_err(), "nothing should have been streamed");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_changed_source_is_refused_rather_than_sent_as_the_old_one() {
        // The path still exists but holds different content now. Sending it would paste
        // something other than what the entry claims.
        let (root, mut sender, s_hist) = setup("changed");
        let src = root.join("notes.txt");
        fs::write(&src, b"version one").unwrap();
        let set = sender
            .describe_local(&FileList { paths: vec![src.clone()] }, "laptop", 10_000, false, u64::MAX, u64::MAX)
            .unwrap();
        let hash = set.files[0].hash.clone();
        s_hist.record_files(Arc::new(set), false, ClipStatus::Ready);

        fs::remove_file(sender.cache.blob_path(&hash)).unwrap();
        fs::write(&src, b"version two - different entirely").unwrap();

        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (to_bulk, _bulk_rx) = tokio::sync::mpsc::unbounded_channel();
        sender.serve_request(&hash, 0, &s_hist, &to_peer, &to_bulk, &test_queue());
        assert!(
            matches!(peer_rx.try_recv().unwrap(), Message::BlobUnavailable { .. }),
            "changed content must not be sent as if it were the original"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 2026-10-14 16:36:30: Thunar read the last byte of a 699 MB zip and closed it before
    /// the end-of-stream frame arrived. The close freed the buffer, so the frame could not
    /// see a complete file and the paste was never recorded as done.
    #[test]
    fn a_paste_that_closes_before_the_end_frame_still_counts_as_arrived() {
        use std::time::Duration;
        let (root, mut t, hist) = setup("close-before-end");
        let hash = "z".repeat(64);
        let set = FileSet {
            files: vec![FileRef { name: "big.zip".into(), size: 10, hash: hash.clone(), content_hashed: false, source_path: None, rel: String::new() }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = hist.record_files(Arc::new(set.clone()), true, ClipStatus::Remote).unwrap();
        let (to_peer, _rx) = tokio::sync::mpsc::unbounded_channel();
        let pipe = t.pipe_for(&hash, 10, false, entry);
        let meta = serde_json::to_vec(&BlobMeta { hash: hash.clone(), name: "big.zip".into(), offset: 0 }).unwrap();
        t.on_bulk_frame(BulkFrame::Begin { transfer: 1, total: 10, meta }, &hist);
        t.on_bulk_frame(BulkFrame::Chunk { transfer: 1, seq: 0, data: b"0123456789".to_vec() }, &hist);
        assert_eq!(pipe.read(0, 10, Duration::from_secs(1)).unwrap(), b"0123456789");
        pipe.release(); // Thunar closes the file
        assert!(t.drive_pipes(&to_peer).is_empty(), "not a cancel");
        assert_eq!(t.take_pasted(), vec![entry]);
        t.on_bulk_frame(BulkFrame::End { transfer: 1 }, &hist); // only now
        assert!(t.has_arrived(&set), "recorded as pasted although the end frame came late");
        let _ = fs::remove_dir_all(&root);
    }

    /// 2026-10-14: four videos streamed into Explorer plus one small .md through the
    /// cache. Everything was pasted, yet the entry sat at "0% of 4.9 GB", because the
    /// streamed files never enter the cache and the pasted .md was dropped from it.
    #[test]
    fn a_set_mixing_streamed_and_cached_files_arrives_once_every_file_has() {
        let (root, mut t, hist) = setup("mixed-arrival");
        let md = b"# notes".to_vec();
        let md_hash = cache::hash_bytes(&md);
        let video_hash = "v".repeat(64);
        let file = |name: &str, size: u64, hash: &str, hashed: bool| FileRef {
            name: name.into(),
            size,
            hash: hash.into(),
            content_hashed: hashed,
            source_path: None,
            rel: String::new(),
        };
        let set = FileSet {
            files: vec![file("a.mkv", 10, &video_hash, false), file("n.md", md.len() as u64, &md_hash, true)],
            dirs: Vec::new(),
            origin: "kali".into(),
        };
        let entry = hist.record_files(Arc::new(set.clone()), true, ClipStatus::Remote).unwrap();
        let pipe = t.pipe_for(&video_hash, 10, false, entry);

        // The video streams into the paste, to the last byte.
        let meta = serde_json::to_vec(&BlobMeta { hash: video_hash.clone(), name: "a.mkv".into(), offset: 0 }).unwrap();
        t.on_bulk_frame(BulkFrame::Begin { transfer: 1, total: 10, meta }, &hist);
        t.on_bulk_frame(BulkFrame::Chunk { transfer: 1, seq: 0, data: b"0123456789".to_vec() }, &hist);
        assert_eq!(t.on_bulk_frame(BulkFrame::End { transfer: 1 }, &hist), Some(entry));
        assert!(pipe.complete());
        assert!(!t.has_arrived(&set), "the .md has not come yet");

        // Then the .md, through the cache.
        let (to_peer, _rx) = tokio::sync::mpsc::unbounded_channel();
        t.request_missing(&set, entry, &to_peer, u64::MAX);
        let meta = serde_json::to_vec(&BlobMeta { hash: md_hash.clone(), name: "n.md".into(), offset: 0 }).unwrap();
        t.on_bulk_frame(BulkFrame::Begin { transfer: 2, total: md.len() as u64, meta }, &hist);
        t.on_bulk_frame(BulkFrame::Chunk { transfer: 2, seq: 0, data: md.clone() }, &hist);
        assert_eq!(t.on_bulk_frame(BulkFrame::End { transfer: 2 }, &hist), Some(entry));
        assert!(!t.is_complete(&set), "the video is not held here, and never will be");
        assert!(t.has_arrived(&set), "but every file has arrived");

        // Explorer finished copying the .md, so mouse-rs dropped its own copy.
        t.cache.discard(&md_hash);
        assert!(t.has_arrived(&set), "still arrived after the pasted copy is dropped");

        // Pasted again later: while the video streams again, the copy is not done.
        let meta = serde_json::to_vec(&BlobMeta { hash: video_hash.clone(), name: "a.mkv".into(), offset: 0 }).unwrap();
        pipe.release();
        pipe.reopen();
        t.on_bulk_frame(BulkFrame::Begin { transfer: 3, total: 10, meta }, &hist);
        t.on_bulk_frame(BulkFrame::Chunk { transfer: 3, seq: 0, data: b"01234".to_vec() }, &hist);
        assert!(!t.has_arrived(&set), "half a video has not arrived, whatever an earlier paste did");
        let _ = fs::remove_dir_all(&root);
    }

    /// B35: small files already in the cache (from before this run, say) are pasted and
    /// our copies removed before the big file streams. They never passed through this
    /// run's arrivals, so only the paste's own record can say they arrived.
    #[test]
    fn files_pasted_from_the_cache_before_the_big_one_still_count_as_arrived() {
        let (root, t, _hist) = setup("b35");
        let small = b"small file".to_vec();
        let small_hash = cache::hash_bytes(&small);
        {
            use std::io::Write;
            let mut f = t.cache.open_partial(&small_hash).unwrap();
            f.write_all(&small).unwrap();
        }
        t.cache.finish(&small_hash).unwrap();
        let set = FileSet {
            files: vec![FileRef {
                name: "s.txt".into(),
                size: small.len() as u64,
                hash: small_hash.clone(),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "kali".into(),
        };
        assert!(t.has_arrived(&set));
        t.cache.discard_pasted(&small_hash);
        assert!(!t.cache.contains(&small_hash));
        assert!(t.has_arrived(&set), "pasted in full, then our copy removed: still arrived");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_interrupted_transfer_resumes_instead_of_starting_over() {
        let (root, mut receiver, r_hist) = setup("resume");
        let hash = cache::hash_bytes(b"0123456789");
        let set = FileSet {
            files: vec![FileRef {
                name: "x.bin".into(),
                size: 10,
                hash: hash.clone(),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = r_hist
            .record_files(Arc::new(set.clone()), true, ClipStatus::Remote)
            .unwrap();

        // A previous attempt got half way.
        {
            let mut f = receiver.cache.open_partial(&hash).unwrap();
            f.write_all(b"01234").unwrap();
        }
        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        receiver.request_missing(&set, entry, &to_peer, u64::MAX);
        match peer_rx.try_recv().unwrap() {
            Message::BlobRequest { offset, .. } => {
                assert_eq!(offset, 5, "the request must resume from what we already have")
            }
            other => panic!("expected BlobRequest, got {other:?}"),
        }

        // The remainder arrives and completes the file.
        let meta = serde_json::to_vec(&BlobMeta { hash: hash.clone(), name: "x.bin".into(), offset: 5 })
            .unwrap();
        receiver.on_bulk_frame(BulkFrame::Begin { transfer: 1, total: 10, meta }, &r_hist);
        receiver.on_bulk_frame(
            BulkFrame::Chunk { transfer: 1, seq: 0, data: b"56789".to_vec() },
            &r_hist,
        );
        let done = receiver.on_bulk_frame(BulkFrame::End { transfer: 1 }, &r_hist);
        assert_eq!(done, Some(entry));
        assert!(receiver.cache.contains(&hash), "the resumed file must verify and land");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn content_that_does_not_match_its_hash_never_reaches_the_history_as_ready() {
        let (root, mut receiver, r_hist) = setup("corrupt");
        let hash = cache::hash_bytes(b"the real content");
        let set = FileSet {
            files: vec![FileRef { name: "f".into(), size: 16, hash: hash.clone(), content_hashed: true, source_path: None, rel: String::new() }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = r_hist
            .record_files(Arc::new(set), true, ClipStatus::Remote)
            .unwrap();

        receiver.requested.insert(hash.clone(), entry);
        let meta = serde_json::to_vec(&BlobMeta { hash: hash.clone(), name: "f".into(), offset: 0 }).unwrap();
        receiver.on_bulk_frame(BulkFrame::Begin { transfer: 2, total: 16, meta }, &r_hist);
        receiver.on_bulk_frame(
            BulkFrame::Chunk { transfer: 2, seq: 0, data: b"something else!!".to_vec() },
            &r_hist,
        );
        receiver.on_bulk_frame(BulkFrame::End { transfer: 2 }, &r_hist);

        assert!(!receiver.cache.contains(&hash), "corrupt content must not be accepted");
        assert!(
            matches!(r_hist.status(entry), Some(ClipStatus::Unavailable(_))),
            "and the entry must say so rather than claim it is ready"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_paste_retried_mid_transfer_keeps_the_progress_it_had() {
        // The receiving side re-asks for a set every time the file manager re-checks the
        // clipboard, which during a large paste is constantly. Recomputing the baseline
        // as "only whole files count" threw away the partial in progress and snapped the
        // bar back to zero — the reported symptom was a transfer that climbed to 3% and
        // appeared to stop, over and over, while the bytes were in fact still arriving.
        let (root, mut receiver, r_hist) = setup("retry-progress");
        let hash = "d".repeat(64);
        let set = FileSet {
            files: vec![FileRef {
                name: "big".into(),
                size: 1000,
                hash: hash.clone(),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = r_hist
            .record_files(Arc::new(set.clone()), true, ClipStatus::Remote)
            .unwrap();
        let (to_peer, _peer_rx) = tokio::sync::mpsc::unbounded_channel();

        assert_eq!(receiver.request_missing(&set, entry, &to_peer, u64::MAX), FetchStart::Asked);
        assert_eq!(receiver.entry_progress(entry), Some((0, 1000)));

        // 400 of 1000 bytes land.
        let meta =
            serde_json::to_vec(&BlobMeta { hash: hash.clone(), name: "big".into(), offset: 0 })
                .unwrap();
        receiver.on_bulk_frame(BulkFrame::Begin { transfer: 1, total: 1000, meta }, &r_hist);
        receiver.on_bulk_frame(
            BulkFrame::Chunk { transfer: 1, seq: 0, data: vec![0u8; 400] },
            &r_hist,
        );
        assert_eq!(receiver.entry_progress(entry), Some((400, 1000)));

        // The paste re-asks while those 400 bytes are still only a partial.
        assert_eq!(
            receiver.request_missing(&set, entry, &to_peer, u64::MAX),
            FetchStart::AlreadyInFlight
        );
        let (done, total) = receiver.entry_progress(entry).expect("still tracked");
        assert_eq!(total, 1000);
        assert!(
            done >= 400,
            "a retry must not report less progress than already arrived; got {done}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn asking_twice_only_downloads_once() {
        let (root, mut receiver, r_hist) = setup("twice");
        let set = FileSet {
            files: vec![FileRef {
                name: "f".into(),
                size: 10,
                hash: "c".repeat(64),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = r_hist.record_files(Arc::new(set.clone()), true, ClipStatus::Remote).unwrap();
        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();

        assert_eq!(receiver.request_missing(&set, entry, &to_peer, u64::MAX), FetchStart::Asked);
        assert!(peer_rx.try_recv().is_ok());
        assert_eq!(
            receiver.request_missing(&set, entry, &to_peer, u64::MAX),
            FetchStart::AlreadyInFlight,
            "a second click must not re-request, and must say the transfer is running \
             rather than report nothing happened — a paste reads this as failure"
        );
        assert!(peer_rx.try_recv().is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_peer_saying_no_updates_the_entry() {
        let (root, mut receiver, r_hist) = setup("unavail");
        let hash = "d".repeat(64);
        let set = FileSet {
            files: vec![FileRef { name: "f".into(), size: 1, hash: hash.clone(), content_hashed: true, source_path: None, rel: String::new() }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = r_hist.record_files(Arc::new(set), true, ClipStatus::Remote).unwrap();
        let got = receiver.on_unavailable(&hash, "no longer at the recorded path", &r_hist);
        assert_eq!(got, Some(entry));
        assert_eq!(
            r_hist.status(entry),
            Some(ClipStatus::Unavailable("no longer at the recorded path".into()))
        );
        let _ = fs::remove_dir_all(&root);
    }
    /// The one that matters most: a sender must not read a file into memory faster than
    /// the network can drain it. Before the queue existed this loop ran to completion and
    /// the whole file sat in the channel — for a 4 GB ISO, 4 GB of RAM.
    #[test]
    fn a_big_file_does_not_pile_up_in_memory_while_the_lane_is_slow() {
        let (root, mut sender, s_hist) = setup("backpressure");
        let src = root.join("big.bin");
        // Comfortably more than the queue limit below, so the sender *must* block.
        let content = vec![7u8; 4 * CHUNK_BYTES];
        fs::write(&src, &content).unwrap();
        let set = sender
            .describe_local(&FileList { paths: vec![src] }, "laptop", 10_000, false, u64::MAX, u64::MAX)
            .unwrap();
        let hash = set.files[0].hash.clone();
        s_hist.record_files(Arc::new(set), false, ClipStatus::Ready);

        // A queue that holds one chunk, and nothing drains it — so after the first chunk
        // the sender has to wait.
        let queue = Arc::new(BulkQueue::new(1));
        let (to_bulk, mut bulk_rx) = tokio::sync::mpsc::unbounded_channel();
        let (to_peer, _peer_rx) = tokio::sync::mpsc::unbounded_channel();
        sender.serve_request(&hash, 0, &s_hist, &to_peer, &to_bulk, &queue);

        // Give it every chance to run away.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            queue.queued() <= CHUNK_BYTES as u64,
            "sender queued {} bytes against a {}-byte limit — backpressure is not applied",
            queue.queued(),
            CHUNK_BYTES
        );

        // Stalled, not finished: no End frame can have been sent.
        let mut frames = Vec::new();
        while let Ok(f) = bulk_rx.try_recv() {
            frames.push(f);
        }
        assert!(
            !frames.iter().any(|f| matches!(f, BulkFrame::End { .. })),
            "the transfer completed despite a full queue, so nothing was holding it back"
        );

        sender.stop_serving(&hash);
        queue.take(u64::MAX);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cancelling_stops_the_stream_and_says_so() {
        let (root, mut sender, s_hist) = setup("cancel-send");
        let src = root.join("film.mkv");
        fs::write(&src, vec![3u8; 3 * CHUNK_BYTES]).unwrap();
        let set = sender
            .describe_local(&FileList { paths: vec![src] }, "laptop", 10_000, false, u64::MAX, u64::MAX)
            .unwrap();
        let hash = set.files[0].hash.clone();
        s_hist.record_files(Arc::new(set), false, ClipStatus::Ready);

        let queue = Arc::new(BulkQueue::new(1));
        let (to_bulk, mut bulk_rx) = tokio::sync::mpsc::unbounded_channel();
        let (to_peer, _peer_rx) = tokio::sync::mpsc::unbounded_channel();
        sender.serve_request(&hash, 0, &s_hist, &to_peer, &to_bulk, &queue);
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert!(sender.stop_serving(&hash), "the stream should have been running");
        // Freeing the queue lets the stopped thread wake up and emit its Abort.
        queue.take(u64::MAX);
        let frames = drain_bulk(&mut bulk_rx);
        assert!(
            frames.iter().any(|f| matches!(f, BulkFrame::Abort { .. })),
            "a cancelled transfer must tell the receiver it stopped, not just go quiet"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_receiver_cancelling_clears_the_request_so_a_click_can_resume() {
        let (root, mut receiver, r_hist) = setup("cancel-recv");
        let hash = "b".repeat(64);
        let set = FileSet {
            files: vec![FileRef {
                name: "iso".into(),
                size: 999,
                hash: hash.clone(),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = r_hist
            .record_files(Arc::new(set.clone()), true, ClipStatus::Remote)
            .unwrap();
        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(receiver.request_missing(&set, entry, &to_peer, u64::MAX), FetchStart::Asked);
        let _ = peer_rx.try_recv();

        assert!(receiver.cancel_entry(entry, &to_peer));
        match peer_rx.try_recv().unwrap() {
            Message::BlobCancel { hash: h } => assert_eq!(h, hash),
            other => panic!("expected BlobCancel, got {other:?}"),
        }
        // The request is forgotten, so clicking again really asks again rather than being
        // swallowed as a duplicate. That is what makes the entry usable after Stop.
        assert_eq!(
            receiver.request_missing(&set, entry, &to_peer, u64::MAX),
            FetchStart::Asked,
            "after cancelling, clicking the entry must start a fresh request"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_folder_is_rebuilt_rather_than_lost() {
        let (root, sender, _h) = setup("empty-dirs");
        let tree = root.join("project");
        fs::create_dir_all(tree.join("logs")).unwrap(); // stays empty
        fs::create_dir_all(tree.join("src")).unwrap();
        fs::write(tree.join("src").join("main.rs"), b"fn main() {}").unwrap();

        let set = sender
            .describe_local(&FileList { paths: vec![tree] }, "laptop", 10_000, false, u64::MAX, u64::MAX)
            .unwrap();
        assert!(
            set.dirs.iter().any(|d| d.ends_with("project/logs")),
            "the empty directory must be described or it cannot be recreated: {:?}",
            set.dirs
        );

        let list = sender.materialize(&set).expect("materialised");
        let top = &list.paths[0];
        assert!(top.join("logs").is_dir(), "the empty directory should exist after pasting");
        assert!(top.join("src").join("main.rs").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_unless_the_owner_asked_for_them() {
        let (root, sender, _h) = setup("symlinks");
        let tree = root.join("folder");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("real.txt"), b"real").unwrap();
        let outside = root.join("outside.txt");
        fs::write(&outside, b"not picked by the user").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("link.txt")).unwrap();

        let skipped = sender
            .describe_local(&FileList { paths: vec![tree.clone()] }, "laptop", 10_000, false, u64::MAX, u64::MAX)
            .unwrap();
        assert_eq!(skipped.files.len(), 1, "the link must not be copied by default");

        let followed = sender
            .describe_local(&FileList { paths: vec![tree] }, "laptop", 10_000, true, u64::MAX, u64::MAX)
            .unwrap();
        assert_eq!(followed.files.len(), 2, "with following on, the link's target is included");
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_terminates_instead_of_running_until_the_file_limit() {
        let (root, sender, _h) = setup("symlink-loop");
        let tree = root.join("loopy");
        fs::create_dir_all(tree.join("inner")).unwrap();
        fs::write(tree.join("inner").join("a.txt"), b"a").unwrap();
        // A link pointing back at its own ancestor. Followed naively this recurses until
        // the file-count limit stops it, producing thousands of bogus duplicates.
        std::os::unix::fs::symlink(&tree, tree.join("inner").join("back")).unwrap();

        let set = sender
            .describe_local(&FileList { paths: vec![tree] }, "laptop", 10_000, true, u64::MAX, u64::MAX)
            .unwrap();
        assert_eq!(set.files.len(), 1, "the loop must be detected, not walked: {:?}", set.files);
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use std::fs;

    /// The reported bug: copying a 23-file folder showed a percentage that jumped
    /// backwards — 40%, 25%, 55%, 29% — because every file was streaming at once and
    /// each chunk reported *its own* file's progress rather than the set's.
    #[test]
    fn progress_across_many_files_only_ever_moves_forward() {
        let root = std::env::temp_dir().join("mouse-rs-progress-test");
        let _ = fs::remove_dir_all(&root);
        let cache = Arc::new(BlobCache::new(root.join("cache")).unwrap());
        let mut t = Transfers::new(cache);
        let hist = ClipboardHistory::new();

        // Three files of very different sizes, as episodes and subtitles would be.
        let sizes = [900_000u64, 300_000, 60_000];
        let files: Vec<FileRef> = sizes
            .iter()
            .enumerate()
            .map(|(i, size)| FileRef {
                name: format!("ep{i}.mkv"),
                size: *size,
                hash: format!("{:0>64}", i + 1),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            })
            .collect();
        let set = FileSet { files, dirs: Vec::new(), origin: "laptop".into() };
        let total: u64 = sizes.iter().sum();
        let entry = hist
            .record_files(Arc::new(set.clone()), true, ClipStatus::Remote)
            .unwrap();

        let (to_peer, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(t.request_missing(&set, entry, &to_peer, u64::MAX), FetchStart::Asked);

        // Open all three at once, exactly as the sender does.
        for (i, size) in sizes.iter().enumerate() {
            let meta = serde_json::to_vec(&BlobMeta {
                hash: set.files[i].hash.clone(),
                name: set.files[i].name.clone(),
                offset: 0,
            })
            .unwrap();
            t.on_bulk_frame(
                BulkFrame::Begin { transfer: i as u64, total: *size, meta },
                &hist,
            );
        }

        // Interleave chunks in a deliberately unhelpful order: the big file races ahead,
        // then the small one delivers, then the big one again. Under the old code this
        // is precisely the sequence that made the number fall.
        let order = [0usize, 0, 0, 2, 1, 0, 2, 1, 0, 1, 2, 0];
        let mut last_pct = 0u64;
        let mut saw_progress = false;
        for i in order {
            t.on_bulk_frame(
                BulkFrame::Chunk {
                    transfer: i as u64,
                    seq: 0,
                    data: vec![0u8; 20_000],
                },
                &hist,
            );
            let Some(ClipStatus::Fetching { done, total: reported }) = hist.status(entry) else {
                panic!("expected a Fetching status");
            };
            assert_eq!(
                reported, total,
                "the total must be the whole set, not whichever file just delivered"
            );
            let pct = done * 100 / reported;
            assert!(
                pct >= last_pct,
                "progress went backwards: {last_pct}% then {pct}% — this is the reported bug"
            );
            if pct > last_pct {
                saw_progress = true;
            }
            last_pct = pct;
        }
        assert!(saw_progress, "the percentage should actually advance, not just not regress");
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::fs;

    /// A set bigger than the cache budget is the owner's call, not ours to refuse — but
    /// it must not silently fail either. The old failure was eviction deleting completed
    /// files while the rest of the set was still arriving, so it never finished.
    #[test]
    fn a_set_larger_than_the_budget_still_transfers() {
        let root = std::env::temp_dir().join("mouse-rs-budget-test");
        let _ = fs::remove_dir_all(&root);
        let cache = Arc::new(BlobCache::new(root.join("cache")).unwrap());
        let mut t = Transfers::new(cache);
        let hist = ClipboardHistory::new();

        let set = FileSet {
            files: vec![FileRef {
                name: "series.mkv".into(),
                size: 7 * 1024 * 1024 * 1024,
                hash: "a".repeat(64),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = hist
            .record_files(Arc::new(set.clone()), true, ClipStatus::Remote)
            .unwrap();
        let (to_peer, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let budget = 2 * 1024 * 1024 * 1024;
        assert_eq!(
            t.request_missing(&set, entry, &to_peer, budget),
            FetchStart::Asked,
            "an oversized set is the owner's decision and must still be requested"
        );
        assert!(rx.try_recv().is_ok(), "it should really have been asked for");
    }

    /// The bug that made an oversized set impossible: files that arrived were evicted to
    /// stay under the budget while the rest were still in flight, so the set was never
    /// complete and asked for itself again forever.
    #[test]
    fn eviction_never_takes_files_from_a_transfer_still_running() {
        let hist = ClipboardHistory::new();
        let hash = "c".repeat(64);
        let set = FileSet {
            files: vec![FileRef {
                name: "ep.mkv".into(),
                size: 10,
                hash: hash.clone(),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = hist
            .record_files(Arc::new(set), true, ClipStatus::Fetching { done: 1, total: 10 })
            .unwrap();
        assert!(
            hist.referenced_hashes().contains(&hash),
            "a file being fetched right now must be protected from eviction"
        );

        // And it stays protected once complete. Losing protection at `Ready` is what
        // made a 6.8 GB folder evict itself down to the 2 GB budget the instant it
        // finished, so the entry claimed to be ready while its files were gone and
        // clicking it restarted the transfer at 29%.
        hist.set_status(entry, ClipStatus::Ready);
        assert!(
            hist.referenced_hashes().contains(&hash),
            "a completed entry must keep its files, or it is lying about being ready"
        );

        // Only removing the entry releases them.
        hist.remove(entry);
        assert!(!hist.referenced_hashes().contains(&hash));
    }
}

#[cfg(test)]
mod placeholder_tests {
    use super::*;
    use std::fs;

    /// A paste waits by checking whether the published paths exist yet. Creating them up
    /// front made that check pass instantly for a *folder* — whose published path is the
    /// folder itself — so the wait was skipped, no transfer started, and the file manager
    /// copied an empty directory. A single file only worked by accident, `create_dir_all`
    /// not creating files.
    #[test]
    fn placeholder_paths_do_not_exist_yet_for_a_folder_or_a_file() {
        let root = std::env::temp_dir().join("mouse-rs-placeholder");
        let _ = fs::remove_dir_all(&root);
        let cache = Arc::new(BlobCache::new(root.join("cache")).unwrap());
        let t = Transfers::new(cache);

        let folder = FileSet {
            files: vec![
                FileRef {
                    name: "ep01.mkv".into(),
                    size: 10,
                    hash: "1".repeat(64),
                    content_hashed: true,
                    source_path: None,
                    rel: "Season 1/ep01.mkv".into(),
                },
                FileRef {
                    name: "ep02.mkv".into(),
                    size: 10,
                    hash: "2".repeat(64),
                    content_hashed: true,
                    source_path: None,
                    rel: "Season 1/ep02.mkv".into(),
                },
            ],
            dirs: vec!["Season 1".into()],
            origin: "laptop".into(),
        };
        let list = t.placeholder_paths(&folder).expect("paths");
        assert_eq!(list.paths.len(), 1, "a folder publishes one top-level path");
        assert!(
            !list.paths[0].exists(),
            "the folder must NOT exist yet, or the paste stops waiting and copies nothing"
        );

        let single = FileSet {
            files: vec![FileRef {
                name: "film.mkv".into(),
                size: 10,
                hash: "3".repeat(64),
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let list = t.placeholder_paths(&single).expect("paths");
        assert!(!list.paths[0].exists(), "nor may a single file");
        let _ = fs::remove_dir_all(&root);
    }

    /// And once the content arrives, `materialize` must produce exactly the paths that
    /// were published — otherwise the waiting paste watches the wrong location for ever.
    #[test]
    fn materialize_lands_on_the_same_paths_that_were_published() {
        let root = std::env::temp_dir().join("mouse-rs-placeholder-match");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let cache = Arc::new(BlobCache::new(root.join("cache")).unwrap());
        let t = Transfers::new(cache.clone());

        let src = root.join("film.mkv");
        fs::write(&src, b"the actual bytes").unwrap();
        let (hash, size) = cache.ingest(&src, u64::MAX).unwrap();
        let set = FileSet {
            files: vec![FileRef {
                name: "film.mkv".into(),
                size,
                hash,
                content_hashed: true,
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };

        let promised = t.placeholder_paths(&set).expect("promised");
        let delivered = t.materialize(&set).expect("materialised");
        assert_eq!(
            promised.paths, delivered.paths,
            "the paste is waiting on the promised path; materialise must use it"
        );
        assert!(delivered.paths[0].is_file(), "and it exists now");
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod quick_id_tests {
    use super::tests::drain_bulk;
    use super::*;
    use std::fs;

    /// The regression this exists to prevent: a file identified without hashing could
    /// not be served at all. `open_for_serving` re-hashed the source and compared it to
    /// the advertised id, which for a quick id can only ever differ — so the peer was
    /// told "no longer at the recorded path" about a file that had never moved, and
    /// every file over the hash limit was unservable.
    #[test]
    fn a_file_identified_without_hashing_can_still_be_served() {
        let root = std::env::temp_dir().join("mouse-rs-quickid-serve");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let cache = Arc::new(BlobCache::new(root.join("cache")).unwrap());
        let mut t = Transfers::new(cache);
        let hist = ClipboardHistory::new();

        let src = root.join("film.mkv");
        fs::write(&src, vec![1u8; 4096]).unwrap();

        // hash_limit of 0 forces the quick-id path, as a large file would.
        let set = t
            .describe_local(&FileList { paths: vec![src.clone()] }, "laptop", 10, false, u64::MAX, 0)
            .expect("described");
        assert!(!set.files[0].content_hashed, "this must be the quick-id path");
        let hash = set.files[0].hash.clone();
        hist.record_files(Arc::new(set), false, ClipStatus::Ready);

        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (to_bulk, mut bulk_rx) = tokio::sync::mpsc::unbounded_channel();
        t.serve_request(&hash, 0, &hist, &to_peer, &to_bulk, &Arc::new(BulkQueue::new(1 << 20)));

        assert!(
            peer_rx.try_recv().is_err(),
            "it must not answer BlobUnavailable for a file that is exactly where it was"
        );
        let frames = drain_bulk(&mut bulk_rx);
        assert!(
            frames.iter().any(|f| matches!(f, BulkFrame::End { .. })),
            "the file should have been streamed in full: {frames:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// And the check must still catch a file that really did change underneath.
    #[test]
    fn a_quick_identified_file_that_changed_is_still_refused() {
        let root = std::env::temp_dir().join("mouse-rs-quickid-changed");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let cache = Arc::new(BlobCache::new(root.join("cache")).unwrap());
        let mut t = Transfers::new(cache);
        let hist = ClipboardHistory::new();

        let src = root.join("notes.bin");
        fs::write(&src, vec![1u8; 4096]).unwrap();
        let set = t
            .describe_local(&FileList { paths: vec![src.clone()] }, "laptop", 10, false, u64::MAX, 0)
            .expect("described");
        let hash = set.files[0].hash.clone();
        hist.record_files(Arc::new(set), false, ClipStatus::Ready);

        // A different length is a different file, whatever the timestamp says.
        fs::write(&src, vec![2u8; 8192]).unwrap();

        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (to_bulk, _bulk_rx) = tokio::sync::mpsc::unbounded_channel();
        t.serve_request(&hash, 0, &hist, &to_peer, &to_bulk, &Arc::new(BulkQueue::new(1 << 20)));
        assert!(
            matches!(peer_rx.try_recv(), Ok(Message::BlobUnavailable { .. })),
            "a changed file must not be sent as if it were the original"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
