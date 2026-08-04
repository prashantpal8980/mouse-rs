//! Clipboard sync for mouse-rs.
//!
//! A dedicated native thread polls the local clipboard and applies the peer's,
//! talking to the async link over channels — the same "blocking backend on its own
//! thread, async core untouched" shape the input backends use. Sync is loop-free
//! via a single last-content guard: whatever we last applied-from-peer or
//! broadcast-from-local is remembered, so re-reading it never echoes back.
//!
//! Text and images. Images travel as base64 PNG (`Message::ClipboardImage`), encoded
//! from and decoded to the RGBA8 that both OS clipboards speak. They are polled less
//! often than text and capped in both dimensions and encoded size, because reading an
//! image off the clipboard is orders of magnitude more expensive than reading text and
//! this process is also carrying live mouse and keyboard input.

pub mod cache;
pub mod files;
pub mod transfer;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use mouse_core::protocol::Message;

/// Default number of entries kept. Matches Windows' own clipboard history; it is only
/// the starting value for [`HistoryLimits::max_items`], which the user can change.
const DEFAULT_HISTORY_ITEMS: usize = 25;
/// Default ceiling on a single entry we will carry. Covers text and encoded images
/// alike, so "biggest item copied automatically" is one number rather than two.
const DEFAULT_MAX_ITEM_BYTES: usize = 5 * 1024 * 1024;
/// Default ceiling on what the whole history may hold in memory at once.
const DEFAULT_MEMORY_BUDGET: usize = 64 * 1024 * 1024;
/// Default largest accepted image edge, in pixels.
const DEFAULT_MAX_IMAGE_PX: usize = 4096;
/// How much of an entry's text is kept for display. The preview is cloned on every
/// panel redraw, so it is deliberately short and built once when the entry is recorded.
const PREVIEW_CHARS: usize = 72;

/// The user-settable bounds on the history. Every field is a deliberate trade the
/// owner is allowed to make; nothing here is a hard limit imposed by the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryLimits {
    /// How many entries to keep before the oldest unpinned one is dropped.
    pub max_items: usize,
    /// Largest single entry that will be stored at all.
    pub max_item_bytes: usize,
    /// Total memory the history may hold. Oldest unpinned entries go first.
    pub memory_budget: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        HistoryLimits {
            max_items: DEFAULT_HISTORY_ITEMS,
            max_item_bytes: DEFAULT_MAX_ITEM_BYTES,
            memory_budget: DEFAULT_MEMORY_BUDGET,
        }
    }
}

/// What a history entry actually holds — the content itself, not a description of it.
///
/// Payloads are `Arc`ed because the panel snapshots the history on every redraw and
/// this is the *only* copy of the item: a per-frame deep clone would cost megabytes
/// many times a second.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipPayload {
    Text(Arc<String>),
    /// Stored as encoded PNG rather than raw pixels. That is already the form that
    /// arrives from the peer and the form we encode to send, so keeping it costs
    /// nothing extra — and a screenshot is roughly 20× smaller than the RGBA it
    /// decodes to, which is the same reason Windows' own history stores PNG.
    Image {
        png: Arc<Vec<u8>>,
        w: usize,
        h: usize,
    },
    /// One or more files. The *contents* are not here — they live in the blob cache,
    /// addressed by hash. This entry costs a few hundred bytes whether it describes a
    /// text file or a 4 GB ISO, which is what lets a big copy appear in the history
    /// instantly and transfer later (or never).
    Files(Arc<FileSet>),
}

pub use mouse_core::protocol::FileRef;

/// The file list behind one history entry.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FileSet {
    pub files: Vec<FileRef>,
    /// Name of the machine the copy happened on, for "on laptop · click to fetch".
    pub origin: String,
}

impl FileSet {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// Where an entry's content actually is, from the user's point of view.
///
/// Shown verbatim in the panel. Every variant exists because it is a state the user can
/// really be in and would otherwise have to guess at — most of all `SourceGone`, which
/// is what a pure path reference turns into the moment the file moves.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ClipStatus {
    /// We hold the content. Pasting is instant and needs no network.
    #[default]
    Ready,
    /// Metadata only; the bytes are still on the peer.
    Remote,
    /// Over the user's auto-transfer limit, waiting for them to say yes.
    NeedsConsent,
    /// Transfer in progress.
    Fetching { done: u64, total: u64 },
    /// The peer looked and could not produce it. Carries the reason to show.
    ///
    /// The wording matters: without indexing the whole disk we cannot tell a delete
    /// from a rename, so the message says "no longer at the recorded path" rather than
    /// claiming one of them.
    Unavailable(String),
}

impl ClipStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, ClipStatus::Ready)
    }
}

impl ClipPayload {
    /// What this entry costs against the memory budget.
    pub fn bytes(&self) -> usize {
        match self {
            ClipPayload::Text(t) => t.len(),
            ClipPayload::Image { png, .. } => png.len(),
            // A file entry costs only its description in memory; the contents live on
            // disk in the blob cache under their own budget.
            ClipPayload::Files(set) => set.files.len() * std::mem::size_of::<FileRef>(),
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, ClipPayload::Image { .. })
    }

    pub fn is_files(&self) -> bool {
        matches!(self, ClipPayload::Files(_))
    }

    /// Bytes on disk this entry refers to — what it costs the *cache* budget, as
    /// opposed to the memory budget `bytes` reports.
    pub fn disk_bytes(&self) -> u64 {
        match self {
            ClipPayload::Files(set) => set.total_bytes(),
            _ => 0,
        }
    }

    /// The short, display-ready line shown in the panel.
    fn preview(&self) -> String {
        match self {
            ClipPayload::Text(t) => {
                let one_line: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
                if one_line.chars().count() > PREVIEW_CHARS {
                    format!("{}…", one_line.chars().take(PREVIEW_CHARS).collect::<String>())
                } else {
                    one_line
                }
            }
            ClipPayload::Image { png, w, h } => {
                format!("image {}×{}, {} KB", w, h, png.len().div_ceil(1024))
            }
            ClipPayload::Files(set) => match set.files.as_slice() {
                [] => "no files".to_string(),
                [one] => one.name.clone(),
                many => format!("{} files — {}, …", many.len(), many[0].name),
            },
        }
    }
}

/// One remembered clipboard item, with everything needed to put it back.
#[derive(Clone, Debug)]
pub struct ClipEntry {
    /// Stable identity, so the panel can name an entry across redraws and reordering.
    pub id: u64,
    /// True if it arrived from the peer, false if it was copied on this machine.
    pub incoming: bool,
    /// Pinned entries are never evicted by either budget.
    pub pinned: bool,
    pub preview: String,
    pub payload: ClipPayload,
    /// Where the content is right now. Only ever `Ready` for text and images, which
    /// carry their content inline; file entries move through the other states.
    pub status: ClipStatus,
}

