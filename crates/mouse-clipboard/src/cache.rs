//! The blob cache: where transferred file contents live once they are ours.
//!
//! Files are stored under a hash of their **contents**, not their name. Three things
//! fall out of that, all of which the feature needs:
//!
//! * **A copied file survives its original.** Once the bytes are here, the entry keeps
//!   working after the source is deleted, renamed or moved on the other machine. A
//!   path-only reference dies the moment the file moves; this one is fragile only until
//!   its first use.
//! * **Deduplication is free.** The same ISO copied twice occupies disk once, and a
//!   file the peer already holds is not transferred at all — it answers "already have
//!   that hash" and a 100 MB copy finishes instantly.
//! * **Integrity is free.** The name *is* the checksum, so a truncated or corrupted
//!   transfer is detectable rather than silently pasted.
//!
//! Layout under the cache root:
//!
//! ```text
//! blobs/<hash>            the content, written once
//! blobs/<hash>.part       an in-progress transfer, promoted on verify
//! names/<hash>/<name>     the same content under its real filename
//! ```
//!
//! The `names/` view exists because a clipboard paste has to hand the OS a path whose
//! **filename is the one the user copied**. Pasting `a3f19c…` instead of `report.pdf`
//! would be technically correct and useless. It is a hard link where the filesystem
//! allows one, and a copy where it does not.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

/// Read buffer for hashing and copying. Large enough to keep syscall count low, small
/// enough that it is irrelevant next to everything else the process is holding.
const IO_BUF: usize = 128 * 1024;

/// A content-addressed store for transferred file contents.
pub struct BlobCache {
    root: PathBuf,
    /// Unix seconds when the over-budget notice was last written. The eviction pass runs
    /// on the tick loop, so an unreclaimable overrun would otherwise log ten times a
    /// second for as long as the condition lasts.
    over_budget_logged: AtomicU64,
}

