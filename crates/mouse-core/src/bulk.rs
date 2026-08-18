//! The bulk lane: a second connection for payloads too big to put in front of a
//! mouse packet.
//!
//! ## Why this exists
//!
//! Mouse, keyboard, clipboard and heartbeat all share one TLS/TCP connection. TCP
//! delivers bytes strictly in order, so anything queued behind a large payload waits
//! for it to drain — *head-of-line blocking*. A 40-byte mouse move does not overtake a
//! 5 MB image; it waits for all of it. On gigabit Ethernet that is ~50 ms and
//! invisible, which is why it has never been reported. On a 40 Mbps hotspot the same
//! image blocks the link for about a second, and the mouse visibly stalls.
//!
//! So bulk data gets its own connection. Input frames then *cannot* queue behind file
//! bytes, because they are not in the same byte stream.
//!
//! ## Why a second connection is not enough
//!
//! Both lanes still share one radio. Saturating the link fills the queues in the NIC
//! and the router — *bufferbloat* — and input latency rises even though the lanes are
//! separate. So the bulk lane is also paced, and the signal it paces against is the
//! thing we actually care about protecting: the input link's own heartbeat round-trip
//! time. See [`Pacer`].
//!
//! ## Framing
//!
//! Binary, not JSON. The input lane's codec is JSON because it carries small,
//! debuggable control messages; putting a 100 MB file through it would mean base64
//! (+33%) and holding the whole payload in memory as one string. Here a frame is a
//! fixed 21-byte header and an opaque body, so a transfer streams in bounded chunks
//! whatever its total size.

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

/// Payload bytes per chunk.
///
/// Small enough that pausing is meaningful (a pause takes effect within one chunk)
/// and that a chunk in flight is a negligible amount of committed, unstoppable data;
/// large enough that per-frame overhead and syscall count stay irrelevant next to the
/// payload.
pub const CHUNK_BYTES: usize = 256 * 1024;

/// Largest bulk frame we will decode: one chunk plus header, with room to spare.
/// A corrupt or hostile length must not be able to ask us for a huge allocation.
const MAX_BULK_FRAME: usize = CHUNK_BYTES + 4096;

/// Header size: kind(1) + transfer(8) + seq(4) + total(8).
const HEADER_BYTES: usize = 21;

const KIND_BEGIN: u8 = 1;
const KIND_CHUNK: u8 = 2;
const KIND_END: u8 = 3;
const KIND_ABORT: u8 = 4;

/// One frame on the bulk lane.
///
/// A transfer is `Begin`, then `Chunk`s in `seq` order, then `End`. `Abort` may
/// replace the rest at any point. `transfer` ties them together so two transfers can
/// interleave on the same connection without a reassembly ambiguity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkFrame {
    /// Start of a transfer. `total` is the declared payload length in bytes, and
    /// `meta` describes what it is (a JSON blob — small, and only sent once).
    Begin {
        transfer: u64,
        total: u64,
        meta: Vec<u8>,
    },
    /// One ordered slice of the payload.
    Chunk {
        transfer: u64,
        seq: u32,
        data: Vec<u8>,
    },
    /// All chunks sent.
    End { transfer: u64 },
    /// Sender gave up. The receiver must discard what it has.
    Abort { transfer: u64 },
}

impl BulkFrame {
    pub fn transfer(&self) -> u64 {
        match self {
            BulkFrame::Begin { transfer, .. }
            | BulkFrame::Chunk { transfer, .. }
            | BulkFrame::End { transfer }
            | BulkFrame::Abort { transfer } => *transfer,
        }
    }

    /// Payload bytes this frame puts on the wire, for pacing. Header is excluded
    /// deliberately: the pacer is protecting bandwidth, and 21 bytes against a 256 KB
    /// chunk is noise.
    pub fn payload_len(&self) -> usize {
        match self {
            BulkFrame::Chunk { data, .. } => data.len(),
            BulkFrame::Begin { meta, .. } => meta.len(),
            _ => 0,
        }
    }
}

/// Length-delimited binary codec for [`BulkFrame`].
pub struct BulkCodec {
    inner: LengthDelimitedCodec,
}

