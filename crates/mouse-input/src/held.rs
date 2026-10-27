//! Which keys the local OS has seen go down and not yet come up (B13).
//!
//! While input crosses to the peer, the capture swallows every key event so the
//! local apps see nothing. That includes the *release* of a key whose *press* the
//! local OS already saw: hold Ctrl, let the pointer cross, let go of Ctrl on the
//! other side, and this machine never hears the release. Its Ctrl then stays down:
//! the wheel zooms instead of scrolling, and the next crossing forwards the stuck
//! Ctrl to the peer, where Alt+Tab stops working. One tap of Ctrl fixed both, which
//! is how the owner found the trigger on 26 Oct 2026.
//!
//! Tracking is from events only: no polling of key state, which B14 showed can't
//! see a key the hook has swallowed.

/// Keys, by virtual-key code, that went down while input was local.
pub(crate) struct LocallyHeld([bool; 256]);

impl Default for LocallyHeld {
    fn default() -> Self {
        LocallyHeld([false; 256])
    }
}

impl LocallyHeld {
    /// Record a key event the local OS received.
    pub(crate) fn note(&mut self, vk: u16, pressed: bool) {
        if let Some(slot) = self.0.get_mut(vk as usize) {
            *slot = pressed;
        }
    }

    /// A release arrived while input is on the peer. True if the local OS saw this
    /// key go down, so the release must reach it too; the key is then forgotten.
    pub(crate) fn take_release(&mut self, vk: u16) -> bool {
        self.0
            .get_mut(vk as usize)
            .map(std::mem::take)
            .unwrap_or(false)
    }
}

/// The last event the keyboard hook saw for each key, swallowed or not: the hook sees
/// every physical key event, so a key whose last event was a release is up on the
/// keyboard, whatever Windows' own key state still says (B13, 27 Oct 2026).
pub(crate) struct LastSeen([bool; 256]);

impl Default for LastSeen {
    fn default() -> Self {
        LastSeen([false; 256])
    }
}

impl LastSeen {
    pub(crate) fn note(&mut self, vk: u16, pressed: bool) {
        if let Some(slot) = self.0.get_mut(vk as usize) {
            *slot = pressed;
        }
    }

    pub(crate) fn is_down(&self, vk: u16) -> bool {
        self.0.get(vk as usize).copied().unwrap_or(false)
    }
}

/// Raw Input names Shift, Ctrl and Alt without a side (0x10, 0x11, 0x12); the hook
/// names the side (0xA0..=0xA5). Convert, so both talk about the same key.
pub(crate) fn sided_vk(vkey: u16, make_code: u16, e0: bool) -> u16 {
    match vkey {
        0x10 if make_code == 0x36 => 0xA1,
        0x10 => 0xA0,
        0x11 if e0 => 0xA3,
        0x11 => 0xA2,
        0x12 if e0 => 0xA5,
        0x12 => 0xA4,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: u16 = 0xA2;

    #[test]
    fn raw_input_modifiers_get_their_side() {
        assert_eq!(sided_vk(0x11, 0x1D, false), 0xA2, "left Ctrl");
        assert_eq!(sided_vk(0x11, 0x1D, true), 0xA3, "right Ctrl");
        assert_eq!(sided_vk(0x10, 0x2A, false), 0xA0, "left Shift");
        assert_eq!(sided_vk(0x10, 0x36, false), 0xA1, "right Shift");
        assert_eq!(sided_vk(0x12, 0x38, true), 0xA5, "AltGr");
        assert_eq!(sided_vk(0x41, 0x1E, false), 0x41, "letters pass through");
    }

    #[test]
    fn last_seen_follows_the_latest_event() {
        let mut seen = LastSeen::default();
        assert!(!seen.is_down(CTRL));
        seen.note(CTRL, true);
        assert!(seen.is_down(CTRL));
        seen.note(CTRL, false);
        assert!(!seen.is_down(CTRL));
        assert!(!seen.is_down(999));
    }

    #[test]
    fn a_release_after_crossing_reaches_the_os_that_saw_the_press() {
        let mut held = LocallyHeld::default();
        held.note(CTRL, true); // pressed here, then the pointer crossed
        assert!(held.take_release(CTRL), "the OS must see Ctrl come up");
        assert!(!held.take_release(CTRL), "only once");
    }

    #[test]
    fn a_key_pressed_and_released_on_the_peer_side_stays_with_the_peer() {
        let mut held = LocallyHeld::default();
        held.note(CTRL, true);
        held.note(CTRL, false); // released before crossing
        assert!(!held.take_release(CTRL));
    }

    #[test]
    fn out_of_range_codes_are_ignored() {
        let mut held = LocallyHeld::default();
        held.note(999, true);
        assert!(!held.take_release(999));
    }
}
