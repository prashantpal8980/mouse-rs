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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mouse_core::bulk::{BulkFrame, BulkQueue, CHUNK_BYTES};
use mouse_core::protocol::{FileRef, Message};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::cache::{self, BlobCache};
use crate::files::FileList;
use crate::{ClipStatus, ClipboardHistory, FileSet};

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
    file: std::fs::File,
    got: u64,
    total: u64,
    entry: Option<u64>,
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
    /// Hashes that are clipboard *images* rather than files, with their dimensions.
    /// They take the same transfer path but are applied to the clipboard as pixels
    /// rather than published as a path.
    pending_images: HashMap<String, (usize, usize)>,
    /// The hash of the most recently completed transfer, so an image can be applied
    /// without the caller having to track transfer ids.
    last_completed: Option<String>,
}

impl Transfers {
    pub fn new(cache: Arc<BlobCache>) -> Transfers {
        Transfers {
            cache,
            incoming: HashMap::new(),
            next_id: 0,
            requested: HashMap::new(),
            serving: HashMap::new(),
            pending_images: HashMap::new(),
            last_completed: None,
        }
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
                match self.cache.ingest(&member) {
                    Ok((hash, size)) => files.push(FileRef {
                        name: member
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "file".into()),
                        size,
                        hash,
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
        if set.files.iter().all(|f| self.cache.contains(&f.hash)) {
            return ClipStatus::Ready;
        }
        let total = set.total_bytes();
        if total <= auto_limit {
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
                            "over the ask limit, but quick enough on this link to just fetch"
                        );
                        return ClipStatus::Remote;
                    }
                }
            }
        }
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
    ) -> bool {
        let mut asked = false;
        for f in &set.files {
            if self.cache.contains(&f.hash) || self.requested.contains_key(&f.hash) {
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
        asked
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
            tracing::debug!(hash, "already streaming this file; ignoring the repeat request");
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
        // Not cached: fall back to the source path, but only if it still hashes to what
        // was advertised. A path that now holds *different* content is not the file the
        // peer asked for, and sending it anyway would silently paste the wrong version.
        let (path, name) = self.source_for(hash, history)?;
        let (actual, len) = cache::hash_file(&PathBuf::from(&path)).ok()?;
        if actual != hash {
            tracing::warn!(path = %path, "the file at the recorded path has changed since it was copied");
            return None;
        }
        Some((std::fs::File::open(&path).ok()?, len, name))
    }

    fn name_for(&self, hash: &str, history: &ClipboardHistory) -> Option<String> {
        history.file_ref(hash).map(|f| f.name)
    }

    fn source_for(&self, hash: &str, history: &ClipboardHistory) -> Option<(String, String)> {
        let f = history.file_ref(hash)?;
        Some((f.source_path?, f.name))
    }

    /// Handle one frame arriving on the bulk lane. Returns the entry id whose status
    /// changed, if any, so the caller can act on a completed set.
    pub fn on_bulk_frame(&mut self, frame: BulkFrame, history: &ClipboardHistory) -> Option<u64> {
        match frame {
            BulkFrame::Begin { transfer, total, meta } => {
                let meta: BlobMeta = serde_json::from_slice(&meta).ok()?;
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
                        Incoming { hash: meta.hash, file, got: 0, total, entry },
                    );
                    return None;
                }
                let entry = history.find_by_hash(&meta.hash);
                self.incoming
                    .insert(transfer, Incoming { hash: meta.hash, file, got, total, entry });
                self.next_id = self.next_id.wrapping_add(1);
                None
            }
            BulkFrame::Chunk { transfer, data, .. } => {
                let inc = self.incoming.get_mut(&transfer)?;
                if let Err(e) = inc.file.write_all(&data) {
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
                let (done, total, entry) = (inc.got, inc.total, inc.entry);
                if let Some(id) = entry {
                    history.set_status(id, ClipStatus::Fetching { done, total });
                }
                entry
            }
            BulkFrame::End { transfer } => {
                let inc = self.incoming.remove(&transfer)?;
                drop(inc.file);
                self.requested.remove(&inc.hash);
                self.last_completed = Some(inc.hash.clone());
                match self.cache.finish(&inc.hash) {
                    Ok(()) => {
                        tracing::info!(hash = %inc.hash, bytes = inc.got, "content received and verified");
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
        let id = history.find_by_hash(hash)?;
        history.set_status(id, ClipStatus::Unavailable(reason.to_string()));
        Some(id)
    }

    /// Do we hold every file in this set?
    pub fn is_complete(&self, set: &FileSet) -> bool {
        set.files.iter().all(|f| self.cache.contains(&f.hash))
    }

    /// Materialise a complete set under its real names, ready for the clipboard.
    ///
    /// A folder is rebuilt as a folder — pasting gives you the directory, not its
    /// contents scattered loose — and a plainly-copied file comes back as itself.
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
        self.cache.evict_to(budget, &history.pinned_hashes());
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

    fn test_queue() -> Arc<BulkQueue> {
        Arc::new(BulkQueue::new(8 * 1024 * 1024))
    }

    /// Collect frames until the transfer ends.
    ///
    /// Serving runs on its own thread so it can wait for the network without stalling
    /// the clipboard, which means a test can no longer assume the frames are already
    /// queued the moment `serve_request` returns.
    fn drain_bulk(
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
            .describe_local(&FileList { paths: vec![a.clone()] }, "kali", 10_000, false)
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
        let set = t.describe_local(&FileList { paths: vec![a] }, "kali", 10_000, false).unwrap();
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
            .describe_local(&FileList { paths: vec![src] }, "laptop", 10_000, false)
            .unwrap();
        s_hist.record_files(Arc::new(set.clone()), false, ClipStatus::Ready);

        // Receiver records it and asks for what it lacks.
        let entry = r_hist
            .record_files(Arc::new(set.clone()), true, ClipStatus::Remote)
            .unwrap();
        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(receiver.request_missing(&set, entry, &to_peer));
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
            .describe_local(&FileList { paths: vec![proj.clone()] }, "laptop", 10_000, false)
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
            .describe_local(&FileList { paths: vec![dir, loose] }, "laptop", 10_000, false)
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
            .describe_local(&FileList { paths: vec![dir] }, "laptop", 5, false)
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
            .describe_local(&FileList { paths: vec![src.clone()] }, "laptop", 10_000, false)
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
            .describe_local(&FileList { paths: vec![src.clone()] }, "laptop", 10_000, false)
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

    #[test]
    fn an_interrupted_transfer_resumes_instead_of_starting_over() {
        let (root, mut receiver, r_hist) = setup("resume");
        let hash = cache::hash_bytes(b"0123456789");
        let set = FileSet {
            files: vec![FileRef {
                name: "x.bin".into(),
                size: 10,
                hash: hash.clone(),
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
        receiver.request_missing(&set, entry, &to_peer);
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
            files: vec![FileRef { name: "f".into(), size: 16, hash: hash.clone(), source_path: None, rel: String::new() }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = r_hist
            .record_files(Arc::new(set), true, ClipStatus::Remote)
            .unwrap();

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
    fn asking_twice_only_downloads_once() {
        let (root, mut receiver, r_hist) = setup("twice");
        let set = FileSet {
            files: vec![FileRef {
                name: "f".into(),
                size: 10,
                hash: "c".repeat(64),
                source_path: None,
                rel: String::new(),
            }],
            dirs: Vec::new(),
            origin: "laptop".into(),
        };
        let entry = r_hist.record_files(Arc::new(set.clone()), true, ClipStatus::Remote).unwrap();
        let (to_peer, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(receiver.request_missing(&set, entry, &to_peer));
        assert!(peer_rx.try_recv().is_ok());
        assert!(
            !receiver.request_missing(&set, entry, &to_peer),
            "a second click must not start the same download again"
        );
        assert!(peer_rx.try_recv().is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_peer_saying_no_updates_the_entry() {
        let (root, mut receiver, r_hist) = setup("unavail");
        let hash = "d".repeat(64);
        let set = FileSet {
            files: vec![FileRef { name: "f".into(), size: 1, hash: hash.clone(), source_path: None, rel: String::new() }],
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
            .describe_local(&FileList { paths: vec![src] }, "laptop", 10_000, false)
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
            .describe_local(&FileList { paths: vec![src] }, "laptop", 10_000, false)
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
        assert!(receiver.request_missing(&set, entry, &to_peer));
        let _ = peer_rx.try_recv();

        assert!(receiver.cancel_entry(entry, &to_peer));
        match peer_rx.try_recv().unwrap() {
            Message::BlobCancel { hash: h } => assert_eq!(h, hash),
            other => panic!("expected BlobCancel, got {other:?}"),
        }
        // The request is forgotten, so clicking again really asks again rather than being
        // swallowed as a duplicate. That is what makes the entry usable after Stop.
        assert!(
            receiver.request_missing(&set, entry, &to_peer),
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
            .describe_local(&FileList { paths: vec![tree] }, "laptop", 10_000, false)
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
            .describe_local(&FileList { paths: vec![tree.clone()] }, "laptop", 10_000, false)
            .unwrap();
        assert_eq!(skipped.files.len(), 1, "the link must not be copied by default");

        let followed = sender
            .describe_local(&FileList { paths: vec![tree] }, "laptop", 10_000, true)
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
            .describe_local(&FileList { paths: vec![tree] }, "laptop", 10_000, true)
            .unwrap();
        assert_eq!(set.files.len(), 1, "the loop must be detected, not walked: {:?}", set.files);
        let _ = fs::remove_dir_all(&root);
    }
}