impl Default for BulkCodec {
    fn default() -> Self {
        let inner = LengthDelimitedCodec::builder()
            .max_frame_length(MAX_BULK_FRAME)
            .length_field_type::<u32>()
            .new_codec();
        BulkCodec { inner }
    }
}

impl BulkCodec {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BulkError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed bulk frame")]
    Malformed,
}

impl Encoder<BulkFrame> for BulkCodec {
    type Error = BulkError;

    fn encode(&mut self, item: BulkFrame, dst: &mut BytesMut) -> Result<(), BulkError> {
        let (kind, transfer, seq, total, body): (u8, u64, u32, u64, &[u8]) = match &item {
            BulkFrame::Begin { transfer, total, meta } => (KIND_BEGIN, *transfer, 0, *total, meta),
            BulkFrame::Chunk { transfer, seq, data } => (KIND_CHUNK, *transfer, *seq, 0, data),
            BulkFrame::End { transfer } => (KIND_END, *transfer, 0, 0, &[]),
            BulkFrame::Abort { transfer } => (KIND_ABORT, *transfer, 0, 0, &[]),
        };
        let mut buf = BytesMut::with_capacity(HEADER_BYTES + body.len());
        buf.put_u8(kind);
        buf.put_u64(transfer);
        buf.put_u32(seq);
        buf.put_u64(total);
        buf.put_slice(body);
        self.inner.encode(buf.freeze(), dst)?;
        Ok(())
    }
}

impl Decoder for BulkCodec {
    type Item = BulkFrame;
    type Error = BulkError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<BulkFrame>, BulkError> {
        let Some(mut frame) = self.inner.decode(src)? else {
            return Ok(None);
        };
        // Every field is read from the network, so a short frame is a protocol error
        // to reject, never something to index past.
        if frame.len() < HEADER_BYTES {
            return Err(BulkError::Malformed);
        }
        let kind = frame.get_u8();
        let transfer = frame.get_u64();
        let seq = frame.get_u32();
        let total = frame.get_u64();
        let body = frame.to_vec();
        Ok(Some(match kind {
            KIND_BEGIN => BulkFrame::Begin { transfer, total, meta: body },
            KIND_CHUNK => BulkFrame::Chunk { transfer, seq, data: body },
            KIND_END => BulkFrame::End { transfer },
            KIND_ABORT => BulkFrame::Abort { transfer },
            _ => return Err(BulkError::Malformed),
        }))
    }
}

// ----------------------------------------------------------------------------
// Pacing
// ----------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Shared view of how the *input* link is behaving, written by the input pump and
/// read by the bulk sender.
///
/// This is the whole trick: the input lane already measures its own heartbeat
/// round-trip for liveness, and that number is a direct measurement of the latency we
/// are trying to protect. Pacing against it needs no new probes, no bandwidth
/// estimate, and no configuration — and it responds to real congestion on the actual
/// path rather than to a guess about the link.
#[derive(Debug, Default)]
pub struct LinkHealth {
    /// Most recent heartbeat round-trip, microseconds. 0 = no sample yet.
    latest_us: AtomicU64,
    /// Best round-trip seen, microseconds — the "uncongested" reference.
    baseline_us: AtomicU64,
    /// Set while the user is actively driving the other machine. Latency is felt most
    /// keenly here, so the bulk lane stands down entirely.
    crossed: AtomicBool,
    /// Bytes per second observed on the last completed bulk transfer, or 0 if none has
    /// finished yet. Used to answer "will this be quick?" with a measurement rather
    /// than a guess — round-trip time says nothing about bandwidth.
    throughput_bps: AtomicU64,
}

impl LinkHealth {
    pub fn new() -> Arc<LinkHealth> {
        Arc::new(LinkHealth::default())
    }

    /// Record a heartbeat round-trip.
    pub fn record_rtt(&self, rtt: Duration) {
        let us = rtt.as_micros().min(u64::MAX as u128) as u64;
        self.latest_us.store(us, Ordering::Relaxed);
        // Baseline tracks the best sample seen. It only ever ratchets down here; it is
        // relaxed upward by `decay_baseline` so that moving to a genuinely slower
        // network doesn't leave us pacing against a baseline we can never reach again.
        let prev = self.baseline_us.load(Ordering::Relaxed);
        if prev == 0 || us < prev {
            self.baseline_us.store(us, Ordering::Relaxed);
        }
    }

