//! Smooth pointer playback on the receiving machine (owner's go-ahead, 27 Oct 2026).
//!
//! With Bluetooth audio on a Wi-Fi + Bluetooth combo chip (Kali's RTL8821CE), the driver
//! gives Wi-Fi only short windows in a 4-slot schedule tied to the router's 102.4 ms beacon.
//! Moves sent in between are held back and arrive in bursts: a recording of the owner's own
//! movement had the pointer frozen in 30.6% of frames with plain "jump to the newest", the way
//! Barrier, Input Leap, Deskflow and Lan Mouse all do it.
//!
//! So moves are played back the way online games draw remote players: a small, steady time
//! behind the sender, moving smoothly between the positions already received, and predicting
//! forward only when data is late. On that same recording, 40–60 ms of delay plus up to 60 ms
//! of prediction left 2.5–4% of frames frozen. The delay adapts: it follows how late moves
//! actually arrive (the 99th percentile over the last few seconds, up to a maximum), so on a
//! clean link (cable, 5 GHz, no Bluetooth audio) it stays near zero and nothing is added.
//!
//! Every move carries the sender's clock (`sent`, ms) and the running total, so this needs
//! nothing else from the network: a lost or late datagram only changes when a position is
//! known, never what it is.

use std::collections::VecDeque;

/// How long the playback delay is chosen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Mode {
    /// Follow the link: the 99th percentile of lateness, never more than `max_ms`.
    Auto { max_ms: f64 },
    /// Always this much (for the owner's feel test).
    Fixed(f64),
}

impl Mode {
    /// From `MOUSE_RS_SMOOTH`: unset or "auto" → auto up to [`AUTO_MAX_MS`]; "auto:<ms>" → auto
    /// up to that; a number → fixed at that many ms ("0" = no delay, prediction only).
    pub(crate) fn from_env(value: Option<&str>) -> Mode {
        let v = value.map(str::trim).unwrap_or("auto");
        if let Some(max) = v.strip_prefix("auto:").and_then(|m| m.parse::<f64>().ok()) {
            return Mode::Auto { max_ms: max.clamp(0.0, 200.0) };
        }
        match v.parse::<f64>() {
            Ok(ms) => Mode::Fixed(ms.clamp(0.0, 200.0)),
            Err(_) => Mode::Auto { max_ms: AUTO_MAX_MS },
        }
    }
}

/// The most an automatic delay may add.
pub(crate) const AUTO_MAX_MS: f64 = 60.0;
/// How far ahead of the newest known position the pointer may be predicted, and from where
/// the prediction eases off to a stop.
const PREDICT_MS: f64 = 60.0;
const PREDICT_FULL_MS: f64 = 40.0;
/// The longest the pointer waits for real data to catch up with a guess, ms.
const HOLD_MAX_MS: f64 = 100.0;
/// The window the automatic delay looks back over.
const WINDOW_MS: f64 = 3000.0;
/// How fast the delay may change: ms of delay per ms of time. Playback speeds up or slows
/// down by at most this fraction, which the eye doesn't notice.
const SLEW: f64 = 0.1;
/// How fast the delay comes back down: a tenth of the way up.
const SLEW_DOWN: f64 = 0.01;
/// Already-played positions kept for measuring speed.
const PLAYED_KEPT: usize = 32;
/// Speed is measured over at least this much of the sender's time.
const SPEED_SPAN_MS: f64 = 8.0;
/// A move counts as overdue only once this many of the usual gaps between moves have
/// passed. Before that, the next one is simply not due yet, and guessing ahead of it is
/// what made the pointer shake back and forth on a clean cable (owner, 27 Oct 2026: 3,498
/// guessed frames for 1,235 real moves in a minute, at a 1 ms delay).
const OVERDUE_GAPS: f64 = 1.5;

/// A position or a velocity, in pixels (or pixels per ms).
type Point = (f64, f64);

#[derive(Clone, Copy, Debug)]
struct Sample {
    seq: u64,
    /// Sender's clock, ms.
    sent: f64,
    pos: (i64, i64),
}

