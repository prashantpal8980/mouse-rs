//! Pointer-motion tracking for the X11 capture backend.
//!
//! X11 grabs the pointer (which keeps *moving*), warps it back to centre, and reads
//! relative motion as the delta from the previous event, absorbing its own warp echo.
//! The Windows backend uses a different model — it *swallows* moves so the cursor is
//! frozen, and reads each delta straight from that frozen centre (see `windows.rs`) —
//! so it only needs [`MotionGeom`]'s centre, not [`Motion`].
// On Windows only the centre fields of `MotionGeom` are used; the rest is the X11
// warp model, which the Windows build doesn't compile a user for.
#![cfg_attr(target_os = "windows", allow(dead_code))]

/// Fixed geometry of the grabbed screen: centre and dimensions.
#[derive(Clone, Copy)]
pub(crate) struct MotionGeom {
    pub cx: i32,
    pub cy: i32,
    pub w: i32,
    pub h: i32,
}

/// What to do with a processed motion event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MotionAction {
    /// Forward this relative delta to the peer.
    Forward { dx: i32, dy: i32 },
    /// Our own warp echo, or a zero move — forward nothing.
    Ignore,
}

/// Per-grab pointer-motion state.
///
/// Relative deltas are taken from the **previous event position**, not from the
/// centre, so a burst of motion events that the OS batched between our reads still
/// sums to the true displacement. We re-centre (warp) only as the cursor nears an
/// edge, so it never reaches the screen edge (where the OS would clamp it and we'd
/// lose motion). Crucially, the warp's own synthetic motion event (always landing
/// exactly on the centre) must be absorbed rather than forwarded as a huge spurious
/// jump — see the module note on the two per-platform absorption strategies.
pub(crate) struct Motion {
    /// Position of the previous real motion event.
    last: (i32, i32),
    /// Count of warps issued whose centre echo we have not yet absorbed.
    pending_warps: u32,
}

impl Motion {
    pub(crate) fn new(center: (i32, i32)) -> Motion {
        Motion {
            last: center,
            pending_warps: 0,
        }
    }

    /// Process a motion event at absolute `(ex, ey)`. Returns the delta to forward
    /// (if any) and whether the caller should warp to centre afterward. The caller
    /// must call [`Motion::note_warp_issued`] iff it actually performed the warp,
    /// so the pending-echo count stays accurate even if a warp call fails.
    pub(crate) fn on_motion(&mut self, ex: i32, ey: i32, g: &MotionGeom) -> (MotionAction, bool) {
        // A motion event landing exactly on the centre while a warp is outstanding
        // is that warp's synthetic echo, not real movement: re-baseline and drop it.
        // (X11 relies on this; Windows usually absorbs the echo earlier via the
        // dwExtraInfo tag, leaving this as a belt-and-braces fallback.)
        if self.pending_warps > 0 && (ex, ey) == (g.cx, g.cy) {
            self.pending_warps -= 1;
            self.last = (g.cx, g.cy);
            return (MotionAction::Ignore, false);
        }
        let dx = ex - self.last.0;
        let dy = ey - self.last.1;
        self.last = (ex, ey);
        let action = if dx != 0 || dy != 0 {
            MotionAction::Forward { dx, dy }
        } else {
            MotionAction::Ignore
        };
        let near_edge =
            ex < g.w / 4 || ex > g.w - g.w / 4 || ey < g.h / 4 || ey > g.h - g.h / 4;
        (action, near_edge)
    }

    /// Record that a warp-to-centre was issued (its echo is now outstanding).
    pub(crate) fn note_warp_issued(&mut self) {
        self.pending_warps += 1;
    }

    /// Absorb a warp echo identified out-of-band (Windows: a move tagged with our
    /// injection magic). Re-baselines to the centre and clears one pending warp, so
    /// the next *real* delta is measured from the centre the cursor was warped to.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn absorb_warp_echo(&mut self, center: (i32, i32)) {
        self.last = center;
        self.pending_warps = self.pending_warps.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1440x900 screen: centre (720,450), edge bands ex<360 / ex>1080 / ey<225 / ey>675.
    fn geom() -> MotionGeom {
        MotionGeom { cx: 720, cy: 450, w: 1440, h: 900 }
    }

