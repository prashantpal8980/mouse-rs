//! Files still arriving from the peer, presented as an ordinary folder.
//!
//! A paste of a file we do not hold yet used to be answered by making the file manager
//! wait inside the clipboard request until every byte had arrived. Thunar gives up on a
//! clipboard request after about 30 seconds, so anything that took longer than that to
//! transfer could not be pasted at all: a 3.1 GB video arrived complete three minutes after
//! the paste had already failed with "nothing on the clipboard".
//!
//! So the clipboard instead names files in a small read-only FUSE filesystem. The paste
//! succeeds at once, and the file manager then copies from here with its own progress
//! dialog. Each read is answered from the transfer's partial file as it grows, and waits
//! (on a thread of its own, never blocking the filesystem) for bytes that have not arrived
//! yet. Opening a file is what starts the transfer, through the same [`PasteSignal`] the
//! clipboard owner raises.
//!
//! The last bytes of a file are held back until the cache has verified the whole file, so a
//! transfer that turns out to be corrupt fails the copy instead of leaving a bad file behind.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, LockOwner, MountOption,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};

use crate::cache::{self, BlobCache};
use crate::files::{FileList, PasteSignal};
use crate::pipe::StreamPipe;
use crate::FileSet;

/// How long attributes may be cached by the kernel. Nothing here changes once created.
const TTL: Duration = Duration::from_secs(60);
/// How often a waiting read looks for new bytes.
const POLL: Duration = Duration::from_millis(50);
/// A read gives up when the file has not grown for this long. Long enough to ride out a
/// reconnect; short enough that a dead transfer ends the copy with an error rather than a
/// progress dialog frozen forever.
const STALL: Duration = Duration::from_secs(180);
/// While waiting, re-ask for the file this often, so a transfer that dropped (a reconnect,
/// a cancelled fetch) is resumed rather than waited on.
const NUDGE: Duration = Duration::from_secs(10);
/// Copies kept visible. Older ones leave the folder; their inodes stay valid.
const KEEP_SETS: usize = 32;

#[derive(Clone)]
struct RemoteFile {
    hash: String,
    size: u64,
    signal: Arc<PasteSignal>,
    /// Large files stream through memory instead of the cache; see [`crate::pipe`].
    pipe: Option<Arc<StreamPipe>>,
}

enum Kind {
    Dir(Vec<(String, u64)>),
    File(RemoteFile),
}

struct Node {
    parent: u64,
    kind: Kind,
}

struct Tree {
    /// Index `i` is inode `i + 1`; inode 1 is the root.
    nodes: Vec<Node>,
    /// Entry id → its folder's inode, oldest first.
    sets: VecDeque<(u64, u64)>,
    by_entry: HashMap<u64, Vec<PathBuf>>,
}

impl Tree {
    fn node(&self, ino: u64) -> Option<&Node> {
        ino.checked_sub(1).and_then(|i| self.nodes.get(i as usize))
    }
    fn add(&mut self, parent: u64, name: &str, kind: Kind) -> u64 {
        self.nodes.push(Node { parent, kind });
        let ino = self.nodes.len() as u64;
        if let Some(Node { kind: Kind::Dir(children), .. }) = self.nodes.get_mut(parent as usize - 1) {
            children.push((name.to_string(), ino));
        }
        ino
    }
    fn child(&self, parent: u64, name: &str) -> Option<u64> {
        match self.node(parent).map(|n| &n.kind) {
            Some(Kind::Dir(children)) => children.iter().find(|(n, _)| n == name).map(|(_, i)| *i),
            _ => None,
        }
    }
    /// The directory `parent/name`, created if it does not exist yet.
    fn dir(&mut self, parent: u64, name: &str) -> u64 {
        self.child(parent, name)
            .unwrap_or_else(|| self.add(parent, name, Kind::Dir(Vec::new())))
    }
}

impl Tree {
    fn new() -> Tree {
        Tree {
            nodes: vec![Node { parent: 1, kind: Kind::Dir(Vec::new()) }],
            sets: VecDeque::new(),
            by_entry: HashMap::new(),
        }
    }