/// The display-only view the panel works with. Deliberately carries no payload: the
/// panel never needs the bytes, only a label and an id to act on, and keeping the
/// bytes out of the render path means a 5 MB image can never be cloned per frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipMeta {
    pub id: u64,
    pub incoming: bool,
    pub pinned: bool,
    pub is_image: bool,
    pub is_files: bool,
    pub preview: String,
    pub bytes: usize,
    pub status: ClipStatus,
    /// Total size on disk for a file entry, so the panel can say "104 MB" without
    /// touching the payload.
    pub disk_bytes: u64,
}

#[derive(Default)]
struct HistoryInner {
    entries: VecDeque<ClipEntry>,
    next_id: u64,
    limits: HistoryLimits,
}

/// A small, shared, newest-first list of recent clipboard items. Cheap to clone (an
/// `Arc`); the sync thread records into it and the panel reads and acts on it.
///
/// This is a clipboard *manager*, not a log: entries keep their content so any of them
/// can be put back on the system clipboard later.
#[derive(Clone, Default)]
pub struct ClipboardHistory {
    inner: Arc<Mutex<HistoryInner>>,
}

impl ClipboardHistory {
    pub fn new() -> ClipboardHistory {
        ClipboardHistory::default()
    }

    pub fn with_limits(limits: HistoryLimits) -> ClipboardHistory {
        let h = ClipboardHistory::default();
        h.set_limits(limits);
        h
    }

    pub fn set_limits(&self, limits: HistoryLimits) {
        if let Ok(mut g) = self.inner.lock() {
            g.limits = limits;
            trim(&mut g.entries, limits);
        }
    }

    pub fn limits(&self) -> HistoryLimits {
        self.inner.lock().map(|g| g.limits).unwrap_or_default()
    }

    /// Record text. Returns the new entry's id, or `None` if it was rejected.
    pub fn record_text(&self, text: Arc<String>, incoming: bool) -> Option<u64> {
        self.record(ClipPayload::Text(text), incoming)
    }

    /// Record a file set. `status` says whether we already hold the contents.
    pub fn record_files(&self, set: Arc<FileSet>, incoming: bool, status: ClipStatus) -> Option<u64> {
        let id = self.record(ClipPayload::Files(set), incoming)?;
        self.set_status(id, status);
        Some(id)
    }

    /// Update where an entry's content is. Returns false if the entry has gone.
    pub fn set_status(&self, id: u64, status: ClipStatus) -> bool {
        let Ok(mut g) = self.inner.lock() else { return false };
        match g.entries.iter_mut().find(|e| e.id == id) {
            Some(e) => {
                e.status = status;
                true
            }
            None => false,
        }
    }

    pub fn status(&self, id: u64) -> Option<ClipStatus> {
        let g = self.inner.lock().ok()?;
        g.entries.iter().find(|e| e.id == id).map(|e| e.status.clone())
    }

    /// The entry currently describing this file set, if any — so an arriving transfer
    /// can find the row it belongs to.
    pub fn find_by_hash(&self, hash: &str) -> Option<u64> {
        let g = self.inner.lock().ok()?;
        g.entries
            .iter()
            .find(|e| match &e.payload {
                ClipPayload::Files(set) => set.files.iter().any(|f| f.hash == hash),
                _ => false,
            })
            .map(|e| e.id)
    }

    /// The description of a file we hold a record of, by hash — so a request can be
    /// answered with the right name, and re-read from the right path.
    pub fn file_ref(&self, hash: &str) -> Option<FileRef> {
        let g = self.inner.lock().ok()?;
        g.entries.iter().find_map(|e| match &e.payload {
            ClipPayload::Files(set) => set.files.iter().find(|f| f.hash == hash).cloned(),
            _ => None,
        })
    }

