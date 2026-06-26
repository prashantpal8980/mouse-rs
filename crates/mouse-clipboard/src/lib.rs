//! Clipboard sync for mouse-rs.
//!
//! A dedicated native thread polls the local clipboard and applies the peer's,
//! talking to the async link over channels — the same "blocking backend on its own
//! thread, async core untouched" shape the input backends use. Sync is loop-free
//! via a single last-content guard: whatever we last applied-from-peer or
//! broadcast-from-local is remembered, so re-reading it never echoes back.
//!
//! Text only for now. The protocol's `ClipboardImage` is reserved for a later pass
//! (it needs RGBA<->PNG conversion and size capping); image messages received now
//! are simply ignored.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use mouse_core::protocol::Message;

/// How often the local clipboard is polled for a user change (also the upper bound
/// on how long a received update waits before being applied).
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Don't forward absurdly large text over the shared input link (it would stall
/// mouse/keyboard latency). ~1 MiB of text is already far beyond normal use.
const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// A minimal text-clipboard backend, abstracted so the sync logic is testable
/// without a real OS clipboard.
trait TextClipboard {
    /// Current clipboard text, or `None` if empty / not text / unavailable.
    fn read_text(&mut self) -> Option<String>;
    /// Set the clipboard text; returns whether it succeeded.
    fn write_text(&mut self, text: &str) -> bool;
}

impl TextClipboard for arboard::Clipboard {
    fn read_text(&mut self) -> Option<String> {
        self.get_text().ok()
    }
    fn write_text(&mut self, text: &str) -> bool {
        self.set_text(text.to_owned()).is_ok()
    }
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
    ) -> ClipboardSync {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let join = std::thread::Builder::new()
            .name("clipboard-sync".into())
            .spawn(move || run(to_peer, remote_rx, st, connected))
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
    // broadcast-from-local — the anchor that makes the sync loop-free.
    let mut last = clipboard.read_text().filter(|t| !t.is_empty());

    loop {
        // Apply a received clipboard update if one arrived; this recv is also the
        // loop's wait point (wakes on a message or every POLL_INTERVAL).
        match remote_rx.recv_timeout(POLL_INTERVAL) {
            Ok(Message::Clipboard { text }) => apply_remote(&mut clipboard, &mut last, text),
            Ok(_) => {} // ClipboardImage and any stray message: ignored for now
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
            poll_local(&mut clipboard, &mut last, &to_peer);
        }
    }
}

/// Apply a clipboard value received from the peer, unless it's what we already have
/// (which would mean it originated here and came back — don't reapply).
fn apply_remote<C: TextClipboard>(clipboard: &mut C, last: &mut Option<String>, text: String) {
    if last.as_deref() == Some(text.as_str()) {
        return;
    }
    if clipboard.write_text(&text) {
        *last = Some(text);
    } else {
        tracing::debug!("clipboard set failed");
    }
}

/// Forward a local clipboard change to the peer, unless it's what we last set/sent
/// (which would echo a value the peer already has).
fn poll_local<C: TextClipboard>(
    clipboard: &mut C,
    last: &mut Option<String>,
    to_peer: &UnboundedSender<Message>,
) {
    let Some(text) = clipboard.read_text() else {
        return; // empty or non-text (e.g. an image) — nothing to forward
    };
    if text.is_empty() || last.as_deref() == Some(text.as_str()) {
        return;
    }
    if text.len() > MAX_TEXT_BYTES {
        tracing::debug!("clipboard text {} bytes exceeds cap; not syncing", text.len());
        *last = Some(text); // remember it so we don't re-check it every poll
        return;
    }
    *last = Some(text.clone());
    let _ = to_peer.send(Message::Clipboard { text });
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
    }
    impl TextClipboard for Fake {
        fn read_text(&mut self) -> Option<String> {
            self.content.clone()
        }
        fn write_text(&mut self, text: &str) -> bool {
            self.content = Some(text.to_owned());
            true
        }
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
        let mut cb = Fake { content: Some(String::new()) };
        let mut last = None;
        poll_local(&mut cb, &mut last, &tx);
        assert!(rx.try_recv().is_err());
    }
}