    #[test]
    fn motion_forwards_relative_delta_away_from_edges() {
        let g = geom();
        let mut m = Motion::new((g.cx, g.cy));
        // A move that stays clear of every edge: forward the true delta, no warp.
        assert_eq!(
            m.on_motion(800, 500, &g),
            (MotionAction::Forward { dx: 80, dy: 50 }, false)
        );
        // Next delta is relative to the previous event, not the centre.
        assert_eq!(
            m.on_motion(810, 500, &g),
            (MotionAction::Forward { dx: 10, dy: 0 }, false)
        );
    }

    #[test]
    fn motion_zero_delta_is_ignored() {
        let g = geom();
        let mut m = Motion::new((720, 450));
        assert_eq!(m.on_motion(720, 450, &g), (MotionAction::Ignore, false));
    }

    #[test]
    fn motion_requests_warp_only_near_an_edge() {
        let g = geom();
        let mut m = Motion::new((720, 450));
        let (_, warp) = m.on_motion(300, 450, &g); // ex=300 < w/4=360
        assert!(warp, "should warp when nearing the left edge");
    }

    #[test]
    fn batched_fast_flick_to_edge_does_not_overcount() {
        // The regression the review caught: several real motion events past the edge
        // threshold arrive in one batch *before* the warp echo. Each must forward only
        // its true ~per-event delta — never a spurious ~half-screen jump diffed against
        // the centre — and the warp echoes must net to zero.
        let g = geom();
        let mut m = Motion::new((720, 450));

        // Fast leftward flick; positions cross the w/4 (360) threshold mid-batch.
        let events = [600, 450, 350, 250, 150]; // x; y stays 450 (clear of y-edges)
        let mut forwarded = 0i32;
        let mut warps = 0u32;
        for &x in &events {
            let (action, warp) = m.on_motion(x, 450, &g);
            if let MotionAction::Forward { dx, dy } = action {
                assert_eq!(dy, 0);
                // No single forwarded delta may exceed a sane per-event magnitude;
                // the buggy version produced ~-470 here.
                assert!(dx.abs() <= 200, "spurious large delta {dx} (over-count bug)");
                forwarded += dx;
            }
            if warp {
                m.note_warp_issued();
                warps += 1;
            }
        }
        // Total forwarded equals the true displacement 720 -> 150 = -570.
        assert_eq!(forwarded, 150 - 720, "summed delta must equal true displacement");

        // Now the `warps` synthetic centre echoes arrive: each is absorbed (no
        // forward), leaving pending_warps back at zero.
        for _ in 0..warps {
            assert_eq!(m.on_motion(720, 450, &g), (MotionAction::Ignore, false));
        }
        assert_eq!(m.pending_warps, 0, "all warp echoes absorbed");

        // A subsequent real move now diffs cleanly from the centre baseline.
        assert_eq!(
            m.on_motion(740, 450, &g),
            (MotionAction::Forward { dx: 20, dy: 0 }, false)
        );
    }

    #[test]
    fn warp_echo_absorbed_before_a_real_event() {
        // Single near-edge move -> warp issued -> echo at centre absorbed.
        let g = geom();
        let mut m = Motion::new((720, 450));
        let (action, warp) = m.on_motion(100, 450, &g);
        assert_eq!(action, MotionAction::Forward { dx: -620, dy: 0 });
        assert!(warp);
        m.note_warp_issued();
        // The warp's synthetic centre event forwards nothing and clears the pending.
        assert_eq!(m.on_motion(720, 450, &g), (MotionAction::Ignore, false));
        assert_eq!(m.pending_warps, 0);
    }

    #[test]
    fn absorb_warp_echo_rebaselines_and_clears_pending() {
        // The Windows path: the echo is recognised out-of-band (dwExtraInfo magic)
        // and absorbed directly, rebaselining to centre so the next delta is clean.
        let g = geom();
        let mut m = Motion::new((720, 450));
        let (_, warp) = m.on_motion(100, 450, &g);
        assert!(warp);
        m.note_warp_issued();
        m.absorb_warp_echo((g.cx, g.cy));
        assert_eq!(m.pending_warps, 0);
        // Next real move diffs from the centre, not from x=100.
        assert_eq!(
            m.on_motion(740, 450, &g),
            (MotionAction::Forward { dx: 20, dy: 0 }, false)
        );
    }
}