pub(crate) struct Playout {
    mode: Mode,
    /// Received positions, ordered by `seq`, from the one being played onwards.
    samples: VecDeque<Sample>,
    /// The last few positions already played, for measuring speed.
    played: VecDeque<Sample>,
    /// The fastest transit seen (arrival on this clock minus send on the sender's): the clock
    /// offset plus the network's best case.
    offset: Option<f64>,
    /// (arrival, lateness above the fastest) over the last [`WINDOW_MS`], for the automatic
    /// delay.
    late: VecDeque<(f64, f64)>,
    /// The delay in use now, ms.
    delay: f64,
    last_tick: Option<f64>,
    /// The sender repeated its newest total: motion has stopped there, predict nothing past it.
    stopped_at: Option<u64>,
    /// The position last returned, and while it was ahead of real data on a guess, the
    /// direction of that guess and when guessing began.
    shown: Option<(f64, f64)>,
    lead: Option<((f64, f64), f64)>,
    /// Renders that were a prediction, for the per-minute counters.
    pub(crate) predicted: u64,
}

impl Playout {
    pub(crate) fn new(mode: Mode) -> Playout {
        let delay = match mode {
            Mode::Fixed(ms) => ms,
            Mode::Auto { .. } => 0.0,
        };
        Playout {
            mode,
            samples: VecDeque::new(),
            played: VecDeque::new(),
            offset: None,
            late: VecDeque::new(),
            delay,
            last_tick: None,
            stopped_at: None,
            shown: None,
            lead: None,
            predicted: 0,
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// The delay in use now, ms.
    pub(crate) fn delay_ms(&self) -> f64 {
        self.delay
    }

    /// A datagram arrived at `now` (ms, this clock) with the total `pos` the sender had at
    /// `sent` (ms, its clock).
    pub(crate) fn push(&mut self, seq: u64, sent: f64, pos: (i64, i64), now: f64) {
        let transit = now - sent;
        let best = self.offset.map_or(transit, |o| o.min(transit));
        self.offset = Some(best);
        self.late.push_back((now, transit - best));
        while self.late.front().is_some_and(|(t, _)| now - t > WINDOW_MS) {
            self.late.pop_front();
        }
        let newest = self.samples.back().map(|s| s.seq);
        match newest {
            Some(n) if seq == n => self.stopped_at = Some(seq), // a repeat: the hand stopped
            Some(n) if seq < n => {
                // Out of order: keep it if it is still ahead of what is playing, for a
                // smoother path; otherwise it is history.
                if self.samples.front().is_some_and(|f| seq > f.seq) {
                    let at = self.samples.iter().position(|s| s.seq > seq).unwrap_or(self.samples.len());
                    let duplicate = at > 0 && self.samples[at - 1].seq == seq;
                    if !duplicate {
                        self.samples.insert(at, Sample { seq, sent, pos });
                    }
                }
            }
            _ => {
                self.samples.push_back(Sample { seq, sent, pos });
                self.stopped_at = None;
            }
        }
    }

    /// A position sync over TCP: a click or key follows, so the pointer goes exactly there
    /// now. Anything older is dropped; playback carries on from here.
    pub(crate) fn sync(&mut self, seq: u64, pos: (i64, i64), now: f64) {
        let here = self.play_time(now).unwrap_or(f64::MIN);
        self.samples.retain(|s| s.seq > seq);
        self.played.clear();
        self.shown = None;
        self.lead = None;
        self.samples.push_front(Sample { seq, sent: here, pos });
        if self.samples.len() == 1 {
            self.stopped_at = Some(seq);
        }
    }

    /// Whether playback may still move the pointer without new data: positions waiting to be
    /// played, or a prediction under way.
    pub(crate) fn active(&self, now: f64) -> bool {
        let (Some(ts), Some(newest)) = (self.play_time(now), self.samples.back()) else {
            return false;
        };
        ts < newest.sent || (self.stopped_at != Some(newest.seq) && ts - newest.sent < PREDICT_MS)
    }

    /// The sender's time being played at `now`.
    fn play_time(&self, now: f64) -> Option<f64> {
        Some(now - self.offset? - self.delay)
    }

    /// Where the pointer should be at `now`, in the sender's total coordinates.
    ///
    /// A prediction is never taken back while the hand is still moving: if real data lands
    /// behind where a guess already put the pointer, the pointer waits there for the real path
    /// to catch up, instead of stepping back. Stepping back was the small left-right jitter
    /// the owner felt in the first feel test (27 Oct): replaying the recording, prediction
    /// turned 0.2% of frames into back-and-forth wobbles at 3.2%. The pointer does go back when
    /// the hand really stopped (the sender's repeat), after a click's sync, or after waiting
    /// [`HOLD_MAX_MS`].
    pub(crate) fn target(&mut self, now: f64) -> Option<(f64, f64)> {
        let (want, guessed, v) = self.raw_target(now)?;
        if let (Some(shown), Some((dir, since))) = (self.shown, self.lead) {
            let stopped = self.samples.back().is_some_and(|n| self.stopped_at == Some(n.seq));
            let back = (want.0 - shown.0) * dir.0 + (want.1 - shown.1) * dir.1;
            if back < 0.0 && !stopped && now - since < HOLD_MAX_MS {
                return Some(shown);
            }
            if !guessed {
                self.lead = None;
            }
        }
        if guessed {
            let speed = v.0.hypot(v.1);
            if speed > 0.0 {
                let since = self.lead.map_or(now, |(_, t)| t);
                self.lead = Some(((v.0 / speed, v.1 / speed), since));
            }
        }
        self.shown = Some(want);
        Some(want)
    }

    /// The playback position at `now`, whether it is a guess, and the speed used for it.
    fn raw_target(&mut self, now: f64) -> Option<(Point, bool, Point)> {
        self.adjust_delay(now);
        let ts = self.play_time(now)?;
        // Drop what has been fully played, keeping the one being played.
        while self.samples.len() >= 2 && self.samples[1].sent <= ts {
            if let Some(done) = self.samples.pop_front() {
                self.played.push_back(done);
                if self.played.len() > PLAYED_KEPT {
                    self.played.pop_front();
                }
            }
        }
        let a = *self.samples.front()?;
        let as_f = |p: (i64, i64)| (p.0 as f64, p.1 as f64);
        if ts < a.sent {
            // Before anything known (the first sample, or right after a sync): hold.
            return Some((as_f(a.pos), false, (0.0, 0.0)));
        }
        if let Some(b) = self.samples.get(1) {
            // Between two known positions: move smoothly from one to the other.
            let span = (b.sent - a.sent).max(f64::EPSILON);
            let k = ((ts - a.sent) / span).clamp(0.0, 1.0);
            let at = (
                a.pos.0 as f64 + (b.pos.0 - a.pos.0) as f64 * k,
                a.pos.1 as f64 + (b.pos.1 - a.pos.1) as f64 * k,
            );
            return Some((at, false, (0.0, 0.0)));
        }
        // Past the newest known position: predict, unless the hand has stopped there.
        if self.stopped_at == Some(a.seq) {
            return Some((as_f(a.pos), false, (0.0, 0.0)));
        }
        let Some(v) = self.speed() else { return Some((as_f(a.pos), false, (0.0, 0.0))) };
        // Guess only for the part of the wait the next move is overdue for.
        let into = ts - a.sent - OVERDUE_GAPS * self.usual_gap();
        let k = eased(into);
        if k > 0.0 {
            self.predicted += 1;
        }
        Some(((a.pos.0 as f64 + v.0 * k, a.pos.1 as f64 + v.1 * k), k > 0.0, v))
    }

    /// The usual time between two moves on the sender's clock: the median of the recent
    /// gaps, within 2..=30 ms. 8 ms until there is anything to measure.
    fn usual_gap(&self) -> f64 {
        let times: Vec<f64> = self.played.iter().chain(self.samples.iter()).map(|s| s.sent).collect();
        let mut gaps: Vec<f64> = times.windows(2).rev().take(16).map(|w| w[1] - w[0]).filter(|g| *g > 0.0).collect();
        if gaps.is_empty() {
            return 8.0;
        }
        gaps.sort_by(f64::total_cmp);
        gaps[gaps.len() / 2].clamp(2.0, 30.0)
    }

    /// Speed (px per ms of the sender's time) at the newest position, over at least
    /// [`SPEED_SPAN_MS`] of recent positions (waiting or already played).
    fn speed(&self) -> Option<(f64, f64)> {
        let newest = self.samples.back()?;
        let older = || self.samples.iter().rev().skip(1).chain(self.played.iter().rev());
        let earlier = older()
            .find(|s| newest.sent - s.sent >= SPEED_SPAN_MS)
            .or_else(|| older().next())?;
        let dt = newest.sent - earlier.sent;
        if !(1.0..=60.0).contains(&dt) {
            return None;
        }
        Some((
            (newest.pos.0 - earlier.pos.0) as f64 / dt,
            (newest.pos.1 - earlier.pos.1) as f64 / dt,
        ))
    }

    fn adjust_delay(&mut self, now: f64) {
        let Mode::Auto { max_ms } = self.mode else {
            self.last_tick = Some(now);
            return;
        };
        let dt = self.last_tick.map_or(0.0, |t| (now - t).max(0.0));
        self.last_tick = Some(now);
        if self.late.is_empty() {
            return;
        }
        let mut late: Vec<f64> = self.late.iter().map(|(_, l)| *l).collect();
        late.sort_by(f64::total_cmp);
        let want = late[(late.len() * 99 / 100).min(late.len() - 1)].clamp(0.0, max_ms);
        // Rise fast when moves start arriving late; come down slowly once they stop, so a calm
        // second between pauses doesn't pull playback into guessing just before the next one.
        let (up, down) = (SLEW * dt, SLEW_DOWN * dt);
        self.delay += (want - self.delay).clamp(-down, up);
    }
}

/// How far (in ms of motion) to carry the pointer `into` ms past the newest known position:
/// at full speed up to [`PREDICT_FULL_MS`], easing to a stop at [`PREDICT_MS`].
fn eased(into: f64) -> f64 {
    if into <= 0.0 {
        0.0
    } else if into <= PREDICT_FULL_MS {
        into
    } else {
        let span = PREDICT_MS - PREDICT_FULL_MS;
        let over = (into - PREDICT_FULL_MS).min(span);
        PREDICT_FULL_MS + over - over * over / (2.0 * span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steady motion at 1 px/ms to the right, a move every 4 ms, arriving `lag(i)` ms after
    /// it was sent (on top of a 1000 ms clock offset).
    fn feed(p: &mut Playout, from: u64, n: u64, lag: impl Fn(u64) -> f64) {
        for i in from..from + n {
            let sent = 4.0 * i as f64;
            p.push(i + 1, sent, (4 * i as i64, 0), sent + 1000.0 + lag(i));
        }
    }

    #[test]
    fn a_clean_link_never_guesses_between_moves_so_the_pointer_cannot_shake() {
        // A hand slowing down, a move every 8 ms, 0–1 ms of jitter, drawn every 2 ms.
        let mut p = Playout::new(Mode::Auto { max_ms: 60.0 });
        let (mut x, mut step, mut last_shown) = (0i64, 20i64, f64::MIN);
        for i in 0..120u64 {
            let sent = 8.0 * i as f64;
            let arrive = sent + 1000.0 + (i % 2) as f64;
            p.push(i + 1, sent, (x, 0), arrive);
            for k in 0..4 {
                let (shown, _) = p.target(arrive + 2.0 * k as f64).unwrap();
                assert!(shown >= last_shown, "went back from {last_shown} to {shown} at move {i}");
                // Ahead of the hand is what has to be pulled back later: that is the shake.
                assert!(shown <= x as f64 + 0.01, "ran ahead of the hand: {shown} > {x} at move {i}");
                last_shown = shown;
            }
            x += step;
            if i % 10 == 9 {
                step = (step - 2).max(1);
            }
        }
        assert_eq!(p.predicted, 0, "nothing guessed on a clean link");
    }

    #[test]
    fn a_clean_link_adds_no_delay() {
        let mut p = Playout::new(Mode::Auto { max_ms: 60.0 });
        let mut t = 0.0;
        for i in 0..200u64 {
            let sent = 4.0 * i as f64;
            t = sent + 1000.0 + (i % 2) as f64; // 0–1 ms of jitter
            p.push(i + 1, sent, (4 * i as i64, 0), t);
            p.target(t);
        }
        assert!(p.delay_ms() <= 1.0, "delay {}", p.delay_ms());
        let (x, _) = p.target(t).unwrap();
        assert!((x - 796.0).abs() <= 4.0, "pointer at {x}, sender at 796");
    }

    #[test]
    fn a_fixed_delay_plays_the_sender_that_far_behind_smoothly() {
        let mut p = Playout::new(Mode::Fixed(50.0));
        feed(&mut p, 0, 100, |_| 0.0);
        // At receiver time 1000 + 300, the sender was at 300 ms → 300 px; 50 ms behind → 250.
        let (x, _) = p.target(1300.0).unwrap();
        assert!((x - 250.0).abs() < 0.01, "{x}");
        // Halfway between two samples, halfway between their positions.
        let (x, _) = p.target(1302.0).unwrap();
        assert!((x - 252.0).abs() < 0.01, "{x}");
    }

    #[test]
    fn late_data_is_predicted_then_corrected() {
        let mut p = Playout::new(Mode::Fixed(0.0));
        feed(&mut p, 0, 20, |_| 0.0); // newest: sent 76 ms, at 76 px
        // Overdue 6 ms after the newest move (1.5 × the usual 4 ms), so 36 ms in is 30 ms of guessing.
        let (x, _) = p.target(1076.0 + 36.0).unwrap();
        assert!((x - 106.0).abs() < 0.5, "30 ms of guessing at 1 px/ms: {x}");
        let (x, _) = p.target(1076.0 + 200.0).unwrap();
        assert!(x <= 76.0 + 6.0 + PREDICT_MS + 0.5, "never beyond the prediction limit: {x}");
        // The held-back moves arrive: the real path takes over.
        feed(&mut p, 20, 60, |_| 0.0);
        let (x, _) = p.target(1076.0 + 200.0).unwrap();
        assert!((x - 276.0).abs() < 0.5, "{x}");
    }

    #[test]
    fn a_guess_is_not_taken_back_while_the_hand_keeps_moving() {
        let mut p = Playout::new(Mode::Fixed(0.0));
        feed(&mut p, 0, 20, |_| 0.0); // at 76 px, 1 px/ms
        let (x, _) = p.target(1076.0 + 36.0).unwrap(); // guessed on to 106
        assert!((x - 106.0).abs() < 0.5, "{x}");
        // Real data: the hand slowed to 0.5 px/ms, so it is only at 91 at that time.
        for i in 0..8u64 {
            let sent = 80.0 + 4.0 * i as f64;
            p.push(21 + i, sent, (78 + 2 * i as i64, 0), 1112.0);
        }
        let (x, _) = p.target(1112.0).unwrap();
        assert!(x >= 106.0 - 0.01, "waits at the guess, doesn't step back: {x}");
        // The hand stops there: the repeat says so, and the pointer goes to the true spot.
        p.push(28, 108.0, (92, 0), 1114.0);
        let (x, _) = p.target(1116.0).unwrap();
        assert_eq!(x, 92.0);
    }

    #[test]
    fn nothing_is_predicted_past_a_stop() {
        let mut p = Playout::new(Mode::Fixed(0.0));
        feed(&mut p, 0, 20, |_| 0.0);
        p.push(20, 76.0, (76, 0), 1101.0); // the sender's repeat of its last total
        let (x, _) = p.target(1076.0 + 30.0).unwrap();
        assert_eq!(x, 76.0);
        assert!(!p.active(1076.0 + 30.0));
    }

    #[test]
    fn a_sync_lands_exactly_and_playback_never_goes_back() {
        let mut p = Playout::new(Mode::Fixed(60.0));
        feed(&mut p, 0, 50, |_| 0.0); // sender at 196 px
        // Playing 60 ms behind at receiver 1196: about 136 px. A click: sync to 196 now.
        p.sync(50, (196, 0), 1196.0);
        let (x, _) = p.target(1196.0).unwrap();
        assert_eq!(x, 196.0);
        let (x, _) = p.target(1220.0).unwrap();
        assert!(x >= 196.0, "no going back to positions before the click: {x}");
    }

    #[test]
    fn regular_pauses_raise_the_automatic_delay_up_to_its_limit() {
        let mut p = Playout::new(Mode::Auto { max_ms: 60.0 });
        // Every 25th move held back 90 ms, its neighbours a little: like the coexistence pauses.
        for i in 0..1000u64 {
            let sent = 4.0 * i as f64;
            let lag = match i % 25 { 0..=4 => 90.0 - 18.0 * (i % 25) as f64, _ => 1.0 };
            let at = sent + 1000.0 + lag;
            p.push(i + 1, sent, (4 * i as i64, 0), at);
            p.target(at);
        }
        let d = p.delay_ms();
        assert!(d > 20.0 && d <= 60.0, "delay {d}");
    }

    /// Replays a recorded trace (`MOUSE_RS_TRACE_IN`: arrival µs, sent ms, seq, x, y) through
    /// playback in the mode `MOUSE_RS_SMOOTH`, rendering every 1 ms, and writes "ms,x,y" to
    /// `MOUSE_RS_TRACE_OUT`. For scoring strategies against real movement; skipped normally.
    #[test]
    #[ignore]
    fn replay_a_recorded_trace() {
        use std::io::Write;
        let input = std::env::var("MOUSE_RS_TRACE_IN").expect("MOUSE_RS_TRACE_IN");
        let output = std::env::var("MOUSE_RS_TRACE_OUT").expect("MOUSE_RS_TRACE_OUT");
        let mut rows: Vec<(f64, f64, u64, i64, i64)> = std::fs::read_to_string(input)
            .unwrap()
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(',').collect();
                (f.len() == 5).then(|| {
                    (
                        f[0].parse::<f64>().unwrap() / 1000.0,
                        f[1].parse().unwrap(),
                        f[2].parse().unwrap(),
                        f[3].parse().unwrap(),
                        f[4].parse().unwrap(),
                    )
                })
            })
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut p = Playout::new(Mode::from_env(std::env::var("MOUSE_RS_SMOOTH").ok().as_deref()));
        let mut out = std::io::BufWriter::new(std::fs::File::create(output).unwrap());
        let (start, end) = (rows[0].0, rows[rows.len() - 1].0);
        let (mut i, mut t, mut last) = (0, start, (0.0, 0.0));
        while t <= end {
            while i < rows.len() && rows[i].0 <= t {
                let r = rows[i];
                p.push(r.2, r.1, (r.3, r.4), r.0);
                i += 1;
            }
            if let Some(at) = p.target(t) {
                last = (at.0.round(), at.1.round());
            }
            writeln!(out, "{t},{},{},{}", last.0, last.1, p.delay_ms()).unwrap();
            t += 1.0;
        }
    }

    #[test]
    fn the_setting_reads_auto_fixed_and_a_custom_maximum() {
        assert_eq!(Mode::from_env(None), Mode::Auto { max_ms: AUTO_MAX_MS });
        assert_eq!(Mode::from_env(Some("auto")), Mode::Auto { max_ms: AUTO_MAX_MS });
        assert_eq!(Mode::from_env(Some("90")), Mode::Fixed(90.0));
        assert_eq!(Mode::from_env(Some("auto:40")), Mode::Auto { max_ms: 40.0 });
        assert_eq!(Mode::from_env(Some("nonsense")), Mode::Auto { max_ms: AUTO_MAX_MS });
    }
}
