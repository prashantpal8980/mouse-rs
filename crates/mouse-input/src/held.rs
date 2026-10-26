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
        self.0.get_mut(vk as usize).map(std::mem::take).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: u16 = 0xA2;

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