    /// Every hash referenced by a **pinned** entry. Passed to the cache so pinning
    /// protects file contents from the disk budget, not just the row from the list.
    pub fn pinned_hashes(&self) -> std::collections::HashSet<String> {
        let Ok(g) = self.inner.lock() else { return Default::default() };
        g.entries
            .iter()
            .filter(|e| e.pinned)
            .filter_map(|e| match &e.payload {
                ClipPayload::Files(set) => Some(set.files.iter().map(|f| f.hash.clone())),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// Record an image, keeping the encoded PNG so it can be pasted again later.
    pub fn record_image(&self, png: Arc<Vec<u8>>, w: usize, h: usize, incoming: bool) -> Option<u64> {
        self.record(ClipPayload::Image { png, w, h }, incoming)
    }

    /// Record a payload at the front. An item we already hold is *moved* to the front
    /// rather than duplicated — the clipboard-manager behaviour, and what stops the
    /// same text appearing twice merely because it travelled in both directions.
    fn record(&self, payload: ClipPayload, incoming: bool) -> Option<u64> {
        let mut g = self.inner.lock().ok()?;
        if payload.bytes() > g.limits.max_item_bytes {
            tracing::debug!(
                bytes = payload.bytes(),
                cap = g.limits.max_item_bytes,
                "clipboard item is over the per-item limit; not kept in history"
            );
            return None;
        }
        if let Some(pos) = g.entries.iter().position(|e| e.payload == payload) {
            let mut existing = g.entries.remove(pos)?;
            existing.incoming = incoming;
            let id = existing.id;
            g.entries.push_front(existing);
            return Some(id);
        }
        g.next_id += 1;
        let id = g.next_id;
        let preview = payload.preview();
        g.entries.push_front(ClipEntry {
            id,
            incoming,
            pinned: false,
            preview,
            payload,
            status: ClipStatus::Ready,
        });
        let limits = g.limits;
        trim(&mut g.entries, limits);
        Some(id)
    }

    /// Newest-first display list for the panel.
    pub fn meta(&self) -> Vec<ClipMeta> {
        self.inner
            .lock()
            .map(|g| {
                g.entries
                    .iter()
                    .map(|e| ClipMeta {
                        id: e.id,
                        incoming: e.incoming,
                        pinned: e.pinned,
                        is_image: e.payload.is_image(),
                        is_files: e.payload.is_files(),
                        preview: e.preview.clone(),
                        bytes: e.payload.bytes(),
                        status: e.status.clone(),
                        disk_bytes: e.payload.disk_bytes(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// How much the history is currently holding, for the panel to show the cost of
    /// the user's own settings rather than leave them to find out later.
    pub fn usage(&self) -> (usize, usize) {
        self.inner
            .lock()
            .map(|g| (g.entries.len(), g.entries.iter().map(|e| e.payload.bytes()).sum()))
            .unwrap_or((0, 0))
    }

    /// The content of an entry, for putting it back on the clipboard.
    pub fn payload(&self, id: u64) -> Option<ClipPayload> {
        let g = self.inner.lock().ok()?;
        g.entries.iter().find(|e| e.id == id).map(|e| e.payload.clone())
    }

    /// Move an entry to the front — what re-using it should do, matching how Windows'
    /// history treats an item you pick.
    pub fn promote(&self, id: u64) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(pos) = g.entries.iter().position(|e| e.id == id) {
                if let Some(e) = g.entries.remove(pos) {
                    g.entries.push_front(e);
                }
            }
        }
    }

    /// Toggle pinning. Returns the new state, or `None` if there is no such entry.
    pub fn toggle_pin(&self, id: u64) -> Option<bool> {
        let mut g = self.inner.lock().ok()?;
        let e = g.entries.iter_mut().find(|e| e.id == id)?;
        e.pinned = !e.pinned;
        Some(e.pinned)
    }

    pub fn remove(&self, id: u64) {
        if let Ok(mut g) = self.inner.lock() {
            g.entries.retain(|e| e.id != id);
        }
    }

    /// Drop every entry the user has not pinned. Pinning exists precisely so that
    /// "clear" does not take something you meant to keep.
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.entries.retain(|e| e.pinned);
        }
    }
}

/// Enforce both budgets, oldest-unpinned first.
///
/// If every remaining entry is pinned we stop rather than evicting one: the user
/// asked for those to stay, and quietly overrunning a budget they set is a smaller
/// surprise than losing something they pinned.
fn trim(entries: &mut VecDeque<ClipEntry>, limits: HistoryLimits) {
    while entries.len() > limits.max_items {
        match entries.iter().rposition(|e| !e.pinned) {
            Some(pos) => {
                entries.remove(pos);
            }
            None => break,
        }
    }
    let mut total: usize = entries.iter().map(|e| e.payload.bytes()).sum();
    while total > limits.memory_budget {
        match entries.iter().rposition(|e| !e.pinned) {
            Some(pos) => match entries.remove(pos) {
                Some(e) => total -= e.payload.bytes(),
                None => break,
            },
            None => break,
        }
    }
}

/// What the panel can ask the clipboard thread to do. Only re-use has to reach the
/// thread — pinning, deleting and clearing touch the shared history directly, because
/// they never talk to the OS clipboard.
enum ClipCommand {
    Reuse(u64),
}

/// The panel's handle for asking that a history entry be put back on the clipboard.
#[derive(Clone)]
pub struct ClipboardCommander {
    tx: std::sync::mpsc::Sender<ClipCommand>,
}

impl ClipboardCommander {
    /// Put entry `id` back on the system clipboard (and send it to the peer, so
    /// re-using an old entry syncs exactly like a fresh copy).
    pub fn reuse(&self, id: u64) {
        let _ = self.tx.send(ClipCommand::Reuse(id));
    }
}

/// The receiving half, handed to the clipboard thread.
pub struct ClipCommandRx {
    rx: Receiver<ClipCommand>,
}

/// Build the panel → clipboard-thread command channel.
pub fn command_channel() -> (ClipboardCommander, ClipCommandRx) {
    let (tx, rx) = std::sync::mpsc::channel();
    (ClipboardCommander { tx }, ClipCommandRx { rx })
}

/// The bulk lane, as the clipboard thread sees it: somewhere to send file chunks, and
/// somewhere to receive them. Both ends are owned by the network layer.
pub struct BulkLink {
    pub to_bulk: UnboundedSender<mouse_core::bulk::BulkFrame>,
    pub from_bulk: Receiver<mouse_core::bulk::BulkFrame>,
    /// Where transferred contents are kept.
    pub cache: Arc<cache::BlobCache>,
}

/// The clipboard-thread half of the user's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardSettings {
    pub sync_text: bool,
    pub sync_images: bool,
    /// Largest text or encoded image we will send or store.
    pub max_item_bytes: usize,
    /// Largest accepted image edge, in pixels.
    pub max_image_px: usize,
    /// Copy files as well as text and images.
    pub sync_files: bool,
    /// Transfer a file set this size or smaller without asking. Above it, the entry
    /// waits for the user. `u64::MAX` means never ask.
    pub auto_transfer_max: u64,
    /// Disk the blob cache may use.
    pub cache_budget: u64,
    /// This machine's name, shown on the other side as where a copy came from.
    pub machine: &'static str,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        ClipboardSettings {
            sync_text: true,
            sync_images: true,
            max_item_bytes: DEFAULT_MAX_ITEM_BYTES,
            max_image_px: DEFAULT_MAX_IMAGE_PX,
            sync_files: true,
            auto_transfer_max: 1024 * 1024,
            cache_budget: 2 * 1024 * 1024 * 1024,
            machine: "this machine",
        }
    }
}

/// The loop's wait quantum. Short enough that a panel command (“put this entry back on
/// the clipboard”) is acted on while the click still feels connected to the result;
/// long enough that an idle machine is doing nothing ten times a second. Received peer
/// messages still wake the loop immediately — this is only the *upper* bound on a wait.
const WAIT_TICK: Duration = Duration::from_millis(100);

/// Poll the local clipboard for text every Nth tick — 500 ms, as before.
const TEXT_POLL_TICKS: u32 = 5;

/// Poll for an *image* only every Nth tick. Reading an image means the OS hands over
/// (and we copy) a full uncompressed bitmap; doing that twice a second forever would
/// burn measurable CPU in a process that must never be slow. Text stays on the fast
/// tick because reading it is trivially cheap.
const IMAGE_POLL_TICKS: u32 = 20; // 20 × 100 ms = every 2 s, as before

/// An RGBA8 image — the common currency between the OS clipboard and the PNG codec.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RgbaImage {
    w: usize,
    h: usize,
    bytes: Vec<u8>,
}

/// A minimal clipboard backend, abstracted so the sync logic is testable without a
/// real OS clipboard.
trait SystemClipboard {
    /// Current clipboard text, or `None` if empty / not text / unavailable.
    fn read_text(&mut self) -> Option<String>;
    /// Set the clipboard text; returns whether it succeeded.
    fn write_text(&mut self, text: &str) -> bool;
    /// Current clipboard image as RGBA8, or `None` if the clipboard holds no image.
    fn read_image(&mut self) -> Option<RgbaImage>;
    /// Set the clipboard image; returns whether it succeeded.
    fn write_image(&mut self, img: &RgbaImage) -> bool;
}

impl SystemClipboard for arboard::Clipboard {
    fn read_text(&mut self) -> Option<String> {
        self.get_text().ok()
    }
    fn write_text(&mut self, text: &str) -> bool {
        self.set_text(text.to_owned()).is_ok()
    }
    fn read_image(&mut self) -> Option<RgbaImage> {
        let img = self.get_image().ok()?;
        Some(RgbaImage {
            w: img.width,
            h: img.height,
            bytes: img.bytes.into_owned(),
        })
    }
    fn write_image(&mut self, img: &RgbaImage) -> bool {
        self.set_image(arboard::ImageData {
            width: img.w,
            height: img.h,
            bytes: std::borrow::Cow::Borrowed(&img.bytes),
        })
        .is_ok()
    }
}

/// A cheap fingerprint of a clipboard image, used only to answer "is this the same
/// image I last sent or applied?" — the loop-free guard that `last` provides for text.
///
/// Deliberately samples rather than hashing every byte: this runs on a poll and the
/// buffers are tens of megabytes. A collision would mean a genuinely new image is
/// mistaken for the current one and not synced — benign and recoverable by re-copying,
/// which is why sampling is an acceptable trade for keeping the poll O(1). Dimensions
/// and length are included, so only images agreeing on size *and* every sampled window
/// can collide.
fn image_key(img: &RgbaImage) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    img.w.hash(&mut h);
    img.h.hash(&mut h);
    img.bytes.len().hash(&mut h);
    const WINDOW: usize = 4096;
    let n = img.bytes.len();
    for start in [0, n / 3, 2 * n / 3, n.saturating_sub(WINDOW)] {
        let end = (start + WINDOW).min(n);
        if start < end {
            img.bytes[start..end].hash(&mut h);
        }
    }
    h.finish()
}