    /// Let the baseline drift up slightly, so a permanently slower path is eventually
    /// accepted as the new normal instead of being read as permanent congestion.
    pub fn decay_baseline(&self) {
        let prev = self.baseline_us.load(Ordering::Relaxed);
        if prev > 0 {
            self.baseline_us.store(prev + prev / 32 + 1, Ordering::Relaxed);
        }
    }

    pub fn set_crossed(&self, crossed: bool) {
        self.crossed.store(crossed, Ordering::Relaxed);
    }

    pub fn crossed(&self) -> bool {
        self.crossed.load(Ordering::Relaxed)
    }

    /// Record what a finished transfer actually achieved.
    pub fn record_throughput(&self, bytes: u64, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        if bytes == 0 || secs <= 0.0 {
            return;
        }
        // Smoothed, so one unusually fast or slow transfer does not swing the estimate.
        let sample = (bytes as f64 / secs) as u64;
        let prev = self.throughput_bps.load(Ordering::Relaxed);
        let next = if prev == 0 { sample } else { (prev * 3 + sample) / 4 };
        self.throughput_bps.store(next, Ordering::Relaxed);
    }

    /// Measured bulk throughput, or `None` if nothing has finished yet.
    pub fn throughput_bps(&self) -> Option<u64> {
        match self.throughput_bps.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }

    /// How long `bytes` would take at the measured rate. `None` until we have a
    /// measurement — guessing here would either nag about fast transfers or silently
    /// start slow ones.
    pub fn estimate_secs(&self, bytes: u64) -> Option<f64> {
        let bps = self.throughput_bps()?;
        Some(bytes as f64 / bps as f64)
    }

    pub fn latest_us(&self) -> u64 {
        self.latest_us.load(Ordering::Relaxed)
    }

    pub fn baseline_us(&self) -> u64 {
        self.baseline_us.load(Ordering::Relaxed)
    }

    /// Is the input link currently slower than its own uncongested baseline by enough
    /// to blame the bulk transfer?
    ///
    /// The absolute floor matters as much as the ratio: on a LAN the baseline can be
    /// 300 µs, and 1.5× of that is still 450 µs — far below anything a human can feel.
    /// Reacting to it would throttle the transfer for no benefit.
    pub fn is_congested(&self) -> bool {
        let base = self.baseline_us();
        let latest = self.latest_us();
        if base == 0 || latest == 0 {
            return false; // no samples yet: don't throttle on ignorance
        }
        latest > base.saturating_mul(3) / 2 && latest > base + CONGESTION_FLOOR_US
    }
}

/// How much worse than baseline the round-trip must get before we call it congestion.
/// 8 ms is comfortably below the threshold where pointer lag becomes perceptible, and
/// comfortably above ordinary scheduling jitter.
const CONGESTION_FLOOR_US: u64 = 8_000;

/// Slowest we will ever pace down to. Below this a transfer is effectively stalled,
/// and a stalled transfer that never finishes is worse than a slightly laggy mouse.
pub const MIN_RATE_BYTES_PER_SEC: u64 = 64 * 1024;
/// Fastest we will pace up to when the link looks healthy. Well above gigabit, so on a
/// good LAN the pacer is effectively out of the way.
pub const MAX_RATE_BYTES_PER_SEC: u64 = 512 * 1024 * 1024;
/// Starting rate for a new transfer: quick on a LAN, not an instant flood on a hotspot.
const START_RATE_BYTES_PER_SEC: u64 = 2 * 1024 * 1024;
/// How much to add per healthy step (additive increase).
const RATE_STEP_BYTES_PER_SEC: u64 = 1024 * 1024;
/// Ceiling while the user is driving the other machine.
///
/// 2 MB/s keeps a 3 GB transfer visibly moving rather than frozen, while using a small
/// enough share of even a hotspot that pointer latency is unaffected. The previous
/// behaviour here was a full stop, which was indistinguishable from a hang.
const CROSSED_RATE_BYTES_PER_SEC: u64 = 2 * 1024 * 1024;

