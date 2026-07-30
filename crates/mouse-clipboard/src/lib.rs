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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use mouse_core::protocol::Message;

/// How many recent clipboard entries the history keeps (for the GUI view).
const HISTORY_CAP: usize = 25;

/// One remembered clipboard transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipEntry {
    pub text: String,
    /// True if received from the peer, false if sent from this machine.
    pub incoming: bool,
}

/// A small, shared, newest-first ring of recent clipboard transfers. Cheap to clone
/// (an `Arc`); the sync thread records into it and the GUI snapshots it.
#[derive(Clone, Default)]
pub struct ClipboardHistory {
    inner: Arc<Mutex<VecDeque<ClipEntry>>>,
}

impl ClipboardHistory {
    pub fn new() -> ClipboardHistory {
        ClipboardHistory::default()
    }

    /// Record a transfer at the front, collapsing an immediate duplicate and capping
    /// the length. Lock poisoning is ignored (a stale view is harmless).
    pub fn record(&self, text: String, incoming: bool) {
        if let Ok(mut q) = self.inner.lock() {
            if q.front().is_some_and(|e| e.text == text && e.incoming == incoming) {
                return;
            }
            q.push_front(ClipEntry { text, incoming });
            q.truncate(HISTORY_CAP);
        }
    }

