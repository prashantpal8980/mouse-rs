//! Combo hotkey parsing + matching for the return/escape key.
//!
//! Matched in the **engine** (platform-agnostic, so it runs on whichever machine is
//! the server and is the same code the unit tests exercise), from the captured key
//! stream: the engine keeps a running modifier bitset and fires when the trigger key
//! is pressed with all required modifiers held. This is how a combo like
//! `ctrl+win+alt+esc` returns control. The single-key `lock_hotkey` (Scroll Lock)
//! remains a separate in-proc failsafe in the capture backends.

use mouse_input::Key;

const MOD_CTRL: u8 = 1 << 0;
const MOD_ALT: u8 = 1 << 1;
const MOD_SHIFT: u8 = 1 << 2;
const MOD_SUPER: u8 = 1 << 3;

/// A parsed hotkey: a set of required modifiers plus a single non-modifier trigger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotkey {
    mods: u8,
    key: Key,
}

/// Modifier bit for a modifier word/alias (`None` if it isn't a modifier).
fn mod_alias(token: &str) -> Option<u8> {
    match token {
        "ctrl" | "control" => Some(MOD_CTRL),
        "alt" | "option" => Some(MOD_ALT),
        "shift" => Some(MOD_SHIFT),
        "super" | "win" | "windows" | "meta" | "cmd" | "command" => Some(MOD_SUPER),
        _ => None,
    }
}

impl Hotkey {
    /// Parse a hotkey string: `"ctrl+win+alt+esc"`, `"scroll_lock"`, `"a"`. Returns
    /// `None` if there's no (single) non-modifier trigger key.
    pub fn parse(s: &str) -> Option<Hotkey> {
        let mut mods = 0u8;
        let mut key: Option<Key> = None;
        for raw in s.split('+') {
            let tok = raw.trim().to_ascii_lowercase();
            if tok.is_empty() {
                continue;
            }
            if let Some(b) = mod_alias(&tok) {
                mods |= b;
                continue;
            }
            // The trigger: a named key, else a single character.
            let k = mouse_input::NamedKey::from_name(&tok)
                .map(Key::Named)
                .or_else(|| single_char(&tok).map(Key::Char));
            if let Some(k) = k {
                key = Some(k);
            }
        }
        key.map(|key| Hotkey { mods, key })
    }

    /// True when `pressed` is the trigger and `held_mods` includes every required
    /// modifier. `held_mods` is the engine's running modifier bitset (see
    /// [`modifier_bit`]).
    pub fn matches(&self, pressed: &Key, held_mods: u8) -> bool {
        (held_mods & self.mods) == self.mods && key_eq(pressed, &self.key)
    }
}

fn single_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// Key equality that collapses left/right for a modifier trigger key.
fn key_eq(a: &Key, b: &Key) -> bool {
    if a == b {
        return true;
    }
    if let (Key::Named(x), Key::Named(y)) = (a, b) {
        if let (Some(tx), Some(ty)) = (x.modifier_token(), y.modifier_token()) {
            return tx == ty;
        }
    }
    false
}

/// The modifier bit for a key, if it is a modifier — used to maintain the running
/// `held_mods` bitset from the key stream.
pub fn modifier_bit(key: &Key) -> Option<u8> {
    if let Key::Named(n) = key {
        return match n.modifier_token() {
            Some("ctrl") => Some(MOD_CTRL),
            Some("alt") => Some(MOD_ALT),
            Some("shift") => Some(MOD_SHIFT),
            Some("super") => Some(MOD_SUPER),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use mouse_input::NamedKey;

    #[test]
    fn parses_combo_and_single() {
        let combo = Hotkey::parse("ctrl+win+alt+esc").unwrap();
        assert_eq!(combo.mods, MOD_CTRL | MOD_ALT | MOD_SUPER);
        assert_eq!(combo.key, Key::Named(NamedKey::Escape));

        let single = Hotkey::parse("scroll_lock").unwrap();
        assert_eq!(single.mods, 0);
        assert_eq!(single.key, Key::Named(NamedKey::ScrollLock));

        assert!(Hotkey::parse("ctrl+alt").is_none(), "no trigger key -> None");
    }

    #[test]
    fn matches_only_with_all_modifiers_held() {
        let hk = Hotkey::parse("ctrl+win+alt+esc").unwrap();
        let esc = Key::Named(NamedKey::Escape);
        let all = MOD_CTRL | MOD_ALT | MOD_SUPER;
        assert!(hk.matches(&esc, all), "fires with all mods held");
        assert!(!hk.matches(&esc, MOD_CTRL | MOD_ALT), "missing super -> no fire");
        assert!(hk.matches(&esc, all | MOD_SHIFT), "extra mods are fine");
        assert!(
            !hk.matches(&Key::Named(NamedKey::Enter), all),
            "wrong trigger key -> no fire"
        );
    }

    #[test]
    fn modifier_bit_tracks_left_and_right() {
        // l/r collapse to the same bit, so either side satisfies the combo.
        assert_eq!(
            modifier_bit(&Key::Named(NamedKey::CtrlL)),
            modifier_bit(&Key::Named(NamedKey::CtrlR))
        );
        assert_eq!(modifier_bit(&Key::Named(NamedKey::SuperR)), Some(MOD_SUPER));
        assert_eq!(modifier_bit(&Key::Char('a')), None);
    }

    #[test]
    fn win_alias_maps_to_super() {
        let a = Hotkey::parse("win+esc").unwrap();
        let b = Hotkey::parse("super+esc").unwrap();
        assert_eq!(a, b);
    }
}