/// How the owner wants the bulk lane to behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkSettings {
    /// Stand down entirely while the user is driving the other machine.
    pub pause_while_crossed: bool,
    /// A fixed rate in bytes per second, or `None` to adapt to measured latency.
    pub rate_cap: Option<u64>,
}

impl Default for BulkSettings {
    fn default() -> Self {
        BulkSettings {
            pause_while_crossed: true,
            rate_cap: None,
        }
    }
}

/// Decides how fast the bulk lane may send, from [`LinkHealth`].
///
/// Additive-increase / multiplicative-decrease, the same shape as TCP congestion
/// control, but driven by *input latency* rather than packet loss — because input
/// latency is the thing we promised to protect, and loss would only tell us about it
/// indirectly and far too late.
pub struct Pacer {
    health: Arc<LinkHealth>,
    rate: u64,
    user_cap: Option<u64>,
}

impl Pacer {
    /// `user_cap` pins the rate if the owner would rather decide than have it adapt.
    pub fn new(health: Arc<LinkHealth>, user_cap: Option<u64>) -> Pacer {
        Pacer {
            rate: user_cap.unwrap_or(START_RATE_BYTES_PER_SEC),
            health,
            user_cap,
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// Re-evaluate the rate. Called once per chunk.
    pub fn step(&mut self) {
        if self.user_cap.is_some() {
            return; // pinned by the user: adapt nothing
        }
        if self.health.is_congested() {
            self.rate = (self.rate / 2).max(MIN_RATE_BYTES_PER_SEC);
        } else {
            self.rate = self
                .rate
                .saturating_add(RATE_STEP_BYTES_PER_SEC)
                .min(MAX_RATE_BYTES_PER_SEC);
        }
    }

    /// How long to wait before sending `bytes`, to hold the current rate.
    pub fn delay_for(&self, bytes: usize) -> Duration {
        self.delay_for_capped(bytes, None)
    }

    /// As [`Pacer::delay_for`], but never faster than `cap` bytes per second.
    pub fn delay_for_capped(&self, bytes: usize, cap: Option<u64>) -> Duration {
        let rate = match cap {
            Some(c) => self.rate.min(c),
            None => self.rate,
        };
        if rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_micros((bytes as u64).saturating_mul(1_000_000) / rate)
    }

    /// The rate ceiling to apply while the user is driving the other machine.
    ///
    /// `None` means no extra ceiling. The floor is deliberately generous enough that a
    /// large transfer still visibly progresses — the point is that the pointer stays
    /// smooth, not that the transfer stops existing.
    pub fn crossed_cap(&self, slow_while_crossed: bool) -> Option<u64> {
        (slow_while_crossed && self.health.crossed()).then_some(CROSSED_RATE_BYTES_PER_SEC)
    }
}

// ----------------------------------------------------------------------------
// Connection handling
// ----------------------------------------------------------------------------

use crate::net::LinkStream;
use crate::protocol::MessageCodec;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, FramedParts};
use tokio_util::sync::CancellationToken;

/// How many payload bytes are queued for the bulk lane but not yet written out.
///
/// This is the difference between a big file being cheap and a big file being fatal.
/// A sender reads from disk at hundreds of MB/s; the lane drains at whatever the
/// network gives, deliberately slowed further while the pointer is crossing. Left
/// unbounded, the queue *becomes* the file — sending a 4 GB ISO would hold 4 GB of it
/// in memory.
///
/// So the producer waits while this sits at its limit. Memory then tracks the limit
/// instead of the file size, which is what makes a 4 GB transfer cost the same as a
/// 4 MB one — the promise the design makes, and this is what backs it.
///
/// It counts bytes *in the channel*, not bytes in flight on the socket, so a dropped
/// connection leaves it accurate: those frames are still queued and go out when the
/// lane returns.
#[derive(Debug)]
pub struct BulkQueue {
    queued: AtomicU64,
    limit: u64,
}

impl BulkQueue {
    /// `limit` is a ceiling on buffered payload in bytes, raised to at least one chunk
    /// — a smaller limit would deadlock a producer that cannot split a chunk to fit.
    pub fn new(limit: u64) -> BulkQueue {
        BulkQueue {
            queued: AtomicU64::new(0),
            limit: limit.max(CHUNK_BYTES as u64),
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Relaxed)
    }

