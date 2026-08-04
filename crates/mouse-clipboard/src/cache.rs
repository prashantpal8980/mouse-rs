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

use sha2::{Digest, Sha256};

/// Read buffer for hashing and copying. Large enough to keep syscall count low, small
/// enough that it is irrelevant next to everything else the process is holding.
const IO_BUF: usize = 128 * 1024;

/// A content-addressed store for transferred file contents.
pub struct BlobCache {
    root: PathBuf,
}

impl BlobCache {
    /// Open (creating if needed) a cache under `root`.
    pub fn new(root: PathBuf) -> io::Result<BlobCache> {
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("names"))?;
        Ok(BlobCache { root })
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

    fn part_path(&self, hash: &str) -> PathBuf {
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

    /// Discard a partial transfer.
    pub fn abandon(&self, hash: &str) {
        if is_safe_hash(hash) {
            let _ = fs::remove_file(self.part_path(hash));
        }
    }

    /// Take a local file into the cache. Returns its hash and size.
    pub fn ingest(&self, src: &Path) -> io::Result<(String, u64)> {
        let (hash, size) = hash_file(src)?;
        if !self.contains(&hash) {
            let part = self.part_path(&hash);
            fs::copy(src, &part)?;
            fs::rename(&part, self.blob_path(&hash))?;
        }
        Ok((hash, size))
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

        let (ha, sa) = c.ingest(&a).unwrap();
        let (hb, sb) = c.ingest(&b).unwrap();
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
        let (hash, _) = c.ingest(&src).unwrap();
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
            let (h, _) = c.ingest(&p).unwrap();
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