    /// Build `<root>/<entry>/…` for a copied set; returns its top-level paths.
    fn add_set(
        &mut self,
        root: &Path,
        entry: u64,
        set: &FileSet,
        signal: Arc<PasteSignal>,
        pipes: &HashMap<String, Arc<StreamPipe>>,
    ) -> Option<FileList> {
        if let Some(paths) = self.by_entry.get(&entry) {
            return Some(FileList { paths: paths.clone() });
        }
        let dir_name = entry.to_string();
        let set_ino = self.add(1, &dir_name, Kind::Dir(Vec::new()));
        let base = root.join(&dir_name);
        let mut tops: Vec<PathBuf> = Vec::new();
        let note_top = |first: &str, tops: &mut Vec<PathBuf>| {
            let p = base.join(first);
            if !tops.contains(&p) {
                tops.push(p);
            }
        };
        // Directories first, so an empty one still appears.
        for d in &set.dirs {
            let parts = cache::safe_components(d);
            let mut at = set_ino;
            for part in &parts {
                at = self.dir(at, part);
            }
            if let Some(first) = parts.first() {
                note_top(first, &mut tops);
            }
        }
        for f in &set.files {
            let rel = if f.rel.is_empty() { f.name.as_str() } else { f.rel.as_str() };
            let parts = cache::safe_components(rel);
            let Some((leaf, dirs)) = parts.split_last() else { continue };
            let mut at = set_ino;
            for part in dirs {
                at = self.dir(at, part);
            }
            if self.child(at, leaf).is_none() {
                self.add(
                    at,
                    leaf,
                    Kind::File(RemoteFile {
                        hash: f.hash.clone(),
                        size: f.size,
                        signal: signal.clone(),
                        pipe: pipes.get(&f.hash).cloned(),
                    }),
                );
            }
            note_top(&parts[0], &mut tops);
        }
        if tops.is_empty() {
            return None;
        }
        self.sets.push_back((entry, set_ino));
        self.by_entry.insert(entry, tops.clone());
        // Keep the folder short: drop the oldest copies from its listing.
        while self.sets.len() > KEEP_SETS {
            if let Some((old_entry, old_ino)) = self.sets.pop_front() {
                self.by_entry.remove(&old_entry);
                if let Some(Node { kind: Kind::Dir(children), .. }) = self.nodes.get_mut(0) {
                    children.retain(|(_, i)| *i != old_ino);
                }
            }
        }
        Some(FileList { paths: tops })
    }
}

/// One open file: which blob, and whether its last bytes have been handed over.
struct Open {
    hash: String,
    tail_served: Arc<std::sync::atomic::AtomicBool>,
    pipe: Option<Arc<StreamPipe>>,
}

struct Inner {
    cache: Arc<BlobCache>,
    tree: Mutex<Tree>,
    opens: Mutex<HashMap<u64, Open>>,
    next_fh: std::sync::atomic::AtomicU64,
    uid: u32,
    gid: u32,
    made: SystemTime,
}

impl Inner {
    fn attr(&self, ino: u64, node: &Node) -> FileAttr {
        let (kind, size, perm) = match &node.kind {
            Kind::Dir(_) => (FileType::Directory, 0, 0o555),
            Kind::File(f) => (FileType::RegularFile, f.size, 0o444),
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: self.made,
            mtime: self.made,
            ctime: self.made,
            crtime: self.made,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 128 * 1024,
        }
    }
}

/// The mounted folder. Unmounted when dropped.
pub struct StreamFs {
    root: PathBuf,
    inner: Arc<Inner>,
    _session: fuser::BackgroundSession,
}