    /// Account for a frame handed to the lane.
    pub fn add(&self, bytes: u64) {
        self.queued.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Account for a frame written to the socket. Saturating, so a miscount can never
    /// wrap to a huge value and wedge the producer permanently.
    pub fn take(&self, bytes: u64) {
        let _ = self
            .queued
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |q| {
                Some(q.saturating_sub(bytes))
            });
    }

    /// Whether a producer should wait before queueing more.
    pub fn is_full(&self) -> bool {
        self.queued() >= self.limit
    }
}

impl Default for BulkQueue {
    fn default() -> BulkQueue {
        BulkQueue::new(8 * 1024 * 1024)
    }
}

/// Channels the app uses to talk to the bulk lane.
pub struct BulkChannels {
    /// Frames the app wants to send to the peer.
    pub outgoing: mpsc::UnboundedReceiver<BulkFrame>,
    /// Frames received from the peer. A std channel, because the far end is the
    /// clipboard thread — a blocking native thread with no runtime, exactly like the
    /// input backends.
    pub incoming: std::sync::mpsc::Sender<BulkFrame>,
    /// Shared with whoever fills `outgoing`, so it knows when to stop reading ahead.
    pub queue: Arc<BulkQueue>,
}

/// What the input listener does with a connection that declared [`crate::protocol::Lane::Bulk`].
///
/// This exists so the accept loop can hand a bulk connection off without knowing
/// anything about transfers: it takes the socket and forgets about it.
pub enum BulkServer {
    /// Bulk lane switched off: connections that ask for it are simply dropped, which
    /// the peer sees as a closed socket and reports as "no bulk lane available".
    Disabled,
    Enabled {
        channels: Arc<tokio::sync::Mutex<Option<BulkChannels>>>,
        health: Arc<LinkHealth>,
        settings: BulkSettings,
    },
}

impl BulkServer {
    pub fn disabled() -> BulkServer {
        BulkServer::Disabled
    }

    pub fn enabled(
        channels: BulkChannels,
        health: Arc<LinkHealth>,
        settings: BulkSettings,
    ) -> BulkServer {
        BulkServer::Enabled {
            channels: Arc::new(tokio::sync::Mutex::new(Some(channels))),
            health,
            settings,
        }
    }

