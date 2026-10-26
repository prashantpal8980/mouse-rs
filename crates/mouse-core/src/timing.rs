//! Once-a-minute timing summaries for the log, so "the pointer feels laggy on Wi-Fi"
//! can be read as numbers: the link's round trip, and how evenly pointer motion
//! arrives on the machine being driven. Both are cheap enough for the hot path:
//! a push into a small Vec, and one log line a minute.

use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);

/// A gap between two moves longer than this, inside continuous motion, shows as a
/// visible hitch (more than a frame at 60 Hz, with margin).
const HITCH: Duration = Duration::from_millis(25);

/// A gap longer than this is the hand pausing, not the network: not counted.
const PAUSE: Duration = Duration::from_millis(150);

/// Round-trip times of the heartbeat, summarised once a minute.
pub struct RttLog {
    since: Instant,
    ms: Vec<f64>,
}

impl Default for RttLog {
    fn default() -> Self {
        RttLog { since: Instant::now(), ms: Vec::new() }
    }
}

impl RttLog {
    /// Record one round trip. Returns the summary line when a minute has passed.
    pub fn push(&mut self, rtt: Duration, now: Instant) -> Option<String> {
        self.ms.push(rtt.as_secs_f64() * 1000.0);
        if now.duration_since(self.since) < WINDOW {
            return None;
        }
        self.since = now;
        let mut ms = std::mem::take(&mut self.ms);
        ms.sort_by(|a, b| a.total_cmp(b));
        let slow = ms.iter().filter(|&&m| m > 50.0).count();
        Some(format!(
            "link round trip over the last minute: min {:.1} ms, median {:.1} ms, max {:.1} ms, \
             {slow} of {} over 50 ms",
            ms[0],
            ms[ms.len() / 2],
            ms[ms.len() - 1],
            ms.len()
        ))
    }
}

/// How evenly pointer moves arrive on the driven machine, summarised once a minute.
pub struct MotionLog {
    since: Instant,
    last: Option<Instant>,
    moves: u32,
    hitches: u32,
    worst: Duration,
}

impl Default for MotionLog {
    fn default() -> Self {
        MotionLog { since: Instant::now(), last: None, moves: 0, hitches: 0, worst: Duration::ZERO }
    }
}

impl MotionLog {
    /// Record one move arriving at `now`. Returns the summary line when a minute has
    /// passed and there was motion in it.
    pub fn moved(&mut self, now: Instant) -> Option<String> {
        if let Some(last) = self.last.replace(now) {
            let gap = now.duration_since(last);
            if gap < PAUSE {
                self.moves += 1;
                if gap > HITCH {
                    self.hitches += 1;
                }
                self.worst = self.worst.max(gap);
            }
        }
        if now.duration_since(self.since) < WINDOW || self.moves == 0 {
            return None;
        }
        let line = format!(
            "pointer motion over the last minute: {} moves, {} hitches over {} ms ({:.2}%), \
             longest gap {} ms",
            self.moves,
            self.hitches,
            HITCH.as_millis(),
            self.hitches as f64 * 100.0 / self.moves as f64,
            self.worst.as_millis()
        );
        *self = MotionLog { since: now, last: Some(now), ..Default::default() };
        Some(line)
    }

    /// Control left this machine: the next move starts fresh motion, not a long gap.
    pub fn paused(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_summary_comes_once_a_minute() {
        let t0 = Instant::now();
        let mut log = RttLog { since: t0, ms: Vec::new() };
        assert!(log.push(Duration::from_millis(2), t0 + Duration::from_secs(2)).is_none());
        assert!(log.push(Duration::from_millis(80), t0 + Duration::from_secs(4)).is_none());
        let line = log.push(Duration::from_millis(4), t0 + Duration::from_secs(61)).unwrap();
        assert!(line.contains("min 2.0 ms") && line.contains("max 80.0 ms"), "{line}");
        assert!(line.contains("1 of 3 over 50 ms"), "{line}");
        assert!(log.ms.is_empty());
    }

    #[test]
    fn motion_counts_hitches_but_not_pauses_or_crossings() {
        let t0 = Instant::now();
        let mut log = MotionLog { since: t0, ..Default::default() };
        let mut t = t0;
        for _ in 0..10 {
            t += Duration::from_millis(8);
            assert!(log.moved(t).is_none());
        }
        t += Duration::from_millis(40); // a hitch
        log.moved(t);
        t += Duration::from_secs(1); // a pause
        log.moved(t);
        log.paused();
        t += Duration::from_millis(60); // first move after a crossing: not a gap
        log.moved(t);
        let line = log.moved(t0 + Duration::from_secs(61)).unwrap();
        assert!(line.contains("1 hitches"), "{line}");
        assert!(line.contains("longest gap 40 ms"), "{line}");
    }
}