impl StreamFs {
    /// Where the folder is mounted: under the runtime directory, which is per user, in
    /// memory, and cleared at logout.
    pub fn default_mountpoint() -> PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(d) if !d.is_empty() => PathBuf::from(d).join("mouse-rs").join("incoming"),
            _ => cache::BlobCache::default_root().with_file_name("incoming"),
        }
    }

    /// Mount it. `None` (logged) when FUSE is unavailable; the caller then falls back to
    /// making the paste wait for the transfer, as before.
    pub fn mount(cache: Arc<BlobCache>, at: &Path) -> Option<StreamFs> {
        // A previous run that died leaves a dead mount behind ("Transport endpoint is not
        // connected"); every later mount on the same path then fails. Clearing it is
        // harmless when nothing is mounted.
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(at)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if let Err(e) = std::fs::create_dir_all(at) {
            tracing::warn!(path = %at.display(), "streaming folder unavailable: cannot create the mount point: {e}");
            return None;
        }
        let meta = std::fs::metadata(at).ok()?;
        let inner = Arc::new(Inner {
            cache,
            tree: Mutex::new(Tree::new()),
            opens: Mutex::new(HashMap::new()),
            next_fh: std::sync::atomic::AtomicU64::new(1),
            uid: meta.uid(),
            gid: meta.gid(),
            made: SystemTime::now(),
        });
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::FSName("mouse-rs".into()),
            MountOption::Subtype("mouse-rs".into()),
            MountOption::RO,
            MountOption::NoExec,
            MountOption::NoSuid,
            MountOption::NoDev,
        ];
        config.n_threads = Some(4);
        match fuser::spawn_mount(Fs(inner.clone()), at, &config) {
            Ok(session) => {
                tracing::info!(path = %at.display(), "streaming folder mounted — large pastes start at once");
                Some(StreamFs { root: at.to_path_buf(), inner, _session: session })
            }
            Err(e) => {
                tracing::warn!(
                    path = %at.display(),
                    "streaming folder unavailable ({e}); a paste will wait for the whole transfer instead"
                );
                None
            }
        }
    }

    /// Present a copied set under `<mount>/<entry>/…` and return its top-level items, which
    /// is what goes on the clipboard. Presenting the same entry again returns the same paths.
    /// `pipes` are the files in the set that stream through memory rather than the cache.
    pub fn add_set(
        &self,
        entry: u64,
        set: &FileSet,
        signal: Arc<PasteSignal>,
        pipes: &HashMap<String, Arc<StreamPipe>>,
    ) -> Option<FileList> {
        let mut tree = self.inner.tree.lock().ok()?;
        tree.add_set(&self.root, entry, set, signal, pipes)
    }

    /// Whether a path lives in this folder.
    pub fn owns(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }
}

struct Fs(Arc<Inner>);

