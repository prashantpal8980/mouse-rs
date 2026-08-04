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
        if self.rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_micros((bytes as u64).saturating_mul(1_000_000) / self.rate)
    }

    /// Should the lane stand down entirely right now?
    ///
    /// True while the user is driving the other machine — the moment lag is most
    /// noticeable, and a few seconds of pause the transfer will not miss.
    pub fn should_pause(&self, pause_while_crossed: bool) -> bool {
        pause_while_crossed && self.health.crossed()
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

/// Channels the app uses to talk to the bulk lane.
pub struct BulkChannels {
    /// Frames the app wants to send to the peer.
    pub outgoing: mpsc::UnboundedReceiver<BulkFrame>,
    /// Frames received from the peer.
    pub incoming: mpsc::UnboundedSender<BulkFrame>,
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
    },
}

impl BulkServer {
    pub fn disabled() -> BulkServer {
        BulkServer::Disabled
    }

    pub fn enabled(channels: BulkChannels, health: Arc<LinkHealth>) -> BulkServer {
        BulkServer::Enabled {
            channels: Arc::new(tokio::sync::Mutex::new(Some(channels))),
            health,
        }
    }

    /// Adopt a freshly handshaken bulk connection. Returns immediately; the session
    /// runs as its own task so the input accept loop is never held up by it.
    pub fn take_server_connection(
        &self,
        framed: Framed<LinkStream, MessageCodec>,
        cancel: &CancellationToken,
    ) {
        let BulkServer::Enabled { channels, health } = self else {
            tracing::debug!("a peer asked for the bulk lane, but it is disabled here");
            return;
        };
        let framed = into_bulk_framed(framed);
        let channels = channels.clone();
        let health = health.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            // One bulk session at a time. A second connection means the peer
            // reconnected, so the previous one is already dead; the guard is held for
            // the session's lifetime, which makes "already running" impossible rather
            // than merely unlikely.
            let mut guard = channels.lock().await;
            let Some(ch) = guard.as_mut() else { return };
            tracing::info!("bulk lane connected");
            bulk_pump(framed, ch, &cancel, &health).await;
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
) {
    use futures::{SinkExt, StreamExt};
    let (mut sink, mut stream) = framed.split();
    // Rate is re-evaluated per frame from the input link's own latency.
    let mut pacer = Pacer::new(health.clone(), None);

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
                    // Hold the paced rate *before* writing, so the delay is spent idle
                    // rather than inside a socket write we could no longer interrupt.
                    let bytes = frame.payload_len();
                    if bytes > 0 {
                        pacer.step();
                        let delay = pacer.delay_for(bytes);
                        if !delay.is_zero() {
                            tokio::select! {
                                _ = cancel.cancelled() => return,
                                _ = tokio::time::sleep(delay) => {}
                            }
                        }
                    }
                    let sent = tokio::select! {
                        _ = cancel.cancelled() => return,
                        r = sink.send(frame) => r,
                    };
                    if let Err(e) = sent {
                        tracing::warn!("bulk send failed: {e}");
                        return;
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
pub async fn run_bulk_client(
    cfg: &crate::config::Config,
    port: u16,
    handshake_timeout: Duration,
    cancel: &CancellationToken,
    ch: &mut BulkChannels,
    health: &Arc<LinkHealth>,
) -> Result<(), String> {
    let framed = crate::net::connect_and_handshake(cfg, port, crate::protocol::Lane::Bulk, handshake_timeout, cancel)
        .await?;
    tracing::info!("bulk lane connected to {}:{}", cfg.server_host, port);
    bulk_pump(into_bulk_framed(framed), ch, cancel, health).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!p.should_pause(true), "not crossed: nothing to pause for");
        h.set_crossed(true);
        assert!(p.should_pause(true));
        assert!(!p.should_pause(false), "the setting must actually be honoured");
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