impl BlobCache {
    /// Open (creating if needed) a cache under `root`.
    pub fn new(root: PathBuf) -> io::Result<BlobCache> {
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("names"))?;
        Ok(BlobCache { root, over_budget_logged: AtomicU64::new(0) })
    }

    /// True at most once a minute, so a standing overrun states itself without flooding.
    fn should_report_overrun(&self) -> bool {
        const QUIET_SECS: u64 = 60;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let last = self.over_budget_logged.load(Ordering::Relaxed);
        if now.saturating_sub(last) < QUIET_SECS {
            return false;
        }
        self.over_budget_logged.store(now, Ordering::Relaxed);
        true
    }

    /// The default location: alongside the config, not in the user's documents.
    pub fn default_root() -> PathBuf {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".mouse_share").join("cache")
    }

    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs").join(hash)
    }

    pub fn part_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs").join(format!("{hash}.part"))
    }

    /// Do we already hold this content?
    pub fn contains(&self, hash: &str) -> bool {
        is_safe_hash(hash) && self.blob_path(hash).is_file()
    }

    pub fn size_of(&self, hash: &str) -> Option<u64> {
        fs::metadata(self.blob_path(hash)).ok().map(|m| m.len())
    }

    /// How far a partially-received blob got, so a resumed transfer can pick up rather
    /// than start again. `0` if there is nothing to resume.
    pub fn partial_len(&self, hash: &str) -> u64 {
        if !is_safe_hash(hash) {
            return 0;
        }
        fs::metadata(self.part_path(hash)).map(|m| m.len()).unwrap_or(0)
    }

    /// Open the partial file for a transfer, appending if one is already underway.
    pub fn open_partial(&self, hash: &str) -> io::Result<fs::File> {
        if !is_safe_hash(hash) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsafe hash"));
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.part_path(hash))
    }

    /// Promote a completed partial into the cache, but **only** if its contents really
    /// hash to `hash`.
    ///
    /// This is the one place a transfer is trusted, so it is the one place that checks.
    /// Accepting on the sender's word would mean a corrupt transfer is pasted as if it
    /// were the file the user asked for.
    pub fn finish(&self, hash: &str) -> io::Result<()> {
        let part = self.part_path(hash);
        let (actual, _) = hash_file(&part)?;
        if actual != hash {
            let _ = fs::remove_file(&part);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transferred content does not match its hash",
            ));
        }
        fs::rename(&part, self.blob_path(hash))
    }

    /// Bytes this cache is using on disk, and how many blobs that is.
    ///
    /// Counted from the filesystem rather than tracked, because the truth is what is on
    /// the disk: a crash mid-transfer, an eviction, or a file removed by hand all make a
    /// running total wrong, and a wrong number here is worse than none — it is the figure
    /// someone decides whether to clear space by.
    pub fn usage(&self) -> (u64, usize) {
        // Counted per *inode*, not per name. `names/` and `trees/` are hard links to the
        // same content as `blobs/`, so adding up file sizes reported a 6.8 GB folder as
        // 9.2 GB in 48 files — inflating the one number someone would use to decide
        // whether to free space, in the alarming direction.
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        let mut bytes = 0u64;
        let mut count = 0usize;
        for dir in ["blobs", "names", "trees"] {
            walk_size(&self.root.join(dir), &mut bytes, &mut count, &mut seen);
        }
        (bytes, count)
    }

    /// Delete everything except the hashes named, and report what was freed.
    ///
    /// `keep` exists so clearing cannot destroy a pinned entry or one still arriving —
    /// the two cases where the bytes are not the program's to throw away.
    pub fn clear_except(&self, keep: &HashSet<String>) -> u64 {
        let before = self.usage().0;
        // Trees are rebuilt on demand from blobs, so they are always safe to drop.
        let _ = fs::remove_dir_all(self.root.join("trees"));
        if let Ok(entries) = fs::read_dir(self.root.join("blobs")) {
            for e in entries.flatten() {
                let path = e.path();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .trim_end_matches(".part")
                    .to_string();
                if keep.contains(&name) {
                    continue;
                }
                if fs::remove_file(&path).is_ok() {
                    let _ = fs::remove_dir_all(self.root.join("names").join(&name));
                }
            }
        }
        let after = self.usage().0;
        before.saturating_sub(after)
    }

    /// Put a received set into a folder the user chose, under real names.
    ///
    /// ## Why this exists, and why it costs nothing
    ///
    /// The complaint it answers: copying a 40 GB file used to mean 40 GB in the cache
    /// *plus* 40 GB wherever it was pasted, because the file manager copies from
    /// whatever path the clipboard hands it. Twice the disk for one file.
    ///
    /// A hard link removes that entirely. The blob and the delivered file are the same
    /// inode — one copy of the data with two names — so a 40 GB file delivered to the
    /// Desktop occupies 40 GB, not 80. Nothing has to be pasted at all: the file is
    /// already where it was wanted, by the time the transfer finishes.
    ///
    /// Eviction becomes free as well. Deleting the cached blob afterwards just drops one
    /// name; the delivered file keeps the data. So the disk budget can do its job without
    /// ever taking away something the user was given.
    ///
    /// Falls back to a copy only across filesystems, where the OS gives no other option —
    /// and says so, because that is the one case where the old cost returns.
    pub fn deliver(
        &self,
        dest_root: &Path,
        items: &[(String, String)],
        dirs: &[String],
    ) -> io::Result<Vec<PathBuf>> {
        fs::create_dir_all(dest_root)?;
        for rel in dirs {
            let parts = safe_components(rel);
            if parts.is_empty() {
                continue;
            }
            let mut d = dest_root.to_path_buf();
            for part in &parts {
                d.push(part);
            }
            fs::create_dir_all(&d)?;
        }

        let mut out = Vec::new();
        for (hash, rel) in items {
            let parts = safe_components(rel);
            if parts.is_empty() {
                continue;
            }
            let mut dest = dest_root.to_path_buf();
            for part in &parts {
                dest.push(part);
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            // Never overwrite something already there. A delivered file landing on top
            // of the user's own work would be a far worse bug than a suffixed name.
            let dest = unique_path(dest);
            let blob = self.blob_path(hash);
            match fs::hard_link(&blob, &dest) {
                Ok(()) => {}
                Err(_) => {
                    // Different filesystem, or a filesystem without links. This is the
                    // only path that costs the space twice, so it is worth saying.
                    tracing::info!(
                        dest = %dest.display(),
                        "delivering by copy rather than link — the destination is on a \
                         different filesystem, so this file briefly uses twice its size"
                    );
                    fs::copy(&blob, &dest)?;
                }
            }
            out.push(dest);
        }
        if out.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "nothing to deliver"));
        }
        Ok(out)
    }

    /// Where a materialised selection lives, so its final paths can be known before any
    /// of its content exists.
    pub fn tree_root(&self, key: &str) -> PathBuf {
        self.root.join("trees").join(key)
    }

    /// Discard a partial transfer.
    pub fn abandon(&self, hash: &str) {
        if is_safe_hash(hash) {
            let _ = fs::remove_file(self.part_path(hash));
        }
    }

    /// Take a local file into the cache. Returns its hash and size.
    ///
    /// `budget` is the disk this cache is allowed. A file larger than the whole budget
    /// is **hashed but not copied**: copying it would write gigabytes only for the very
    /// next eviction pass to delete them again for being over budget. That actually
    /// happened — a 3.3 GB copy against a 2 GB budget wrote 3.3 GB and kept none of it.
    ///
    /// Nothing is lost by skipping the copy. The cache is an optimisation here; serving
    /// falls back to the original path, re-verifying the hash first so a file that
    /// changed underneath is refused rather than sent as the old one.
    pub fn ingest(&self, src: &Path, budget: u64) -> io::Result<(String, u64)> {
        let (hash, size) = hash_file(src)?;
        if size > budget {
            tracing::info!(
                path = %src.display(),
                size,
                budget,
                "not caching this file — it is larger than the whole cache budget, so it \
                 would be evicted immediately. It will be read from its original path instead."
            );
            return Ok((hash, size));
        }
        if !self.contains(&hash) {
            let part = self.part_path(&hash);
            fs::copy(src, &part)?;
            fs::rename(&part, self.blob_path(&hash))?;
        }
        Ok((hash, size))
    }

    /// Identify a file for the wire, hashing its contents only when that is affordable.
    ///
    /// Returns `(id, size, content_hashed)`.
    ///
    /// Hashing reads every byte. Doing it before a copy can even be announced cost 65
    /// seconds on a 12.6 GB folder — the copy did not appear on the other machine for
    /// over a minute, which reads as the software being broken rather than being careful.
    ///
    /// So above `hash_limit` the identity is derived from the path, size and
    /// modification time. That is instant, and stable for the same file, so dedup and
    /// resume still work. What is given up is content verification, replaced on receipt
    /// by a length check — which still catches the realistic failure of a truncated or
    /// interrupted transfer, over a link that is authenticated and encrypted anyway.
    pub fn identify(&self, src: &Path, budget: u64, hash_limit: u64) -> io::Result<(String, u64, bool)> {
        let meta = fs::metadata(src)?;
        let size = meta.len();
        if size <= hash_limit {
            let (hash, size) = self.ingest(src, budget)?;
            return Ok((hash, size, true));
        }
        let id = quick_id(src, &meta);
        tracing::debug!(
            path = %src.display(),
            size,
            "identified without hashing: over the hash limit, so the copy is announced immediately"
        );
        Ok((id, size, false))
    }

    /// Promote a completed partial, verifying contents when the id is a content hash and
    /// length when it is not.
    ///
    /// The length check is not a formality. It is what catches the failure that actually
    /// happens — a transfer cut short — and without it a half-received film would be
    /// promoted and pasted as if it were whole.
    pub fn finish_checked(&self, hash: &str, content_hashed: bool, expect: u64) -> io::Result<()> {
        let part = self.part_path(hash);
        let got = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

        // **A short file is kept.** Whatever arrived is a valid prefix of the file, and
        // deleting it throws away everything already paid for: two 2 GB partials were
        // destroyed this way, so every retry began at offset 0 and the transfer appeared
        // to restart from nothing, repeatedly, with no error shown. Resuming is the whole
        // reason the partial exists.
        if got < expect {
            tracing::warn!(
                hash,
                got,
                expect,
                "the transfer ended early — keeping what arrived so a retry resumes from \
                 there rather than starting again"
            );
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("received {got} of {expect} bytes; kept for resume"),
            ));
        }
        if got > expect {
            // Longer than the file it claims to be: not a prefix of anything, so there is
            // nothing here worth resuming from.
            let _ = fs::remove_file(&part);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("received {got} bytes but the file is {expect}"),
            ));
        }
        if content_hashed {
            return self.finish(hash);
        }
        fs::rename(&part, self.blob_path(hash))
    }

    /// A path to this content **under its original filename**, ready to be put on the
    /// clipboard. Hard-linked when the filesystem allows it, copied when it does not.
    pub fn materialize(&self, hash: &str, name: &str) -> io::Result<PathBuf> {
        if !is_safe_hash(hash) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsafe hash"));
        }
        let name = sanitize_name(name);
        let dir = self.root.join("names").join(hash);
        fs::create_dir_all(&dir)?;
        let dest = dir.join(&name);
        if dest.exists() {
            return Ok(dest);
        }
        let blob = self.blob_path(hash);
        match fs::hard_link(&blob, &dest) {
            Ok(()) => Ok(dest),
            // Cross-device, unsupported, or a filesystem that refuses links: a copy is
            // slower and costs the space twice, but it is never wrong.
            Err(_) => {
                fs::copy(&blob, &dest)?;
                Ok(dest)
            }
        }
    }

    /// Rebuild a copied selection under a directory of its own, preserving the shape a
    /// folder had, and return the top-level paths to put on the clipboard.
    ///
    /// `items` is `(hash, relative path)`. A plain file has a one-component relative
    /// path and comes back as itself; a folder comes back as the folder, so pasting
    /// gives you the directory rather than a heap of loose files.
    ///
    /// Every relative path is rebuilt component by component from sanitised parts. It
    /// is peer-controlled data, and `../../.config/autostart/evil.desktop` taken
    /// literally is a write outside the cache — so no component is ever trusted.
    pub fn materialize_tree(
        &self,
        key: &str,
        items: &[(String, String)],
        dirs: &[String],
    ) -> io::Result<Vec<PathBuf>> {
        if !is_safe_hash(key) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsafe tree key"));
        }
        let root = self.root.join("trees").join(key);
        let mut tops: Vec<PathBuf> = Vec::new();
        // Directories first, and from the same sanitised-component rule as files —
        // a peer-supplied directory name is no more trustworthy than a file name.
        for rel in dirs {
            let parts = safe_components(rel);
            if parts.is_empty() {
                continue;
            }
            let mut dest = root.clone();
            for part in &parts {
                dest.push(part);
            }
            fs::create_dir_all(&dest)?;
            let top = root.join(&parts[0]);
            if !tops.contains(&top) {
                tops.push(top);
            }
        }
        for (hash, rel) in items {
            let parts = safe_components(rel);
            if parts.is_empty() {
                continue;
            }
            let mut dest = root.clone();
            for part in &parts {
                dest.push(part);
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if !dest.exists() {
                let blob = self.blob_path(hash);
                if fs::hard_link(&blob, &dest).is_err() {
                    fs::copy(&blob, &dest)?;
                }
            }
            let top = root.join(&parts[0]);
            if !tops.contains(&top) {
                tops.push(top);
            }
        }
        if tops.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "nothing to materialise"));
        }
        Ok(tops)
    }

    /// Total bytes held in blobs.
    pub fn total_bytes(&self) -> u64 {
        let Ok(entries) = fs::read_dir(self.root.join("blobs")) else {
            return 0;
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }

    /// Evict oldest-used blobs until the cache fits `budget`, never touching anything
    /// in `keep`.
    ///
    /// `keep` is how pinning survives here: an entry the user pinned must not lose its
    /// contents to a budget they also set, and quietly overrunning the budget is the
    /// smaller surprise of the two.
    pub fn evict_to(&self, budget: u64, keep: &HashSet<String>) {
        let Ok(entries) = fs::read_dir(self.root.join("blobs")) else {
            return;
        };
        let mut blobs: Vec<(PathBuf, u64, std::time::SystemTime)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                if !meta.is_file() {
                    return None;
                }
                let path = e.path();
                // Leave partials alone: they belong to a transfer in flight.
                if path.extension().is_some_and(|x| x == "part") {
                    return None;
                }
                let used = meta.accessed().or_else(|_| meta.modified()).ok()?;
                Some((path, meta.len(), used))
            })
            .collect();

        let mut total: u64 = blobs.iter().map(|(_, len, _)| *len).sum();
        if total <= budget {
            return;
        }
        // What is actually reclaimable: everything the history no longer references.
        // The rest belongs to entries the user can still see and click, and taking it
        // would make those entries lie about being ready.
        let reclaimable: u64 = blobs
            .iter()
            .filter(|(p, _, _)| {
                !keep.contains(p.file_name().and_then(|n| n.to_str()).unwrap_or_default())
            })
            .map(|(_, len, _)| *len)
            .sum();
        if total.saturating_sub(reclaimable) > budget && self.should_report_overrun() {
            tracing::info!(
                total,
                budget,
                reclaimable,
                "over the disk budget and cannot get under it without deleting files the \
                 history still needs. Keeping them: an entry that cannot deliver what it \
                 says is worse than being over budget. Remove entries, or press Clear, to \
                 free the rest."
            );
        }
        // Oldest first.
        blobs.sort_by_key(|(_, _, used)| *used);
        for (path, len, _) in blobs {
            if total <= budget {
                break;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            if keep.contains(&name) {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                let _ = fs::remove_dir_all(self.root.join("names").join(&name));
                total = total.saturating_sub(len);
                tracing::debug!(hash = %name, bytes = len, "evicted a cached blob");
            }
        }
    }
}