/// Is this image within the user's size limit?
///
/// Checked per-edge *and* on the total, because the total is what actually gets
/// allocated: at 4 bytes a pixel, an image the user's limit allows on both edges is
/// still `max_px² × 4` bytes of RGBA. Both checks run before any decode or encode, so
/// a corrupt or hostile peer message can never talk us into a huge allocation.
fn within_image_limit(w: usize, h: usize, max_px: usize) -> bool {
    w > 0 && h > 0 && w <= max_px && h <= max_px && w.saturating_mul(h) <= max_px.saturating_mul(max_px)
}

/// Encode RGBA8 as PNG. `None` if the image is outside the caps or the encoder fails.
fn encode_png(img: &RgbaImage, max_px: usize) -> Option<Vec<u8>> {
    if !within_image_limit(img.w, img.h, max_px) {
        return None;
    }
    if img.bytes.len() != img.w.checked_mul(img.h)?.checked_mul(4)? {
        return None; // not the RGBA8 we were promised
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.w as u32, img.h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&img.bytes).ok()?;
    }
    Some(out)
}

/// Decode a PNG to RGBA8. Accepts the colour types a peer might plausibly send, not
/// just the RGBA we ourselves write. `None` on anything malformed or oversized — this
/// parses data that arrived over the network, so it must never panic or over-allocate.
fn decode_png(data: &[u8], max_px: usize) -> Option<RgbaImage> {
    let mut dec = png::Decoder::new(data);
    dec.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::EXPAND);
    let mut reader = dec.read_info().ok()?;
    let (w, h) = {
        let info = reader.info();
        (info.width as usize, info.height as usize)
    };
    if !within_image_limit(w, h, max_px) {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).ok()?;
    let src = &buf[..frame.buffer_size()];
    let bytes: Vec<u8> = match frame.color_type {
        png::ColorType::Rgba => src.to_vec(),
        png::ColorType::Rgb => src.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::Grayscale => src.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::GrayscaleAlpha => {
            src.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect()
        }
        _ => return None, // indexed should have been expanded; anything else is unexpected
    };
    if bytes.len() != w.checked_mul(h)?.checked_mul(4)? {
        return None;
    }
    Some(RgbaImage { w, h, bytes })
}

/// Owns the clipboard-sync thread. Dropping or calling [`ClipboardSync::stop`]
/// tears it down (bounded, so a wedged clipboard backend can't hang Stop).
pub struct ClipboardSync {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ClipboardSync {
    /// Spawn the sync thread. `to_peer` carries local clipboard changes to the peer;
    /// `remote_rx` delivers the `Clipboard` messages the engine received from the peer;
    /// `commands` delivers the panel's re-use requests; `connected` (set by the engine)
    /// gates outgoing broadcasts so we never queue unboundedly while disconnected nor
    /// flush a stale backlog on reconnect.
    pub fn spawn(
        to_peer: UnboundedSender<Message>,
        remote_rx: Receiver<Message>,
        commands: Option<ClipCommandRx>,
        connected: Arc<AtomicBool>,
        history: Option<ClipboardHistory>,
        settings: ClipboardSettings,
        bulk: Option<BulkLink>,
    ) -> ClipboardSync {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let join = std::thread::Builder::new()
            .name("clipboard-sync".into())
            .spawn(move || run(to_peer, remote_rx, commands, st, connected, history, settings, bulk))
            .ok();
        if join.is_none() {
            tracing::warn!("could not spawn clipboard-sync thread; clipboard sync disabled");
        }
        ClipboardSync { stop, join }
    }