    /// Adopt a freshly handshaken bulk connection. Returns immediately; the session
    /// runs as its own task so the input accept loop is never held up by it.
    pub fn take_server_connection(
        &self,
        framed: Framed<LinkStream, MessageCodec>,
        cancel: &CancellationToken,
    ) {
        let BulkServer::Enabled { channels, health, settings } = self else {
            tracing::debug!("a peer asked for the bulk lane, but it is disabled here");
            return;
        };
        let framed = into_bulk_framed(framed);
        let channels = channels.clone();
        let health = health.clone();
        let settings = *settings;
        let cancel = cancel.clone();
        tokio::spawn(async move {
            // One bulk session at a time. A second connection means the peer
            // reconnected, so the previous one is already dead; the guard is held for
            // the session's lifetime, which makes "already running" impossible rather
            // than merely unlikely.
            let mut guard = channels.lock().await;
            let Some(ch) = guard.as_mut() else { return };
            tracing::info!("bulk lane connected");
            bulk_pump(framed, ch, &cancel, &health, settings).await;
            tracing::info!("bulk lane closed");
        });
    }
}

/// Swap a handshake-era JSON codec for the binary bulk codec, keeping whatever the
/// handshake already read into the buffer. Dropping that buffer would silently lose
/// the first bytes of the first frame if the peer wrote them in the same packet.
fn into_bulk_framed(framed: Framed<LinkStream, MessageCodec>) -> Framed<LinkStream, BulkCodec> {
    let parts = framed.into_parts();
    let mut new = FramedParts::new::<BulkFrame>(parts.io, BulkCodec::new());
    new.read_buf = parts.read_buf;
    new.write_buf = parts.write_buf;
    Framed::from_parts(new)
}

/// The steady-state bulk loop: forward frames both ways until cancelled or closed.
///
/// Deliberately dumber than the input pump — no heartbeat and no liveness window. The
/// input link already answers "is the peer alive?", and duplicating that here would
/// mean two independent opinions about the same question and a way for them to
/// disagree. If the input link is down, so is the session.
async fn bulk_pump(
    framed: Framed<LinkStream, BulkCodec>,
    ch: &mut BulkChannels,
    cancel: &CancellationToken,
    health: &Arc<LinkHealth>,
    settings: BulkSettings,
) {
    use futures::{SinkExt, StreamExt};
    let (mut sink, mut stream) = framed.split();
    // Rate is re-evaluated per frame from the input link's own latency, unless the
    // owner has pinned it.
    let mut pacer = Pacer::new(health.clone(), settings.rate_cap);
    // Bytes and time for the current transfer, so a finished one leaves behind a real
    // throughput measurement rather than an assumption.
    let mut sent_bytes: u64 = 0;
    let mut started: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,

            item = stream.next() => match item {
                None => return,
                Some(Err(e)) => {
                    tracing::warn!("bulk lane closing: {e}");
                    return;
                }
                Some(Ok(frame)) => {
                    let _ = ch.incoming.send(frame);
                }
            },

            out = ch.outgoing.recv() => match out {
                None => return,
                Some(frame) => {
                    // Only the *first* Begin of an idle period starts the clock. A folder
                    // interleaves a Begin per file — 23 of them — and resetting on each
                    // one measured a fragment of whichever file began last rather than
                    // the transfer, which is why the panel could not show a usable rate.
                    if matches!(frame, BulkFrame::Begin { .. }) && started.is_none() {
                        sent_bytes = 0;
                        started = Some(tokio::time::Instant::now());
                    }
                    // Hold the paced rate *before* writing, so the delay is spent idle
                    // rather than inside a socket write we could no longer interrupt.
                    //
                    // While the user is driving the other machine the rate is capped
                    // hard rather than stopped. Stopping was the old behaviour and it
                    // reads as a hang: the owner watched a copy freeze the instant they
                    // crossed and resume when they crossed back, which looks like a bug
                    // however deliberate it is.
                    let bytes = frame.payload_len();
                    if bytes > 0 {
                        pacer.step();
                        let delay = pacer.delay_for_capped(
                            bytes,
                            pacer.crossed_cap(settings.pause_while_crossed),
                        );
                        if !delay.is_zero() {
                            tokio::select! {
                                _ = cancel.cancelled() => return,
                                _ = tokio::time::sleep(delay) => {}
                            }
                        }
                    }
                    // Likewise, the measurement closes when the lane actually goes quiet,
                    // not on the first file to finish out of many.
                    let is_end = matches!(frame, BulkFrame::End { .. }) && ch.queue.queued() == 0;
                    let sent = tokio::select! {
                        _ = cancel.cancelled() => return,
                        r = sink.send(frame) => r,
                    };
                    if let Err(e) = sent {
                        tracing::warn!("bulk send failed: {e}");
                        return;
                    }
                    // Credit the producer only once the bytes are actually gone. This
                    // is what paces the reader against the network instead of the disk.
                    ch.queue.take(bytes as u64);
                    sent_bytes += bytes as u64;
                    if is_end {
                        if let Some(t0) = started.take() {
                            health.record_throughput(sent_bytes, t0.elapsed());
                        }
                    }
                }
            },
        }
    }
}