impl Filesystem for Fs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Ok(tree) = self.0.tree.lock() else { return reply.error(Errno::EIO) };
        let found = name.to_str().and_then(|n| tree.child(parent.0, n));
        match found.and_then(|ino| tree.node(ino).map(|n| (ino, n))) {
            Some((ino, node)) => reply.entry(&TTL, &self.0.attr(ino, node), fuser::Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Ok(tree) = self.0.tree.lock() else { return reply.error(Errno::EIO) };
        match tree.node(ino.0) {
            Some(node) => reply.attr(&TTL, &self.0.attr(ino.0, node)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Ok(tree) = self.0.tree.lock() else { return reply.error(Errno::EIO) };
        let Some(node) = tree.node(ino.0) else { return reply.error(Errno::ENOENT) };
        let Kind::Dir(children) = &node.kind else { return reply.error(Errno::ENOTDIR) };
        let mut entries: Vec<(u64, FileType, &str)> =
            vec![(ino.0, FileType::Directory, "."), (node.parent, FileType::Directory, "..")];
        for (name, child) in children {
            let kind = match tree.node(*child).map(|n| &n.kind) {
                Some(Kind::Dir(_)) => FileType::Directory,
                _ => FileType::RegularFile,
            };
            entries.push((*child, kind, name));
        }
        for (i, (child, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.0 & (libc_o_accmode()) != 0 {
            return reply.error(Errno::EROFS);
        }
        let file = match self.0.tree.lock() {
            Ok(tree) => match tree.node(ino.0).map(|n| &n.kind) {
                Some(Kind::File(f)) => f.clone(),
                Some(Kind::Dir(_)) => return reply.error(Errno::EISDIR),
                None => return reply.error(Errno::ENOENT),
            },
            Err(_) => return reply.error(Errno::EIO),
        };
        // Opening is the paste: start the transfer now, unless we already hold the file.
        // A piped file is fetched by its first read instead, from the offset it asks for.
        if file.pipe.is_none() && !self.0.cache.contains(&file.hash) {
            file.signal.failed.store(false, Ordering::SeqCst);
            file.signal.wanted.store(true, Ordering::SeqCst);
            tracing::info!(bytes = file.size, "streaming folder: a paste opened a file still on the peer; fetching");
        }
        let fh = self.0.next_fh.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut opens) = self.0.opens.lock() {
            opens.insert(
                fh,
                Open { hash: file.hash.clone(), tail_served: Default::default(), pipe: file.pipe.clone() },
            );
        }
        // Direct I/O: every read comes to us, so nothing stale is served from the page cache
        // for a file that is still filling in.
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        self.closed(fh.0);
        reply.ok();
    }

    #[allow(clippy::too_many_arguments)] // the trait's signature
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let file = match self.0.tree.lock() {
            Ok(tree) => match tree.node(ino.0).map(|n| &n.kind) {
                Some(Kind::File(f)) => f.clone(),
                _ => return reply.error(Errno::ENOENT),
            },
            Err(_) => return reply.error(Errno::EIO),
        };
        let cache = self.0.cache.clone();
        let tail_flag = self.0.opens.lock().ok().and_then(|o| o.get(&fh.0).map(|o| o.tail_served.clone()));
        // Answered from a thread of its own: a read waiting on the network must not hold
        // up every other request to this folder.
        std::thread::spawn(move || match read_file(&cache, &file, offset, size as u64) {
            Ok(bytes) => {
                if offset + bytes.len() as u64 >= file.size {
                    if let Some(flag) = tail_flag {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                reply.data(&bytes)
            }
            Err(e) => reply.error(e),
        });
    }
}

impl Fs {
    /// Called when a file is closed. After a complete copy-out the file lives where it was
    /// pasted, so our copy goes, unless something else still has the same file open.
    fn closed(&self, fh: u64) {
        let Ok(mut opens) = self.0.opens.lock() else { return };
        let Some(open) = opens.remove(&fh) else { return };
        let others = opens.values().any(|o| o.hash == open.hash);
        if let Some(pipe) = &open.pipe {
            if !others {
                pipe.release();
                tracing::info!(hash = %open.hash, "paste closed the streamed file; stream stopped and its memory freed");
            }
            return;
        }
        if !open.tail_served.load(Ordering::SeqCst) {
            return; // cancelled part-way: keep what arrived, so a retry resumes
        }
        if others {
            return;
        }
        drop(opens);
        self.0.cache.discard(&open.hash);
        tracing::info!(hash = %open.hash, "pasted copy complete; mouse-rs's own copy removed");
    }
}

/// `O_ACCMODE`: any write access asked for at open.
fn libc_o_accmode() -> i32 {
    0o3
}

/// A read, from the pipe for a streamed file, otherwise from the cache.
fn read_file(cache: &BlobCache, file: &RemoteFile, offset: u64, len: u64) -> Result<Vec<u8>, Errno> {
    match &file.pipe {
        Some(pipe) => pipe.read(offset, len, STALL).ok_or_else(|| {
            tracing::warn!(offset, size = file.size, "streaming folder: no data for {STALL:?}; failing the copy");
            Errno::EIO
        }),
        None => read_when_ready(cache, file, offset, len),
    }
}

/// The bytes `offset..offset+len` of `file`, waiting for them to arrive if need be.
fn read_when_ready(cache: &BlobCache, file: &RemoteFile, offset: u64, len: u64) -> Result<Vec<u8>, Errno> {
    if offset >= file.size || len == 0 {
        return Ok(Vec::new());
    }
    let end = (offset + len).min(file.size);
    // The tail is only served once the whole file is verified, so a corrupt transfer
    // fails the copy instead of completing it.
    let tail = end == file.size;
    let mut grew_at = Instant::now();
    let mut nudged_at = Instant::now();
    let mut seen = 0u64;
    loop {
        if cache.contains(&file.hash) {
            return read_range(&cache.blob_path(&file.hash), offset, end);
        }
        let have = cache.partial_len(&file.hash);
        if !tail && have >= end {
            // The partial can be renamed into place between the check and the read; then
            // the next pass reads the finished blob instead.
            if let Ok(bytes) = read_range(&cache.part_path(&file.hash), offset, end) {
                return Ok(bytes);
            }
            continue;
        }
        if file.signal.failed.load(Ordering::SeqCst) {
            tracing::warn!(bytes = file.size, "streaming folder: the transfer gave up; failing the copy");
            return Err(Errno::EIO);
        }
        if have != seen {
            seen = have;
            grew_at = Instant::now();
        }
        if grew_at.elapsed() > STALL {
            tracing::warn!(have, size = file.size, "streaming folder: no progress for {STALL:?}; failing the copy");
            return Err(Errno::EIO);
        }
        if nudged_at.elapsed() > NUDGE && grew_at.elapsed() > NUDGE {
            file.signal.wanted.store(true, Ordering::SeqCst);
            nudged_at = Instant::now();
        }
        std::thread::sleep(POLL);
    }
}

fn read_range(path: &Path, offset: u64, end: u64) -> Result<Vec<u8>, Errno> {
    let f = std::fs::File::open(path).map_err(|_| Errno::EIO)?;
    let mut buf = vec![0u8; (end - offset) as usize];
    f.read_exact_at(&mut buf, offset).map_err(|_| Errno::EIO)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mouse_core::protocol::FileRef;

    fn fref(name: &str, rel: &str, size: u64) -> FileRef {
        FileRef {
            name: name.into(),
            size,
            hash: cache::hash_bytes(name.as_bytes()),
            content_hashed: true,
            source_path: None,
            rel: rel.into(),
        }
    }

    #[test]
    fn a_read_waits_for_bytes_and_withholds_the_tail_until_verified() {
        let dir = std::env::temp_dir().join(format!("mouse-rs-sfs-{}", std::process::id()));
        let cache = Arc::new(BlobCache::new(dir.clone()).unwrap());
        let body = b"0123456789".to_vec();
        let hash = cache::hash_bytes(&body);
        let file = RemoteFile { hash: hash.clone(), size: 10, signal: Arc::new(PasteSignal::default()), pipe: None };

        // Half the file has arrived: the first half reads, and opening raised nothing yet.
        {
            use std::io::Write as _;
            let mut p = cache.open_partial(&hash).unwrap();
            p.write_all(&body[..6]).unwrap();
        }
        assert_eq!(read_when_ready(&cache, &file, 0, 4).unwrap(), b"0123");

        // The tail waits: finish the file from another thread and the read completes.
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
        assert_eq!(read_when_ready(&cache, &file, 6, 100).unwrap(), b"6789");
        t.join().unwrap();

        // A transfer marked failed ends a waiting read with an error.
        let missing = RemoteFile {
            hash: cache::hash_bytes(b"never"),
            size: 5,
            signal: Arc::new(PasteSignal::default()),
            pipe: None,
        };
        missing.signal.failed.store(true, Ordering::SeqCst);
        assert!(read_when_ready(&cache, &missing, 0, 5).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_copy_keeps_its_tree_and_names_only_its_top_items() {
        let mut tree = Tree::new();
        let set = FileSet {
            files: vec![
                fref("a.mkv", "", 3),
                fref("1.txt", "Album/1.txt", 1),
                fref("2.txt", "Album/Disc 2/2.txt", 2),
            ],
            dirs: vec!["Album".into(), "Album/Disc 2".into(), "Album/Empty".into()],
            origin: "laptop".into(),
        };
        let root = Path::new("/mnt");
        let list = tree.add_set(root, 7, &set, Arc::new(PasteSignal::default()), &HashMap::new()).unwrap();
        assert_eq!(list.paths, vec![PathBuf::from("/mnt/7/Album"), PathBuf::from("/mnt/7/a.mkv")]);
        let set_ino = tree.child(1, "7").unwrap();
        let album = tree.child(set_ino, "Album").unwrap();
        let disc = tree.child(album, "Disc 2").unwrap();
        assert!(tree.child(album, "Empty").is_some(), "an empty folder is kept");
        match tree.node(tree.child(disc, "2.txt").unwrap()).map(|n| &n.kind) {
            Some(Kind::File(f)) => assert_eq!(f.size, 2),
            _ => panic!("2.txt should be a file"),
        }
        // Presenting the same entry again gives the same paths and builds nothing new.
        let n = tree.nodes.len();
        assert_eq!(tree.add_set(root, 7, &set, Arc::new(PasteSignal::default()), &HashMap::new()).unwrap(), list);
        assert_eq!(tree.nodes.len(), n);
    }

    /// Mounts for real, so it needs FUSE: `cargo test -p mouse-clipboard -- --ignored mounted`.
    #[test]
    #[ignore]
    fn a_mounted_copy_completes_while_the_file_is_still_arriving() {
        use std::io::Write as _;
        let base = std::env::temp_dir().join(format!("mouse-rs-sfs-mount-{}", std::process::id()));
        let cache = Arc::new(BlobCache::new(base.join("cache")).unwrap());
        let at = base.join("mnt");
        let sfs = StreamFs::mount(cache.clone(), &at).expect("FUSE mount");

        let body: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        let hash = cache::hash_bytes(&body);
        let mut f = fref("video.mkv", "", body.len() as u64);
        f.hash = hash.clone();
        let set = FileSet { files: vec![f], dirs: vec![], origin: "peer".into() };
        let sig = Arc::new(PasteSignal::default());
        let list = sfs.add_set(1, &set, sig.clone(), &HashMap::new()).unwrap();

        // The "transfer": starts only once the copy has opened the file, then trickles in.
        let (c2, h2, b2, s2) = (cache.clone(), hash.clone(), body.clone(), sig.clone());
        let writer = std::thread::spawn(move || {
            while !s2.wanted.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let mut p = c2.open_partial(&h2).unwrap();
            for chunk in b2.chunks(512 * 1024) {
                p.write_all(chunk).unwrap();
                p.flush().unwrap();
                std::thread::sleep(Duration::from_millis(30));
            }
            drop(p);
            c2.finish(&h2).unwrap();
        });
        let dest = base.join("pasted.mkv");
        std::fs::copy(&list.paths[0], &dest).unwrap();
        writer.join().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        // The paste is the only copy left: no middle cache to clean up.
        let mut gone = false;
        for _ in 0..50 {
            if !cache.contains(&hash) && cache.partial_len(&hash) == 0 {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone, "mouse-rs's own copy should be removed once the paste has it");
        drop(sfs);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Mounts for real: `cargo test -p mouse-clipboard -- --ignored mounted`.
    #[test]
    #[ignore]
    fn mounted_piped_copy_survives_a_drop_and_writes_nothing_to_the_cache() {
        use crate::pipe::PipeCmd;
        let base = std::env::temp_dir().join(format!("mouse-rs-sfs-pipe-{}", std::process::id()));
        let cache = Arc::new(BlobCache::new(base.join("cache")).unwrap());
        let at = base.join("mnt");
        let sfs = StreamFs::mount(cache.clone(), &at).expect("FUSE mount");

        let body: Vec<u8> = (0..24 * 1024 * 1024u32).map(|i| (i % 253) as u8).collect();
        let mut f = fref("big.iso", "", body.len() as u64);
        f.content_hashed = false;
        let hash = f.hash.clone();
        let pipe = Arc::new(StreamPipe::new(body.len() as u64));
        let mut pipes = HashMap::new();
        pipes.insert(hash.clone(), pipe.clone());
        let set = FileSet { files: vec![f], dirs: vec![], origin: "peer".into() };
        let list = sfs.add_set(1, &set, Arc::new(PasteSignal::default()), &pipes).unwrap();

        // The peer, reduced to its essentials: answer each start from the asked offset.
        // The first stream "drops" after 9 MB, as a pulled cable would.
        let (p2, b2) = (pipe.clone(), body.clone());
        let peer = std::thread::spawn(move || {
            let mut starts = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline && !p2.complete() {
                if let PipeCmd::Start(off) = p2.take_command() {
                    starts.push(off);
                    assert!(p2.begin(off));
                    let stop_at = if starts.len() == 1 { 9 * 1024 * 1024 } else { b2.len() };
                    let mut pos = off as usize;
                    while pos < stop_at {
                        let end = (pos + 256 * 1024).min(stop_at);
                        p2.push(&b2[pos..end]);
                        pos = end;
                    }
                    p2.interrupted();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            starts
        });
        let dest = base.join("pasted.iso");
        std::fs::copy(&list.paths[0], &dest).unwrap();
        let starts = peer.join().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body, "byte-identical");
        assert_eq!(starts[0], 0);
        assert!(starts.len() >= 2, "the drop was resumed");
        assert!(starts[1] > 0, "resumed from where the paste got to, not from zero");
        assert_eq!(cache.partial_len(&hash), 0, "nothing spooled to disk");
        assert!(!cache.contains(&hash));
        drop(sfs);
        let _ = std::fs::remove_dir_all(&base);
    }
}