    /// Stop the thread, bounded so a wedged clipboard backend can't hang the caller.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            join_bounded(j, Duration::from_millis(800));
        }
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    to_peer: UnboundedSender<Message>,
    remote_rx: Receiver<Message>,
    commands: Option<ClipCommandRx>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    history: Option<ClipboardHistory>,
    settings: ClipboardSettings,
    bulk: Option<BulkLink>,
) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("clipboard unavailable; sync disabled: {e}");
            // Drain the channel so the engine's sends don't pile up unboundedly.
            while remote_rx.recv().is_ok() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
            }
            return;
        }
    };

    // Seed with the current content so we don't broadcast the pre-existing clipboard
    // on startup. `last` is whatever we most recently applied-from-peer or
    // broadcast-from-local — the anchor that makes the sync loop-free. `last_img` is
    // the same anchor for images, kept as a fingerprint rather than the bitmap itself.
    let mut last = clipboard.read_text().filter(|t| !t.is_empty());
    let mut last_img = clipboard.read_image().as_ref().map(image_key);
    // Same anchor for file lists. Seeded from whatever is already on the clipboard so a
    // pre-existing selection isn't broadcast the moment sharing starts.
    let mut last_files = if settings.sync_files {
        files::read_files().as_ref().map(files::list_key)
    } else {
        None
    };
    let (mut transfers, to_bulk, from_bulk) = match bulk {
        Some(b) => (
            Some(transfer::Transfers::new(b.cache)),
            Some(b.to_bulk),
            Some(b.from_bulk),
        ),
        None => (None, None, None),
    };
    let mut tick: u32 = 0;

    loop {
        // Apply a received clipboard update if one arrived; this recv is also the
        // loop's wait point (wakes on a message or every WAIT_TICK).
        match remote_rx.recv_timeout(WAIT_TICK) {
            Ok(Message::Clipboard { text }) if settings.sync_text => {
                let text = Arc::new(text);
                if apply_remote(&mut clipboard, &mut last, &text) {
                    tracing::info!(bytes = text.len(), "clipboard text received from the peer");
                    if let Some(h) = &history {
                        h.record_text(text, true);
                    }
                }
            }
            Ok(Message::ClipboardImage { data, w, h }) if settings.sync_images => {
                if let Some(got) =
                    apply_remote_image(&mut clipboard, &mut last_img, &data, w, h, settings)
                {
                    tracing::info!(
                        w = got.w,
                        h = got.h,
                        bytes = got.png.len(),
                        "clipboard image received from the peer"
                    );
                    if let Some(hist) = &history {
                        hist.record_image(got.png, got.w, got.h, true);
                    }
                }
            }
            // Files copied on the other machine: record the description, then decide
            // whether to fetch now, ask first, or do nothing because we already have it.
            Ok(Message::ClipboardFiles { files: refs, origin }) if settings.sync_files => {
                if let (Some(t), Some(h)) = (transfers.as_mut(), history.as_ref()) {
                    let set = Arc::new(FileSet { files: refs, origin });
                    let status = t.initial_status(&set, settings.auto_transfer_max);
                    tracing::info!(
                        count = set.files.len(),
                        bytes = set.total_bytes(),
                        ?status,
                        "the peer copied files"
                    );
                    if let Some(id) = h.record_files(set.clone(), true, status.clone()) {
                        if status == ClipStatus::Remote {
                            t.request_missing(&set, id, &to_peer);
                        }
                    }
                }
            }
            // The peer wants a file we advertised.
            Ok(Message::BlobRequest { hash, offset }) => {
                if let (Some(t), Some(h), Some(tb)) =
                    (transfers.as_ref(), history.as_ref(), to_bulk.as_ref())
                {
                    t.serve_request(&hash, offset, h, &to_peer, tb);
                }
            }
            Ok(Message::BlobUnavailable { hash, reason }) => {
                if let (Some(t), Some(h)) = (transfers.as_mut(), history.as_ref()) {
                    tracing::warn!(hash, "the peer cannot send a file: {reason}");
                    t.on_unavailable(&hash, &reason, h);
                }
            }
            // A stray message for this thread, or a kind the user has switched off.
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break, // engine dropped the sender
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Panel requests are drained every tick so re-using an entry feels immediate.
        // They are handled whether or not a peer is connected: putting an item back on
        // *this* machine's clipboard is useful on its own.
        if let Some(cmds) = &commands {
            while let Ok(ClipCommand::Reuse(id)) = cmds.rx.try_recv() {
                if let Some(h) = &history {
                    reuse_entry(
                        &mut clipboard,
                        &mut last,
                        &mut last_img,
                        h,
                        &to_peer,
                        connected.load(Ordering::SeqCst),
                        id,
                        settings,
                        transfers.as_mut(),
                        &mut last_files,
                    );
                }
            }
        }
        // Drain whatever the bulk lane delivered. A completed set is published to the
        // clipboard here, on the one thread allowed to touch it.
        if let (Some(fb), Some(t), Some(h)) = (from_bulk.as_ref(), transfers.as_mut(), history.as_ref())
        {
            while let Ok(frame) = fb.try_recv() {
                if let Some(entry) = t.on_bulk_frame(frame, h) {
                    if let Some(ClipPayload::Files(set)) = h.payload(entry) {
                        if t.is_complete(&set) {
                            h.set_status(entry, ClipStatus::Ready);
                            if let Some(list) = t.materialize(&set) {
                                if files::write_files(&list) {
                                    last_files = Some(files::list_key(&list));
                                    tracing::info!(
                                        count = set.files.len(),
                                        "files received and put on the clipboard"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            t.enforce_disk_budget(settings.cache_budget, h);
        }
        tick = tick.wrapping_add(1);
        // Only broadcast local changes while a peer is connected: this avoids
        // queueing unboundedly while disconnected, and avoids flushing a stale
        // backlog (clipboard copied while disconnected) to whoever connects next. On
        // reconnect the first poll naturally syncs the current clipboard once.
        if connected.load(Ordering::SeqCst) {
            if settings.sync_text && tick % TEXT_POLL_TICKS == 0 {
                if let Some(sent) = poll_local(&mut clipboard, &mut last, &to_peer, settings) {
                    tracing::info!(bytes = sent.len(), "clipboard text sent to the peer");
                    if let Some(h) = &history {
                        h.record_text(sent, false);
                    }
                }
            }
            // Files on the same slow tick as images: reading the list is cheap, but
            // hashing what it points at is not, so it must not run twice a second.
            if settings.sync_files && tick % IMAGE_POLL_TICKS == 0 {
                if let (Some(t), Some(h)) = (transfers.as_ref(), history.as_ref()) {
                    if let Some(list) = files::read_files() {
                        let key = files::list_key(&list);
                        if last_files != Some(key) {
                            last_files = Some(key);
                            if let Some(set) = t.describe_local(&list, settings.machine) {
                                tracing::info!(
                                    count = set.files.len(),
                                    bytes = set.total_bytes(),
                                    "files copied here; telling the peer"
                                );
                                let _ = to_peer.send(Message::ClipboardFiles {
                                    files: set.files.clone(),
                                    origin: set.origin.clone(),
                                });
                                h.record_files(Arc::new(set), false, ClipStatus::Ready);
                            }
                        }
                    }
                }
            }
            // Images on the slow tick — see `IMAGE_POLL_TICKS`. When the clipboard holds
            // text rather than an image the read fails fast, so the expensive path only
            // runs when there is actually an image to copy.
            if settings.sync_images && tick % IMAGE_POLL_TICKS == 0 {
                if let Some(sent) = poll_local_image(&mut clipboard, &mut last_img, &to_peer, settings)
                {
                    tracing::info!(
                        w = sent.w,
                        h = sent.h,
                        bytes = sent.png.len(),
                        "clipboard image sent to the peer"
                    );
                    if let Some(h) = &history {
                        h.record_image(sent.png, sent.w, sent.h, false);
                    }
                }
            }
        }
    }
}

/// Put a history entry back on the system clipboard, and send it to the peer so that
/// re-using an old entry behaves exactly like copying it afresh.
///
/// `last`/`last_img` are updated to the value we just wrote. That is what stops the
/// next local poll from seeing our own write as a new user copy and sending it twice.
#[allow(clippy::too_many_arguments)]
fn reuse_entry<C: SystemClipboard>(
    clipboard: &mut C,
    last: &mut Option<String>,
    last_img: &mut Option<u64>,
    history: &ClipboardHistory,
    to_peer: &UnboundedSender<Message>,
    connected: bool,
    id: u64,
    settings: ClipboardSettings,
    transfers: Option<&mut transfer::Transfers>,
    last_files: &mut Option<u64>,
) -> bool {
    let Some(payload) = history.payload(id) else {
        tracing::debug!(id, "re-use asked for a clipboard entry that no longer exists");
        return false;
    };
    match payload {
        ClipPayload::Text(text) => {
            if !clipboard.write_text(&text) {
                tracing::warn!(id, "could not put the history entry back on the clipboard");
                return false;
            }
            *last = Some((*text).clone());
            if connected {
                let _ = to_peer.send(Message::Clipboard {
                    text: (*text).clone(),
                });
            }
        }
        ClipPayload::Image { png, .. } => {
            let Some(img) = decode_png(&png, settings.max_image_px) else {
                tracing::warn!(id, "stored clipboard image could not be decoded");
                return false;
            };
            if !clipboard.write_image(&img) {
                tracing::warn!(id, "could not put the history image back on the clipboard");
                return false;
            }
            *last_img = Some(image_key(&img));
            if connected {
                use base64::Engine as _;
                let _ = to_peer.send(Message::ClipboardImage {
                    data: base64::engine::general_purpose::STANDARD.encode(png.as_slice()),
                    w: img.w as i32,
                    h: img.h as i32,
                });
            }
        }
        ClipPayload::Files(set) => {
            let Some(t) = transfers else {
                tracing::warn!(id, "file entries need the blob cache, which is unavailable");
                return false;
            };
            // Already ours: publish it. Otherwise this click *is* the consent, so ask
            // for what is missing and let the arriving transfer publish it later.
            if t.is_complete(&set) {
                let Some(list) = t.materialize(&set) else { return false };
                if !files::write_files(&list) {
                    tracing::warn!(id, "could not put the files on the clipboard");
                    return false;
                }
                *last_files = Some(files::list_key(&list));
                history.set_status(id, ClipStatus::Ready);
                if connected {
                    let _ = to_peer.send(Message::ClipboardFiles {
                        files: set.files.clone(),
                        origin: set.origin.clone(),
                    });
                }
            } else if connected {
                history.set_status(
                    id,
                    ClipStatus::Fetching { done: 0, total: set.total_bytes() },
                );
                if !t.request_missing(&set, id, to_peer) {
                    // Nothing new to ask for: a request is already in flight.
                    tracing::debug!(id, "a transfer for this entry is already running");
                }
                return true; // the click did something; publishing happens on arrival
            } else {
                history.set_status(
                    id,
                    ClipStatus::Unavailable("needs the other machine connected".into()),
                );
                return false;
            }
        }
    }
    history.promote(id);
    tracing::info!(id, "clipboard entry re-used from history");
    true
}

/// Apply a clipboard value received from the peer, unless it's what we already have
/// (which would mean it originated here and came back — don't reapply). Returns true
/// if a new value was actually written (so the caller can record it in history).
fn apply_remote<C: SystemClipboard>(clipboard: &mut C, last: &mut Option<String>, text: &Arc<String>) -> bool {
    if last.as_deref() == Some(text.as_str()) {
        return false;
    }
    if clipboard.write_text(text) {
        *last = Some((**text).clone());
        true
    } else {
        tracing::debug!("clipboard set failed");
        false
    }
}

/// Forward a local clipboard change to the peer, unless it's what we last set/sent
/// (which would echo a value the peer already has). Returns the forwarded text (so the
/// caller can record it in history), or `None` if nothing was sent.
fn poll_local<C: SystemClipboard>(
    clipboard: &mut C,
    last: &mut Option<String>,
    to_peer: &UnboundedSender<Message>,
    settings: ClipboardSettings,
) -> Option<Arc<String>> {
    let text = clipboard.read_text()?; // empty or non-text (e.g. an image): nothing to forward
    if text.is_empty() || last.as_deref() == Some(text.as_str()) {
        return None;
    }
    if text.len() > settings.max_item_bytes {
        tracing::debug!(
            bytes = text.len(),
            cap = settings.max_item_bytes,
            "clipboard text is over the per-item limit; not syncing"
        );
        *last = Some(text); // remember it so we don't re-check it every poll
        return None;
    }
    *last = Some(text.clone());
    let _ = to_peer.send(Message::Clipboard { text });
    // `last` now holds exactly what we sent, so re-wrap that rather than cloning twice.
    last.as_ref().map(|t| Arc::new(t.clone()))
}

/// An image that crossed the link, in the form the history stores it: the encoded PNG
/// plus the dimensions it decodes to.
struct SyncedImage {
    png: Arc<Vec<u8>>,
    w: usize,
    h: usize,
}

/// Apply an image received from the peer. Returns the stored form if a new image was
/// actually written, `None` if it was rejected or is what we already have.
///
/// `w`/`h` come off the wire alongside the PNG; they are treated as a claim to be
/// checked against what the PNG actually decodes to, not as trusted dimensions.
fn apply_remote_image<C: SystemClipboard>(
    clipboard: &mut C,
    last_img: &mut Option<u64>,
    data: &str,
    w: i32,
    h: i32,
    settings: ClipboardSettings,
) -> Option<SyncedImage> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    if raw.len() > settings.max_item_bytes {
        tracing::debug!(
            bytes = raw.len(),
            cap = settings.max_item_bytes,
            "received clipboard image is over the per-item limit; ignoring"
        );
        return None;
    }
    let img = decode_png(&raw, settings.max_image_px).or_else(|| {
        tracing::debug!("received clipboard image could not be decoded; ignoring");
        None
    })?;
    if w > 0 && h > 0 && (img.w != w as usize || img.h != h as usize) {
        tracing::debug!(
            "clipboard image dimensions {}×{} disagree with the declared {w}×{h}; ignoring",
            img.w,
            img.h
        );
        return None;
    }
    let key = image_key(&img);
    if *last_img == Some(key) {
        return None; // originated here and came back
    }
    if !clipboard.write_image(&img) {
        tracing::debug!("clipboard image set failed");
        return None;
    }
    *last_img = Some(key);
    Some(SyncedImage {
        png: Arc::new(raw),
        w: img.w,
        h: img.h,
    })
}

/// Forward a local clipboard image to the peer, unless it's what we last set/sent.
/// Returns the stored form if something was sent.
fn poll_local_image<C: SystemClipboard>(
    clipboard: &mut C,
    last_img: &mut Option<u64>,
    to_peer: &UnboundedSender<Message>,
    settings: ClipboardSettings,
) -> Option<SyncedImage> {
    use base64::Engine as _;
    let img = clipboard.read_image()?; // no image on the clipboard: nothing to do
    let key = image_key(&img);
    if *last_img == Some(key) {
        return None;
    }
    // Remember it either way: a rejected image must not be re-encoded every poll.
    *last_img = Some(key);
    let png = encode_png(&img, settings.max_image_px)?;
    if png.len() > settings.max_item_bytes {
        tracing::debug!(
            bytes = png.len(),
            cap = settings.max_item_bytes,
            "local clipboard image is over the per-item limit; not syncing"
        );
        return None;
    }
    let _ = to_peer.send(Message::ClipboardImage {
        data: base64::engine::general_purpose::STANDARD.encode(&png),
        w: img.w as i32,
        h: img.h as i32,
    });
    Some(SyncedImage {
        png: Arc::new(png),
        w: img.w,
        h: img.h,
    })
}

/// Join `handle`, but never block longer than `timeout`: detach a wedged backend
/// rather than hang the caller (mirrors the input backends' bounded teardown).
fn join_bounded(handle: JoinHandle<()>, timeout: Duration) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });
    if rx.recv_timeout(timeout).is_err() {
        tracing::warn!("clipboard thread did not stop within {timeout:?}; detaching");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    /// Image edge limit used throughout the tests.
    const TEST_PX: usize = 4096;

    /// Default settings for tests that don't care about the limits themselves.
    fn set() -> ClipboardSettings {
        ClipboardSettings::default()
    }

    /// A fake shared clipboard for the sync logic.
    #[derive(Default)]
    struct Fake {
        content: Option<String>,
        image: Option<RgbaImage>,
    }
    impl SystemClipboard for Fake {
        fn read_text(&mut self) -> Option<String> {
            self.content.clone()
        }
        fn write_text(&mut self, text: &str) -> bool {
            self.content = Some(text.to_owned());
            true
        }
        fn read_image(&mut self) -> Option<RgbaImage> {
            self.image.clone()
        }
        fn write_image(&mut self, img: &RgbaImage) -> bool {
            self.image = Some(img.clone());
            true
        }
    }

    /// A solid-colour RGBA image, with `seed` mixed into every pixel so different
    /// seeds differ everywhere (not just in a corner the fingerprint might miss).
    fn img(w: usize, h: usize, seed: u8) -> RgbaImage {
        let bytes = (0..w * h)
            .flat_map(|i| {
                let v = seed.wrapping_add(i as u8);
                [v, v.wrapping_mul(3), v.wrapping_mul(7), 255]
            })
            .collect();
        RgbaImage { w, h, bytes }
    }

    fn b64(data: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn png_roundtrip_preserves_the_image_exactly() {
        // Clipboard images must survive the wire unchanged — a lossy or misordered
        // channel conversion would silently corrupt every screenshot.
        let original = img(32, 24, 5);
        let encoded = encode_png(&original, TEST_PX).expect("encode");
        let decoded = decode_png(&encoded, TEST_PX).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn applied_remote_image_is_not_echoed_back() {
        // The loop-free invariant, for images: an image we received and applied must
        // not be re-sent when the next poll reads it back off the clipboard.
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last_img = None;
        let source = img(16, 16, 9);
        let png = encode_png(&source, TEST_PX).unwrap();

        assert!(apply_remote_image(&mut cb, &mut last_img, &b64(&png), 16, 16, set()).is_some());
        assert_eq!(cb.image.as_ref(), Some(&source));
        // The poll now reads back exactly what we just applied.
        assert!(poll_local_image(&mut cb, &mut last_img, &tx, set()).is_none());
        assert!(rx.try_recv().is_err(), "applied image must not echo back to the peer");
    }

    #[test]
    fn a_new_local_image_is_sent_once() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake {
            image: Some(img(8, 8, 1)),
            ..Default::default()
        };
        let mut last_img = None;

        assert!(poll_local_image(&mut cb, &mut last_img, &tx, set()).is_some());
        match rx.try_recv() {
            Ok(Message::ClipboardImage { w, h, data }) => {
                assert_eq!((w, h), (8, 8));
                assert_eq!(decode_png(&{
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD.decode(&data).unwrap()
                }, TEST_PX), Some(img(8, 8, 1)));
            }
            other => panic!("expected a ClipboardImage, got {other:?}"),
        }
        // Unchanged clipboard: no repeat send on the next poll.
        assert!(poll_local_image(&mut cb, &mut last_img, &tx, set()).is_none());
        assert!(rx.try_recv().is_err());

        // A genuinely different image does go out.
        cb.image = Some(img(8, 8, 200));
        assert!(poll_local_image(&mut cb, &mut last_img, &tx, set()).is_some());
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn malformed_and_oversized_images_are_rejected_not_applied() {
        // This decodes attacker-reachable data off the network: it must refuse
        // cleanly rather than panic, over-allocate, or write junk to the clipboard.
        let mut cb = Fake::default();
        let mut last_img = None;

        assert!(apply_remote_image(&mut cb, &mut last_img, "not base64!!", 4, 4, set()).is_none());
        assert!(apply_remote_image(&mut cb, &mut last_img, &b64(b"not a png"), 4, 4, set()).is_none());
        assert!(apply_remote_image(&mut cb, &mut last_img, "", 0, 0, set()).is_none());

        // A valid PNG whose real dimensions contradict the declared ones is refused:
        // trusting the header over the payload is how buffer-size bugs start.
        let png = encode_png(&img(8, 8, 3), TEST_PX).unwrap();
        assert!(apply_remote_image(&mut cb, &mut last_img, &b64(&png), 4096, 4096, set()).is_none());

        // And an image beyond the user's own edge limit is refused before decoding.
        let small = ClipboardSettings { max_image_px: 4, ..set() };
        assert!(apply_remote_image(&mut cb, &mut last_img, &b64(&png), 8, 8, small).is_none());

        assert!(cb.image.is_none(), "nothing should have reached the clipboard");
        assert_eq!(last_img, None);
    }

    #[test]
    fn an_image_too_large_to_encode_is_not_retried_every_poll() {
        // Over-cap images must be remembered as "seen", otherwise every poll re-encodes
        // a huge bitmap forever — burning CPU in the process that carries live input.
        let (tx, _rx) = unbounded_channel();
        let mut cb = Fake {
            // Wider than MAX_IMAGE_PIXELS allows, so encode_png refuses it.
            image: Some(RgbaImage { w: 5000, h: 5000, bytes: Vec::new() }),
            ..Default::default()
        };
        let mut last_img = None;
        assert!(poll_local_image(&mut cb, &mut last_img, &tx, set()).is_none());
        assert!(last_img.is_some(), "the rejected image must still be fingerprinted");
    }

    #[test]
    fn applied_remote_is_not_echoed_back() {
        // The core loop-free invariant: text we received and set locally must NOT be
        // re-forwarded when the next local poll reads it back.
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;

        apply_remote(&mut cb, &mut last, &txt("hello"));
        assert_eq!(cb.content.as_deref(), Some("hello"));
        assert_eq!(last.as_deref(), Some("hello"));

        poll_local(&mut cb, &mut last, &tx, set());
        assert!(rx.try_recv().is_err(), "applied-remote text must not echo back");
    }

    #[test]
    fn local_change_is_forwarded_exactly_once() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;

        // User copies something locally.
        cb.content = Some("world".to_string());
        poll_local(&mut cb, &mut last, &tx, set());
        assert_eq!(rx.try_recv().unwrap(), Message::Clipboard { text: "world".to_string() });

        // Polling again with no change must not resend.
        poll_local(&mut cb, &mut last, &tx, set());
        assert!(rx.try_recv().is_err(), "unchanged clipboard must not resend");
    }

    #[test]
    fn remote_then_local_change_round_trips_without_loop() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;

        apply_remote(&mut cb, &mut last, &txt("A"));
        poll_local(&mut cb, &mut last, &tx, set());
        assert!(rx.try_recv().is_err(), "no echo for A");

        // A genuinely new local copy is forwarded.
        cb.content = Some("B".to_string());
        poll_local(&mut cb, &mut last, &tx, set());
        assert_eq!(rx.try_recv().unwrap(), Message::Clipboard { text: "B".to_string() });
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn oversized_text_is_not_forwarded() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;
        cb.content = Some("x".repeat(set().max_item_bytes + 1));
        poll_local(&mut cb, &mut last, &tx, set());
        assert!(rx.try_recv().is_err(), "oversized clipboard must not be synced");
        // And it's remembered so we don't re-evaluate it forever.
        assert!(last.is_some());
    }

    #[test]
    fn empty_clipboard_is_ignored() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake { content: Some(String::new()), ..Default::default() };
        let mut last = None;
        poll_local(&mut cb, &mut last, &tx, set());
        assert!(rx.try_recv().is_err());
    }

    fn txt(s: &str) -> Arc<String> {
        Arc::new(s.to_string())
    }

    #[test]
    fn history_is_newest_first_and_moves_repeats_to_the_front() {
        let h = ClipboardHistory::new();
        h.record_text(txt("a"), false);
        h.record_text(txt("b"), true);
        let m = h.meta();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].preview, "b", "newest first");
        assert!(m[0].incoming);

        // Copying something we already hold moves it up rather than duplicating it —
        // the clipboard-manager behaviour. Direction is refreshed to the latest.
        h.record_text(txt("a"), true);
        let m = h.meta();
        assert_eq!(m.len(), 2, "a repeat must not create a second entry");
        assert_eq!(m[0].preview, "a");
        assert!(m[0].incoming, "direction follows the most recent transfer");
    }

    #[test]
    fn the_item_count_limit_is_honoured() {
        let limits = HistoryLimits { max_items: 3, ..Default::default() };
        let h = ClipboardHistory::with_limits(limits);
        for i in 0..10 {
            h.record_text(txt(&format!("x{i}")), false);
        }
        let m = h.meta();
        assert_eq!(m.len(), 3, "capped at the user's setting");
        assert_eq!(m[0].preview, "x9", "the newest survives");
    }

    #[test]
    fn the_memory_budget_evicts_oldest_first() {
        // Three 100-byte entries against a 250-byte budget: the oldest goes.
        let limits = HistoryLimits {
            max_items: 100,
            max_item_bytes: 1024,
            memory_budget: 250,
        };
        let h = ClipboardHistory::with_limits(limits);
        for i in 0..3 {
            h.record_text(txt(&format!("{}{}", i, "x".repeat(99))), false);
        }
        let (count, bytes) = h.usage();
        assert_eq!(count, 2, "evicted down to the budget");
        assert!(bytes <= 250, "budget respected, got {bytes}");
        assert!(h.meta()[0].preview.starts_with('2'), "the newest survives");
    }

    #[test]
    fn an_item_over_the_per_item_limit_is_not_kept() {
        let limits = HistoryLimits { max_item_bytes: 10, ..Default::default() };
        let h = ClipboardHistory::with_limits(limits);
        assert!(h.record_text(txt("x".repeat(11).as_str()), false).is_none());
        assert!(h.meta().is_empty());
        assert!(h.record_text(txt("short"), false).is_some());
    }

    #[test]
    fn pinned_entries_survive_eviction_and_clear() {
        let limits = HistoryLimits { max_items: 2, ..Default::default() };
        let h = ClipboardHistory::with_limits(limits);
        let keep = h.record_text(txt("keep me"), false).unwrap();
        assert_eq!(h.toggle_pin(keep), Some(true));

        // Push well past the cap; the pinned entry must still be there.
        for i in 0..10 {
            h.record_text(txt(&format!("junk{i}")), false);
        }
        assert!(h.meta().iter().any(|e| e.id == keep), "pinned entry was evicted");

        h.clear();
        let m = h.meta();
        assert_eq!(m.len(), 1, "clear keeps pinned entries and drops the rest");
        assert_eq!(m[0].id, keep);
    }

    #[test]
    fn images_keep_their_bytes_so_they_can_be_pasted_again() {
        // The defect this whole change exists to fix: an image used to be stored as a
        // text label with the pixels thrown away, so it could never be put back.
        let h = ClipboardHistory::new();
        let png = encode_png(&img(8, 8, 3), TEST_PX).unwrap();
        let id = h.record_image(Arc::new(png.clone()), 8, 8, true).unwrap();

        let m = h.meta();
        assert!(m[0].is_image);
        assert_eq!(m[0].preview, format!("image 8×8, {} KB", png.len().div_ceil(1024)));
        match h.payload(id) {
            Some(ClipPayload::Image { png: stored, w, h: hh }) => {
                assert_eq!(*stored, png, "the encoded image itself must be kept");
                assert_eq!((w, hh), (8, 8));
            }
            other => panic!("expected the image payload back, got {other:?}"),
        }
    }

    #[test]
    fn reusing_an_entry_sets_the_clipboard_sends_it_and_promotes_it() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;
        let mut last_img = None;
        let h = ClipboardHistory::new();
        let old = h.record_text(txt("older"), false).unwrap();
        h.record_text(txt("newer"), false);

        assert!(reuse_entry(&mut cb, &mut last, &mut last_img, &h, &tx, true, old, set(), None, &mut None));
        assert_eq!(cb.content.as_deref(), Some("older"), "clipboard was set");
        assert_eq!(
            rx.try_recv().unwrap(),
            Message::Clipboard { text: "older".into() },
            "re-use syncs like a fresh copy"
        );
        assert_eq!(h.meta()[0].id, old, "the re-used entry moves to the front");

        // `last` now holds what we wrote, so the next poll must not send it again —
        // otherwise every re-use would cost two sends.
        cb.content = Some("older".into());
        assert!(poll_local(&mut cb, &mut last, &tx, set()).is_none());
        assert!(rx.try_recv().is_err(), "re-use must not echo on the next poll");
    }

    #[test]
    fn reusing_while_disconnected_still_sets_the_local_clipboard() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let h = ClipboardHistory::new();
        let id = h.record_text(txt("offline"), false).unwrap();

        assert!(reuse_entry(&mut cb, &mut None, &mut None, &h, &tx, false, id, set(), None, &mut None));
        assert_eq!(cb.content.as_deref(), Some("offline"));
        assert!(rx.try_recv().is_err(), "nothing goes to a peer that isn't connected");
    }

    #[test]
    fn reusing_a_missing_entry_is_a_no_op() {
        let (tx, _rx) = unbounded_channel();
        let mut cb = Fake::default();
        let h = ClipboardHistory::new();
        assert!(!reuse_entry(&mut cb, &mut None, &mut None, &h, &tx, true, 999, set(), None, &mut None));
        assert!(cb.content.is_none());
    }

    #[test]
    fn apply_and_poll_report_changes_for_history() {
        let (tx, _rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;
        assert!(apply_remote(&mut cb, &mut last, &Arc::new("in".to_string())), "new remote -> true");
        assert!(!apply_remote(&mut cb, &mut last, &Arc::new("in".to_string())), "same remote -> false");

        cb.content = Some("out".into());
        assert_eq!(poll_local(&mut cb, &mut last, &tx, set()).as_deref().map(|s| s.as_str()), Some("out"));
        assert!(poll_local(&mut cb, &mut last, &tx, set()).is_none(), "unchanged -> None");
    }
}