/// A file's identity without reading it: path, size and modification time.
///
/// Same shape as a content hash — 64 hex characters — so everything that validates or
/// stores an id keeps working unchanged. Stable for an unmodified file, which is what
/// makes dedup and resume survive without hashing gigabytes.
///
/// Exposed because the *serving* side has to recompute it. Checking a quick id against
/// a content hash can only ever fail, and when it did the peer was told the file was
/// "no longer at the recorded path" while it sat exactly where it always had.
pub fn quick_id(path: &Path, meta: &fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    hash_bytes(format!("{}|{}|{mtime}", path.to_string_lossy(), meta.len()).as_bytes())
}

/// Hash a file's contents, streaming. Returns the hex digest and the byte count.
///
/// Streaming rather than reading the file in: this runs against whatever the user
/// copied, which may be an ISO. Peak memory here has to be independent of file size or
/// the feature is a memory bomb dressed as a convenience.
pub fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; IO_BUF];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex(&hasher.finalize()), total))
}

/// Hash bytes already in memory (clipboard images take this path).
pub fn hash_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Is this string safe to use as a filename inside the cache?
///
/// Hashes arrive from the peer, and a "hash" of `../../.ssh/authorized_keys` would
/// otherwise be a path-traversal write primitive. Only lowercase hex of the right
/// length is accepted, which no traversal can survive.
fn is_safe_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Reduce a peer-supplied filename to something that can only ever land inside the
/// directory we chose.
///
/// The peer names the file, and a name is exactly the kind of field an attacker
/// controls: `../../autostart/evil.desktop` must become `evil.desktop`, not a write
/// outside the cache.
pub fn sanitize_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .trim_matches('.');
    let cleaned: String = base
        .chars()
        .filter(|c| !matches!(c, '\0' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .take(180)
        .collect();
    if cleaned.is_empty() {
        "clipboard-file".to_string()
    } else {
        cleaned
    }
}

/// Split a peer-supplied relative path into components that cannot escape their root.
///
/// `..` and empty/current-directory components are dropped rather than rejected, so a
/// slightly odd path still delivers its file somewhere sane instead of failing the
/// whole folder; every surviving component then goes through [`sanitize_name`], which
/// also strips drive letters and separators.
pub fn safe_components(rel: &str) -> Vec<String> {
    rel.split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .map(sanitize_name)
        .filter(|c| c != "clipboard-file" || !rel.is_empty())
        .collect()
}

/// Everything a walk found: the files to transfer, and the directories that hold them.
///
/// Directories are listed separately because an *empty* one has no file to imply it,
/// and rebuilding a folder without its empty subdirectories is not the folder that was
/// copied.
#[derive(Debug, Default)]
pub struct Walked {
    /// `(absolute path, path relative to the copied folder's parent)`.
    pub files: Vec<(PathBuf, String)>,
    /// Relative paths of every directory in the tree, in the same form.
    pub dirs: Vec<String>,
}

/// Walk a directory, returning every file inside it with its path relative to the
/// directory's *parent* — so the folder's own name is the first component and the copy
/// arrives with its shape intact.
///
/// `follow_symlinks` is the user's call, and off by default for two reasons: a link
/// pointing outside the copied tree silently pulls in unrelated files, and one pointing
/// back inside recurses. When it is on, directories already visited are remembered so a
/// cycle terminates instead of running until the file limit stops it.
pub fn walk_dir(root: &Path, max_files: usize, follow_symlinks: bool) -> io::Result<Walked> {
    let base = root.parent().unwrap_or(Path::new(""));
    let rel_of = |path: &Path| -> String {
        path.strip_prefix(base)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    };

    let mut out = Walked::default();
    out.dirs.push(rel_of(root));
    let mut stack = vec![root.to_path_buf()];
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    if follow_symlinks {
        if let Ok(c) = fs::canonicalize(root) {
            seen.insert(c);
        }
    }

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    // One unreadable file must not kill a 500-file copy.
                    tracing::warn!(path = %path.display(), "skipping: {e}");
                    continue;
                }
            };
            // `entry.metadata()` describes the link itself, so this is the point where
            // following or not following is decided.
            let meta = if meta.file_type().is_symlink() {
                if !follow_symlinks {
                    tracing::debug!(path = %path.display(), "skipping a symlink inside a copied folder");
                    continue;
                }
                match fs::metadata(&path) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), "skipping a broken symlink: {e}");
                        continue;
                    }
                }
            } else {
                meta
            };
            if meta.is_dir() {
                if follow_symlinks {
                    // Only a followed link can produce a cycle, so only pay for the
                    // canonicalize when following is on.
                    match fs::canonicalize(&path) {
                        Ok(c) => {
                            if !seen.insert(c) {
                                tracing::warn!(path = %path.display(), "not descending: this link loops back into the tree");
                                continue;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(path = %path.display(), "skipping: {e}");
                            continue;
                        }
                    }
                }
                out.dirs.push(rel_of(&path));
                stack.push(path);
                continue;
            }
            out.files.push((path, rel_of(&entry.path())));
            if out.files.len() >= max_files {
                tracing::warn!(
                    limit = max_files,
                    "the copied folder hit the file-count limit; the rest was not included"
                );
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// Copy `src` into `dst`, streaming, returning bytes written.
pub fn stream_copy(src: &mut impl Read, dst: &mut impl Write) -> io::Result<u64> {
    let mut buf = vec![0u8; IO_BUF];
    let mut total = 0u64;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        total += n as u64;
    }
    Ok(total)
}

/// `name.ext` → `name (2).ext` and upward, until nothing is in the way.
///
/// Silently replacing a file that is already there would be the worst possible failure
/// mode for a feature whose whole job is to put files on someone's desktop.
fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file").to_string();
    let ext = path.extension().and_then(|s| s.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    for n in 2..10_000 {
        let candidate = parent.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

/// Add up a directory tree's file sizes, counting each inode once and ignoring
/// anything unreadable.
///
/// `seen` is what makes the total *disk usage* rather than the sum of names: the same
/// content deliberately appears under several paths here, and counting it repeatedly
/// would overstate what freeing it would recover.
///
/// Errors are skipped rather than propagated: this feeds a display, and a permission
/// problem on one file should shrink the number slightly, not withhold it entirely.
// On Windows `seen` really is unused: there is no cheap inode to key on, so links are
// counted per name there. Kept in the signature rather than split into two functions,
// because the traversal is identical and the divergence is one `if`.
#[cfg_attr(windows, allow(clippy::only_used_in_recursion))]
fn walk_size(dir: &Path, bytes: &mut u64, count: &mut usize, seen: &mut HashSet<(u64, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            walk_size(&e.path(), bytes, count, seen);
        } else if meta.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if !seen.insert((meta.dev(), meta.ino())) {
                    continue; // another name for content already counted
                }
            }
            *bytes += meta.len();
            *count += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "mouse-rs-cache-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        base
    }

    #[test]
    fn ingesting_the_same_content_twice_stores_it_once() {
        let root = tmp();
        let c = BlobCache::new(root.clone()).unwrap();
        let a = root.join("a.bin");
        let b = root.join("b.bin");
        fs::write(&a, b"identical").unwrap();
        fs::write(&b, b"identical").unwrap();

        let (ha, sa) = c.ingest(&a, u64::MAX).unwrap();
        let (hb, sb) = c.ingest(&b, u64::MAX).unwrap();
        assert_eq!(ha, hb, "same bytes, same hash");
        assert_eq!((sa, sb), (9, 9));
        assert_eq!(c.total_bytes(), 9, "stored once, not twice");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn content_survives_the_original_being_deleted() {
        // The whole reason for keeping bytes rather than a path.
        let root = tmp();
        let c = BlobCache::new(root.clone()).unwrap();
        let src = root.join("gone-soon.txt");
        fs::write(&src, b"still here").unwrap();
        let (hash, _) = c.ingest(&src, u64::MAX).unwrap();
        fs::remove_file(&src).unwrap();

        assert!(c.contains(&hash));
        let out = c.materialize(&hash, "gone-soon.txt").unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"still here");
        assert_eq!(out.file_name().unwrap(), "gone-soon.txt", "pasted under its real name");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_transfer_that_does_not_match_its_hash_is_refused() {
        // Accepting on the sender's word would paste corrupted content as if it were
        // the file the user asked for.
        let root = tmp();
        let c = BlobCache::new(root.clone()).unwrap();
        let claimed = "0".repeat(64);
        {
            let mut f = c.open_partial(&claimed).unwrap();
            f.write_all(b"not what was promised").unwrap();
        }
        assert!(c.finish(&claimed).is_err());
        assert!(!c.contains(&claimed));
        assert_eq!(c.partial_len(&claimed), 0, "the bad partial is cleaned up");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_matching_transfer_is_promoted() {
        let root = tmp();
        let c = BlobCache::new(root.clone()).unwrap();
        let content = b"the real thing";
        let hash = hash_bytes(content);
        {
            let mut f = c.open_partial(&hash).unwrap();
            f.write_all(content).unwrap();
        }
        c.finish(&hash).unwrap();
        assert!(c.contains(&hash));
        assert_eq!(fs::read(c.blob_path(&hash)).unwrap(), content);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_partial_transfer_reports_how_far_it_got() {
        // Resume needs this: without it every dropped link restarts a 4 GB copy.
        let root = tmp();
        let c = BlobCache::new(root.clone()).unwrap();
        let hash = "a".repeat(64);
        assert_eq!(c.partial_len(&hash), 0);
        {
            let mut f = c.open_partial(&hash).unwrap();
            f.write_all(&[0u8; 500]).unwrap();
        }
        assert_eq!(c.partial_len(&hash), 500);
        // Re-opening appends rather than truncating, or resume would lose the prefix.
        {
            let mut f = c.open_partial(&hash).unwrap();
            f.write_all(&[0u8; 100]).unwrap();
        }
        assert_eq!(c.partial_len(&hash), 600);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn eviction_respects_the_budget_and_never_touches_pinned_content() {
        let root = tmp();
        let c = BlobCache::new(root.clone()).unwrap();
        let mut hashes = Vec::new();
        for i in 0..4u8 {
            let p = root.join(format!("f{i}.bin"));
            fs::write(&p, vec![i; 1000]).unwrap();
            let (h, _) = c.ingest(&p, u64::MAX).unwrap();
            hashes.push(h);
            // Distinct timestamps so "oldest" is well defined.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(c.total_bytes(), 4000);

        let mut keep = HashSet::new();
        keep.insert(hashes[0].clone()); // the oldest — and pinned
        c.evict_to(2000, &keep);

        assert!(c.contains(&hashes[0]), "a pinned blob must survive its own budget");
        assert!(c.contains(&hashes[3]), "the newest should survive");
        assert!(c.total_bytes() <= 3000, "and the cache should have shrunk");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hostile_hashes_and_names_cannot_escape_the_cache() {
        // Both fields arrive from the peer. Either one, taken literally, is a write
        // outside the directory we chose.
        let root = tmp();
        let c = BlobCache::new(root.clone()).unwrap();
        assert!(!c.contains("../../etc/passwd"));
        assert!(c.open_partial("../../etc/passwd").is_err());
        assert!(
            c.materialize("../../etc", "passwd").is_err(),
            "a traversal in the hash must be refused outright"
        );

        assert_eq!(sanitize_name("../../../evil.desktop"), "evil.desktop");
        assert_eq!(sanitize_name("C:\\Windows\\System32\\evil.dll"), "evil.dll");
        assert_eq!(sanitize_name("/etc/shadow"), "shadow");
        assert_eq!(sanitize_name(""), "clipboard-file");
        assert_eq!(sanitize_name("..."), "clipboard-file");
        assert!(!sanitize_name("a\0b:c").contains('\0'));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hashing_is_stable_and_size_reporting_is_right() {
        let root = tmp();
        fs::create_dir_all(&root).unwrap();
        let p = root.join("x.bin");
        fs::write(&p, b"abc").unwrap();
        let (h, n) = hash_file(&p).unwrap();
        assert_eq!(n, 3);
        // Known SHA-256 of "abc".
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hash_bytes(b"abc"), h, "file and in-memory hashing must agree");
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod deliver_tests {
    use super::*;

    /// The complaint this answers: a 40 GB file used to cost 40 GB in the cache plus
    /// 40 GB wherever it was pasted. Delivering by hard link makes it one copy under two
    /// names — same inode, same blocks — so the second name is free.
    #[test]
    fn a_delivered_file_shares_its_blocks_with_the_cached_one() {
        let root = std::env::temp_dir().join("mouse-rs-deliver-test");
        let _ = fs::remove_dir_all(&root);
        let c = BlobCache::new(root.join("cache")).unwrap();

        let src = root.join("episode.mkv");
        fs::create_dir_all(&root).unwrap();
        fs::write(&src, vec![9u8; 65_536]).unwrap();
        let (hash, _) = c.ingest(&src, u64::MAX).unwrap();

        let dest = root.join("Desktop");
        let out = c
            .deliver(&dest, &[(hash.clone(), "episode.mkv".into())], &[])
            .expect("delivered");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file_name().unwrap(), "episode.mkv");

        // Same inode = one copy of the data. This is the whole claim.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = fs::metadata(c.blob_path(&hash)).unwrap();
            let b = fs::metadata(&out[0]).unwrap();
            assert_eq!(a.ino(), b.ino(), "delivery must link, not copy");
            assert_eq!(b.nlink(), 2, "two names, one file");
        }

        // And the delivered file survives the cache being cleared — dropping one name
        // does not touch the data while another name still holds it.
        c.clear_except(&HashSet::new());
        assert!(out[0].is_file(), "clearing the cache must not take the delivered file");
        assert_eq!(fs::read(&out[0]).unwrap().len(), 65_536);
        let _ = fs::remove_dir_all(&root);
    }

    /// Landing on top of the user's own file would be far worse than an awkward name.
    #[test]
    fn delivery_never_overwrites_something_already_there() {
        let root = std::env::temp_dir().join("mouse-rs-deliver-clash");
        let _ = fs::remove_dir_all(&root);
        let c = BlobCache::new(root.join("cache")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let src = root.join("notes.txt");
        fs::write(&src, b"from the peer").unwrap();
        let (hash, _) = c.ingest(&src, u64::MAX).unwrap();

        let dest = root.join("Desktop");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("notes.txt"), b"MINE - do not lose this").unwrap();

        let out = c.deliver(&dest, &[(hash, "notes.txt".into())], &[]).unwrap();
        assert_eq!(out[0].file_name().unwrap(), "notes (2).txt");
        assert_eq!(fs::read(dest.join("notes.txt")).unwrap(), b"MINE - do not lose this");
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(all(test, unix))]
mod move_paste_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// What a "move" paste actually does, and why it is free: the file manager renames
    /// the link we published. No bytes move, the destination is wherever the user
    /// pasted, and the cache still holds the data afterwards.
    #[test]
    fn a_moved_paste_relocates_the_link_and_leaves_the_data_in_the_cache() {
        let root = std::env::temp_dir().join("mouse-rs-movepaste");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let c = BlobCache::new(root.join("cache")).unwrap();

        let src = root.join("big.iso");
        fs::write(&src, vec![4u8; 131_072]).unwrap();
        let (hash, _) = c.ingest(&src, u64::MAX).unwrap();
        let published = c.materialize(&hash, "big.iso").unwrap();
        let blob = c.blob_path(&hash);
        assert_eq!(fs::metadata(&published).unwrap().ino(), fs::metadata(&blob).unwrap().ino());

        // The file manager pasting with "cut" is exactly this rename.
        let downloads = root.join("Downloads");
        fs::create_dir_all(&downloads).unwrap();
        let landed = downloads.join("big.iso");
        fs::rename(&published, &landed).unwrap();

        assert!(landed.is_file(), "the file is where it was pasted");
        assert_eq!(fs::read(&landed).unwrap().len(), 131_072);
        assert!(blob.is_file(), "and the cache still has the data for another paste");
        assert_eq!(
            fs::metadata(&landed).unwrap().ino(),
            fs::metadata(&blob).unwrap().ino(),
            "same inode: the paste moved a name, not 128 KB of data"
        );

        // A second paste needs the link back. This is what `Publish::relink` rebuilds,
        // and it must not disturb the file already delivered by the first paste.
        assert!(!published.exists(), "the first paste consumed the published link");
        fs::create_dir_all(published.parent().unwrap()).unwrap();
        fs::hard_link(&blob, &published).unwrap();
        assert!(published.is_file(), "relinked, so pasting again works");
        assert!(landed.is_file(), "and the first paste is untouched");
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    /// Two 2 GB partials were deleted because the transfer ended short, so every retry
    /// began at offset 0 and an 11 GB copy appeared to restart from nothing, repeatedly,
    /// with nothing shown to explain it. What arrived is a valid prefix; throwing it away
    /// discards exactly the work the partial exists to preserve.
    #[test]
    fn a_transfer_that_ends_early_keeps_what_arrived() {
        let root = std::env::temp_dir().join("mouse-rs-resume-short");
        let _ = fs::remove_dir_all(&root);
        let c = BlobCache::new(root.join("cache")).unwrap();
        let hash = "a".repeat(64);

        let mut f = c.open_partial(&hash).unwrap();
        f.write_all(&vec![7u8; 4096]).unwrap();
        drop(f);

        // Claimed to be 8192 bytes; only 4096 arrived.
        let err = c.finish_checked(&hash, false, 8192).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            c.partial_len(&hash),
            4096,
            "the prefix must survive, or the retry starts from zero"
        );
        assert!(!c.contains(&hash), "and it must not be promoted as if complete");

        // The retry resumes: the rest arrives and it completes.
        let mut f = c.open_partial(&hash).unwrap();
        f.write_all(&vec![7u8; 4096]).unwrap();
        drop(f);
        c.finish_checked(&hash, false, 8192).unwrap();
        assert!(c.contains(&hash));
        assert_eq!(c.size_of(&hash), Some(8192));
        let _ = fs::remove_dir_all(&root);
    }

    /// Longer than it claims is not a prefix of anything, so there is nothing to resume
    /// from and keeping it would only waste the space.
    #[test]
    fn a_partial_longer_than_the_file_is_discarded() {
        let root = std::env::temp_dir().join("mouse-rs-resume-long");
        let _ = fs::remove_dir_all(&root);
        let c = BlobCache::new(root.join("cache")).unwrap();
        let hash = "b".repeat(64);
        let mut f = c.open_partial(&hash).unwrap();
        f.write_all(&vec![7u8; 9000]).unwrap();
        drop(f);
        assert!(c.finish_checked(&hash, false, 8192).is_err());
        assert_eq!(c.partial_len(&hash), 0, "an over-long partial is not worth keeping");
        let _ = fs::remove_dir_all(&root);
    }
}