/// Open the client side of the bulk lane and pump it until cancelled or dropped.
///
/// Failure here is not fatal to anything: the input link carries on, and the app falls
/// back to sending large items inline. A machine with no bulk lane is slower, not broken.
#[allow(clippy::too_many_arguments)]
pub async fn run_bulk_client(
    cfg: &crate::config::Config,
    port: u16,
    handshake_timeout: Duration,
    cancel: &CancellationToken,
    ch: &mut BulkChannels,
    health: &Arc<LinkHealth>,
    settings: BulkSettings,
) -> Result<(), String> {
    let framed = crate::net::connect_and_handshake(cfg, port, crate::protocol::Lane::Bulk, handshake_timeout, cancel)
        .await?;
    tracing::info!("bulk lane connected to {}:{}", cfg.server_host, port);
    bulk_pump(into_bulk_framed(framed), ch, cancel, health, settings).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_queue_reports_full_and_drains_back_down() {
        let q = BulkQueue::new(1024 * 1024);
        assert!(!q.is_full(), "a fresh queue has room");
        q.add(1024 * 1024);
        assert!(q.is_full(), "at the limit the producer must wait");
        q.take(512 * 1024);
        assert!(!q.is_full(), "sending frees room again");
        assert_eq!(q.queued(), 512 * 1024);
    }

    #[test]
    fn the_queue_limit_is_never_below_one_chunk() {
        // A limit under one chunk would leave a producer that has just read a chunk with
        // nowhere to put it and no way to make room: a deadlock, not a tight budget.
        let q = BulkQueue::new(1);
        assert_eq!(q.limit(), CHUNK_BYTES as u64);
    }

    #[test]
    fn crediting_more_than_was_queued_cannot_wrap() {
        // If it wrapped, `queued` would become astronomically large and every producer
        // would block forever.
        let q = BulkQueue::new(1024 * 1024);
        q.add(10);
        q.take(999_999);
        assert_eq!(q.queued(), 0);
        assert!(!q.is_full());
    }

    fn roundtrip(f: BulkFrame) -> BulkFrame {
        let mut codec = BulkCodec::new();
        let mut buf = BytesMut::new();
        codec.encode(f.clone(), &mut buf).expect("encode");
        let got = codec.decode(&mut buf).expect("decode").expect("a frame");
        assert!(buf.is_empty(), "codec must consume exactly one frame");
        got
    }

    #[test]
    fn every_frame_kind_survives_the_wire() {
        let big = vec![7u8; CHUNK_BYTES];
        for f in [
            BulkFrame::Begin { transfer: 9, total: 1234, meta: b"{\"kind\":\"image\"}".to_vec() },
            BulkFrame::Chunk { transfer: 9, seq: 41, data: big },
            BulkFrame::End { transfer: 9 },
            BulkFrame::Abort { transfer: 9 },
        ] {
            assert_eq!(roundtrip(f.clone()), f);
        }
    }

    #[test]
    fn a_full_chunk_fits_inside_the_frame_limit() {
        // If the cap were ever set below chunk+header, every transfer would fail at
        // the first full chunk rather than at a boundary anyone would notice in review.
        let mut codec = BulkCodec::new();
        let mut buf = BytesMut::new();
        let f = BulkFrame::Chunk { transfer: 1, seq: 0, data: vec![0u8; CHUNK_BYTES] };
        codec.encode(f, &mut buf).expect("a full chunk must encode");
        assert!(codec.decode(&mut buf).expect("decode").is_some());
    }

    #[test]
    fn partial_frames_decode_to_none_not_an_error() {
        // The stream can split anywhere; a half-arrived frame is normal, not a fault.
        let mut codec = BulkCodec::new();
        let mut buf = BytesMut::new();
        codec
            .encode(BulkFrame::Chunk { transfer: 3, seq: 1, data: vec![1, 2, 3, 4] }, &mut buf)
            .unwrap();
        let full = buf.split();
        let mut partial = BytesMut::from(&full[..full.len() - 2]);
        assert!(codec.decode(&mut partial).unwrap().is_none(), "incomplete -> None");
        partial.extend_from_slice(&full[full.len() - 2..]);
        assert!(codec.decode(&mut partial).unwrap().is_some(), "completed -> frame");
    }

    #[test]
    fn malformed_frames_are_rejected_not_indexed_past() {
        // Attacker-reachable: a frame shorter than the header, and an unknown kind.
        let mut codec = BulkCodec::new();
        let mut inner = LengthDelimitedCodec::builder()
            .max_frame_length(MAX_BULK_FRAME)
            .length_field_type::<u32>()
            .new_codec();

        let mut buf = BytesMut::new();
        inner.encode(BytesMut::from(&b"short"[..]).freeze(), &mut buf).unwrap();
        assert!(codec.decode(&mut buf).is_err(), "a truncated header must be refused");

        let mut buf = BytesMut::new();
        let mut body = BytesMut::new();
        body.put_u8(99); // no such kind
        body.put_u64(1);
        body.put_u32(0);
        body.put_u64(0);
        inner.encode(body.freeze(), &mut buf).unwrap();
        assert!(codec.decode(&mut buf).is_err(), "an unknown kind must be refused");
    }

    #[test]
    fn no_rtt_samples_means_no_throttling() {
        // Throttling on ignorance would make every transfer start slow for no reason.
        let h = LinkHealth::new();
        assert!(!h.is_congested());
        h.record_rtt(Duration::from_millis(20));
        assert!(!h.is_congested(), "one sample is its own baseline");
    }

    #[test]
    fn lan_jitter_does_not_count_as_congestion() {
        // 300 µs -> 600 µs is a doubling, but it is 0.3 ms: invisible, and throttling
        // for it would cost real throughput to protect nothing.
        let h = LinkHealth::new();
        h.record_rtt(Duration::from_micros(300));
        h.record_rtt(Duration::from_micros(600));
        assert!(!h.is_congested(), "sub-millisecond jitter must not throttle");
    }

    #[test]
    fn a_real_latency_rise_counts_as_congestion() {
        let h = LinkHealth::new();
        h.record_rtt(Duration::from_millis(10));
        h.record_rtt(Duration::from_millis(90));
        assert!(h.is_congested(), "9x baseline and +80 ms is exactly what we protect against");
    }

    #[test]
    fn the_pacer_backs_off_hard_and_recovers_gently() {
        let h = LinkHealth::new();
        h.record_rtt(Duration::from_millis(10));
        let mut p = Pacer::new(h.clone(), None);

        // Ramp up while healthy.
        let start = p.rate();
        p.step();
        assert!(p.rate() > start, "a healthy link should speed up");

        // Congestion halves it — decrease must be much faster than increase, or we
        // spend most of the transfer sitting on top of the latency we caused.
        h.record_rtt(Duration::from_millis(200));
        let before = p.rate();
        p.step();
        assert_eq!(p.rate(), before / 2);

        // And it never collapses to a standstill.
        for _ in 0..100 {
            p.step();
        }
        assert_eq!(p.rate(), MIN_RATE_BYTES_PER_SEC);
    }

    #[test]
    fn a_user_set_rate_is_never_overridden() {
        // "Pin it if you'd rather decide" has to mean exactly that, in both directions.
        let h = LinkHealth::new();
        let mut p = Pacer::new(h.clone(), Some(1_000_000));
        h.record_rtt(Duration::from_millis(10));
        h.record_rtt(Duration::from_millis(500));
        p.step();
        assert_eq!(p.rate(), 1_000_000, "congestion must not move a pinned rate");
        h.record_rtt(Duration::from_millis(1));
        p.step();
        assert_eq!(p.rate(), 1_000_000, "nor must a healthy link");
    }

    #[test]
    fn delay_matches_the_rate() {
        let h = LinkHealth::new();
        let p = Pacer::new(h, Some(1024 * 1024)); // 1 MiB/s
        assert_eq!(p.delay_for(1024 * 1024), Duration::from_secs(1));
        assert_eq!(p.delay_for(256 * 1024), Duration::from_millis(250));
    }

    #[test]
    fn crossing_pauses_the_lane_only_when_the_user_asked() {
        let h = LinkHealth::new();
        let p = Pacer::new(h.clone(), None);
        assert!(p.crossed_cap(true).is_none(), "not crossed: no extra ceiling");
        h.set_crossed(true);
        assert_eq!(p.crossed_cap(true), Some(CROSSED_RATE_BYTES_PER_SEC));
        assert!(p.crossed_cap(false).is_none(), "the setting must actually be honoured");
        // The point of the change: crossing slows the transfer, it does not stop it.
        assert!(
            !p.delay_for_capped(CHUNK_BYTES, p.crossed_cap(true)).is_zero(),
            "crossing must slow the lane"
        );
    }

    #[test]
    fn the_baseline_relaxes_so_a_slower_network_becomes_normal() {
        // Move from a LAN to a hotspot: without decay the old 1 ms baseline would make
        // the new 40 ms normal look like permanent congestion and pin the rate at the
        // floor forever.
        let h = LinkHealth::new();
        h.record_rtt(Duration::from_millis(1));
        h.record_rtt(Duration::from_millis(40));
        assert!(h.is_congested());
        for _ in 0..200 {
            h.decay_baseline();
        }
        h.record_rtt(Duration::from_millis(40));
        assert!(!h.is_congested(), "the new normal must eventually be accepted");
    }
}