    /// Newest-first snapshot for display.
    pub fn snapshot(&self) -> Vec<ClipEntry> {
        self.inner
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Drop all remembered entries.
    pub fn clear(&self) {
        if let Ok(mut q) = self.inner.lock() {
            q.clear();
        }
    }
}

/// How often the local clipboard is polled for a user change (also the upper bound
/// on how long a received update waits before being applied).
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Don't forward absurdly large text over the shared input link (it would stall
/// mouse/keyboard latency). ~1 MiB of text is already far beyond normal use.
const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// Cap on the encoded PNG we will send. The frame codec allows 8 MiB; staying well
/// under it keeps a large screenshot from adding latency to mouse and keyboard, which
/// share this link.
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

/// Cap on raw pixels, checked *before* decoding or encoding. 4096×4096 is 64 MiB of
/// RGBA — beyond that, the conversion itself costs more time than a clipboard sync is
/// worth, and a hostile or corrupt peer message could otherwise ask us to allocate
/// gigabytes.
const MAX_IMAGE_PIXELS: usize = 4096 * 4096;

/// Poll the clipboard for an *image* only every Nth tick. Reading an image means the
/// OS hands over (and we copy) a full uncompressed bitmap; doing that twice a second
/// forever would burn measurable CPU in a process that must never be slow. Text stays
/// on the fast tick because reading it is trivially cheap.
const IMAGE_POLL_EVERY: u32 = 4; // 4 × 500 ms = every 2 s

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

/// Encode RGBA8 as PNG. `None` if the image is outside the caps or the encoder fails.
fn encode_png(img: &RgbaImage) -> Option<Vec<u8>> {
    if img.w == 0 || img.h == 0 || img.w.checked_mul(img.h)? > MAX_IMAGE_PIXELS {
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
fn decode_png(data: &[u8]) -> Option<RgbaImage> {
    let mut dec = png::Decoder::new(data);
    dec.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::EXPAND);
    let mut reader = dec.read_info().ok()?;
    let (w, h) = {
        let info = reader.info();
        (info.width as usize, info.height as usize)
    };
    if w == 0 || h == 0 || w.checked_mul(h)? > MAX_IMAGE_PIXELS {
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

/// How an image is shown in the clipboard history (which is a list of strings).
fn image_label(img: &RgbaImage, encoded: usize) -> String {
    format!("[image {}×{}, {} KB]", img.w, img.h, encoded.div_ceil(1024))
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
    /// `connected` (set by the engine) gates outgoing broadcasts so we never queue
    /// unboundedly while disconnected nor flush a stale backlog on reconnect.
    pub fn spawn(
        to_peer: UnboundedSender<Message>,
        remote_rx: Receiver<Message>,
        connected: Arc<AtomicBool>,
        history: Option<ClipboardHistory>,
    ) -> ClipboardSync {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let join = std::thread::Builder::new()
            .name("clipboard-sync".into())
            .spawn(move || run(to_peer, remote_rx, st, connected, history))
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

fn run(
    to_peer: UnboundedSender<Message>,
    remote_rx: Receiver<Message>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    history: Option<ClipboardHistory>,
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
    let mut tick: u32 = 0;

    loop {
        // Apply a received clipboard update if one arrived; this recv is also the
        // loop's wait point (wakes on a message or every POLL_INTERVAL).
        match remote_rx.recv_timeout(POLL_INTERVAL) {
            Ok(Message::Clipboard { text }) => {
                if apply_remote(&mut clipboard, &mut last, text.clone()) {
                    if let Some(h) = &history {
                        h.record(text, true);
                    }
                }
            }
            Ok(Message::ClipboardImage { data, w, h }) => {
                if let Some(label) = apply_remote_image(&mut clipboard, &mut last_img, &data, w, h) {
                    if let Some(hist) = &history {
                        hist.record(label, true);
                    }
                }
            }
            Ok(_) => {} // stray message for this thread: ignore
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break, // engine dropped the sender
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Only broadcast local changes while a peer is connected: this avoids
        // queueing unboundedly while disconnected, and avoids flushing a stale
        // backlog (clipboard copied while disconnected) to whoever connects next. On
        // reconnect the first poll naturally syncs the current clipboard once.
        if connected.load(Ordering::SeqCst) {
            if let Some(sent) = poll_local(&mut clipboard, &mut last, &to_peer) {
                if let Some(h) = &history {
                    h.record(sent, false);
                }
            }
            // Images on the slow tick — see `IMAGE_POLL_EVERY`. When the clipboard holds
            // text rather than an image the read fails fast, so the expensive path only
            // runs when there is actually an image to copy.
            tick = tick.wrapping_add(1);
            if tick % IMAGE_POLL_EVERY == 0 {
                if let Some(sent) = poll_local_image(&mut clipboard, &mut last_img, &to_peer) {
                    if let Some(h) = &history {
                        h.record(sent, false);
                    }
                }
            }
        }
    }
}

/// Apply a clipboard value received from the peer, unless it's what we already have
/// (which would mean it originated here and came back — don't reapply). Returns true
/// if a new value was actually written (so the caller can record it in history).
fn apply_remote<C: SystemClipboard>(clipboard: &mut C, last: &mut Option<String>, text: String) -> bool {
    if last.as_deref() == Some(text.as_str()) {
        return false;
    }
    if clipboard.write_text(&text) {
        *last = Some(text);
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
) -> Option<String> {
    let text = clipboard.read_text()?; // empty or non-text (e.g. an image): nothing to forward
    if text.is_empty() || last.as_deref() == Some(text.as_str()) {
        return None;
    }
    if text.len() > MAX_TEXT_BYTES {
        tracing::debug!("clipboard text {} bytes exceeds cap; not syncing", text.len());
        *last = Some(text); // remember it so we don't re-check it every poll
        return None;
    }
    *last = Some(text.clone());
    let _ = to_peer.send(Message::Clipboard { text: text.clone() });
    Some(text)
}

/// Apply an image received from the peer. Returns a history label if a new image was
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
) -> Option<String> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    if raw.len() > MAX_IMAGE_BYTES {
        tracing::debug!("received clipboard image of {} bytes exceeds cap; ignoring", raw.len());
        return None;
    }
    let img = decode_png(&raw).or_else(|| {
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
    Some(image_label(&img, raw.len()))
}

/// Forward a local clipboard image to the peer, unless it's what we last set/sent.
/// Returns a history label if something was sent.
fn poll_local_image<C: SystemClipboard>(
    clipboard: &mut C,
    last_img: &mut Option<u64>,
    to_peer: &UnboundedSender<Message>,
) -> Option<String> {
    use base64::Engine as _;
    let img = clipboard.read_image()?; // no image on the clipboard: nothing to do
    let key = image_key(&img);
    if *last_img == Some(key) {
        return None;
    }
    // Remember it either way: a rejected image must not be re-encoded every poll.
    *last_img = Some(key);
    let png = encode_png(&img)?;
    if png.len() > MAX_IMAGE_BYTES {
        tracing::debug!("local clipboard image is {} bytes encoded; not syncing", png.len());
        return None;
    }
    let label = image_label(&img, png.len());
    let _ = to_peer.send(Message::ClipboardImage {
        data: base64::engine::general_purpose::STANDARD.encode(&png),
        w: img.w as i32,
        h: img.h as i32,
    });
    Some(label)
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
        let encoded = encode_png(&original).expect("encode");
        let decoded = decode_png(&encoded).expect("decode");
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
        let png = encode_png(&source).unwrap();

        assert!(apply_remote_image(&mut cb, &mut last_img, &b64(&png), 16, 16).is_some());
        assert_eq!(cb.image.as_ref(), Some(&source));
        // The poll now reads back exactly what we just applied.
        assert_eq!(poll_local_image(&mut cb, &mut last_img, &tx), None);
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

        assert!(poll_local_image(&mut cb, &mut last_img, &tx).is_some());
        match rx.try_recv() {
            Ok(Message::ClipboardImage { w, h, data }) => {
                assert_eq!((w, h), (8, 8));
                assert_eq!(decode_png(&{
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD.decode(&data).unwrap()
                }), Some(img(8, 8, 1)));
            }
            other => panic!("expected a ClipboardImage, got {other:?}"),
        }
        // Unchanged clipboard: no repeat send on the next poll.
        assert_eq!(poll_local_image(&mut cb, &mut last_img, &tx), None);
        assert!(rx.try_recv().is_err());

        // A genuinely different image does go out.
        cb.image = Some(img(8, 8, 200));
        assert!(poll_local_image(&mut cb, &mut last_img, &tx).is_some());
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn malformed_and_oversized_images_are_rejected_not_applied() {
        // This decodes attacker-reachable data off the network: it must refuse
        // cleanly rather than panic, over-allocate, or write junk to the clipboard.
        let mut cb = Fake::default();
        let mut last_img = None;

        assert_eq!(apply_remote_image(&mut cb, &mut last_img, "not base64!!", 4, 4), None);
        assert_eq!(apply_remote_image(&mut cb, &mut last_img, &b64(b"not a png"), 4, 4), None);
        assert_eq!(apply_remote_image(&mut cb, &mut last_img, "", 0, 0), None);

        // A valid PNG whose real dimensions contradict the declared ones is refused:
        // trusting the header over the payload is how buffer-size bugs start.
        let png = encode_png(&img(8, 8, 3)).unwrap();
        assert_eq!(apply_remote_image(&mut cb, &mut last_img, &b64(&png), 4096, 4096), None);

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
        assert_eq!(poll_local_image(&mut cb, &mut last_img, &tx), None);
        assert!(last_img.is_some(), "the rejected image must still be fingerprinted");
    }

    #[test]
    fn applied_remote_is_not_echoed_back() {
        // The core loop-free invariant: text we received and set locally must NOT be
        // re-forwarded when the next local poll reads it back.
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;

        apply_remote(&mut cb, &mut last, "hello".to_string());
        assert_eq!(cb.content.as_deref(), Some("hello"));
        assert_eq!(last.as_deref(), Some("hello"));

        poll_local(&mut cb, &mut last, &tx);
        assert!(rx.try_recv().is_err(), "applied-remote text must not echo back");
    }

    #[test]
    fn local_change_is_forwarded_exactly_once() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;

        // User copies something locally.
        cb.content = Some("world".to_string());
        poll_local(&mut cb, &mut last, &tx);
        assert_eq!(rx.try_recv().unwrap(), Message::Clipboard { text: "world".to_string() });

        // Polling again with no change must not resend.
        poll_local(&mut cb, &mut last, &tx);
        assert!(rx.try_recv().is_err(), "unchanged clipboard must not resend");
    }

    #[test]
    fn remote_then_local_change_round_trips_without_loop() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;

        apply_remote(&mut cb, &mut last, "A".to_string());
        poll_local(&mut cb, &mut last, &tx);
        assert!(rx.try_recv().is_err(), "no echo for A");

        // A genuinely new local copy is forwarded.
        cb.content = Some("B".to_string());
        poll_local(&mut cb, &mut last, &tx);
        assert_eq!(rx.try_recv().unwrap(), Message::Clipboard { text: "B".to_string() });
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn oversized_text_is_not_forwarded() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;
        cb.content = Some("x".repeat(MAX_TEXT_BYTES + 1));
        poll_local(&mut cb, &mut last, &tx);
        assert!(rx.try_recv().is_err(), "oversized clipboard must not be synced");
        // And it's remembered so we don't re-evaluate it forever.
        assert!(last.is_some());
    }

    #[test]
    fn empty_clipboard_is_ignored() {
        let (tx, mut rx) = unbounded_channel();
        let mut cb = Fake { content: Some(String::new()), ..Default::default() };
        let mut last = None;
        poll_local(&mut cb, &mut last, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn history_records_newest_first_dedups_and_caps() {
        let h = ClipboardHistory::new();
        h.record("a".into(), false);
        h.record("a".into(), false); // immediate duplicate collapses
        h.record("b".into(), true);
        let snap = h.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0], ClipEntry { text: "b".into(), incoming: true }, "newest first");
        assert_eq!(snap[1], ClipEntry { text: "a".into(), incoming: false });

        // The same text in the other direction is a distinct entry.
        h.record("b".into(), false);
        assert_eq!(h.snapshot().len(), 3);

        for i in 0..HISTORY_CAP + 10 {
            h.record(format!("x{i}"), false);
        }
        assert_eq!(h.snapshot().len(), HISTORY_CAP, "capped");

        h.clear();
        assert!(h.snapshot().is_empty());
    }

    #[test]
    fn apply_and_poll_report_changes_for_history() {
        let (tx, _rx) = unbounded_channel();
        let mut cb = Fake::default();
        let mut last = None;
        assert!(apply_remote(&mut cb, &mut last, "in".into()), "new remote -> true");
        assert!(!apply_remote(&mut cb, &mut last, "in".into()), "same remote -> false");

        cb.content = Some("out".into());
        assert_eq!(poll_local(&mut cb, &mut last, &tx).as_deref(), Some("out"));
        assert_eq!(poll_local(&mut cb, &mut last, &tx), None, "unchanged -> None");
    }
}
