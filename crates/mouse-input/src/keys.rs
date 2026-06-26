//! OS-neutral key identity.
//!
//! A KVM forwards *key events*, not composed characters: modifiers (Shift, Ctrl…)
//! cross the wire as their own press/release events and the receiving OS composes
//! the final character. So the wire identity of a key is one of:
//!
//! * [`Key::Named`] — a non-printable / well-known key (Enter, F5, Shift, arrows…),
//!   named in an OS-neutral way. Each backend maps the name to its native code.
//! * [`Key::Char`] — the base (unshifted) character a printable key produces.
//! * [`Key::Raw`] — a fallback carrying a raw keysym we couldn't name, so unusual
//!   keys still round-trip rather than being dropped.
//!
//! Wire format (stable, human-readable, debuggable):
//! `k:enter`, `k:f5`, `k:shift`, `c:a`, `c:1`, `c:€`, `r:1008ff11`.

use std::fmt;
use std::str::FromStr;

/// A well-known, OS-neutral named key. Left/right modifier variants are kept
/// distinct (so remapping tools and games behave) but normalize to a common
/// token for hotkey matching via [`NamedKey::modifier_token`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedKey {
    // editing / whitespace
    Enter,
    Escape,
    Backspace,
    Tab,
    Space,
    Insert,
    Delete,
    // navigation
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    // locks / system
    CapsLock,
    NumLock,
    ScrollLock,
    PrintScreen,
    Pause,
    Menu,
    // modifiers (l/r distinguished)
    ShiftL,
    ShiftR,
    CtrlL,
    CtrlR,
    AltL,
    AltR,
    AltGr,
    SuperL,
    SuperR,
    // function keys
    F(u8),
}

impl NamedKey {
    /// Stable wire token (without the `k:` prefix).
    pub fn name(self) -> String {
        let s = match self {
            NamedKey::Enter => "enter",
            NamedKey::Escape => "esc",
            NamedKey::Backspace => "backspace",
            NamedKey::Tab => "tab",
            NamedKey::Space => "space",
            NamedKey::Insert => "insert",
            NamedKey::Delete => "delete",
            NamedKey::Up => "up",
            NamedKey::Down => "down",
            NamedKey::Left => "left",
            NamedKey::Right => "right",
            NamedKey::Home => "home",
            NamedKey::End => "end",
            NamedKey::PageUp => "page_up",
            NamedKey::PageDown => "page_down",
            NamedKey::CapsLock => "caps_lock",
            NamedKey::NumLock => "num_lock",
            NamedKey::ScrollLock => "scroll_lock",
            NamedKey::PrintScreen => "print_screen",
            NamedKey::Pause => "pause",
            NamedKey::Menu => "menu",
            NamedKey::ShiftL => "shift",
            NamedKey::ShiftR => "shift_r",
            NamedKey::CtrlL => "ctrl",
            NamedKey::CtrlR => "ctrl_r",
            NamedKey::AltL => "alt",
            NamedKey::AltR => "alt_r",
            NamedKey::AltGr => "alt_gr",
            NamedKey::SuperL => "super",
            NamedKey::SuperR => "super_r",
            NamedKey::F(n) => return format!("f{n}"),
        };
        s.to_string()
    }

    /// Parse a wire token (without the `k:` prefix).
    pub fn from_name(s: &str) -> Option<NamedKey> {
        if let Some(num) = s.strip_prefix('f') {
            if let Ok(n) = num.parse::<u8>() {
                if (1..=24).contains(&n) {
                    return Some(NamedKey::F(n));
                }
            }
        }
        Some(match s {
            "enter" => NamedKey::Enter,
            "esc" => NamedKey::Escape,
            "backspace" => NamedKey::Backspace,
            "tab" => NamedKey::Tab,
            "space" => NamedKey::Space,
            "insert" => NamedKey::Insert,
            "delete" => NamedKey::Delete,
            "up" => NamedKey::Up,
            "down" => NamedKey::Down,
            "left" => NamedKey::Left,
            "right" => NamedKey::Right,
            "home" => NamedKey::Home,
            "end" => NamedKey::End,
            "page_up" => NamedKey::PageUp,
            "page_down" => NamedKey::PageDown,
            "caps_lock" => NamedKey::CapsLock,
            "num_lock" => NamedKey::NumLock,
            "scroll_lock" => NamedKey::ScrollLock,
            "print_screen" => NamedKey::PrintScreen,
            "pause" => NamedKey::Pause,
            "menu" => NamedKey::Menu,
            "shift" => NamedKey::ShiftL,
            "shift_r" => NamedKey::ShiftR,
            "ctrl" => NamedKey::CtrlL,
            "ctrl_r" => NamedKey::CtrlR,
            "alt" => NamedKey::AltL,
            "alt_r" => NamedKey::AltR,
            "alt_gr" => NamedKey::AltGr,
            "super" => NamedKey::SuperL,
            "super_r" => NamedKey::SuperR,
            _ => return None,
        })
    }

    /// Canonical modifier token for hotkey matching (collapses l/r), or None if
    /// this key isn't a modifier.
    pub fn modifier_token(self) -> Option<&'static str> {
        match self {
            NamedKey::ShiftL | NamedKey::ShiftR => Some("shift"),
            NamedKey::CtrlL | NamedKey::CtrlR => Some("ctrl"),
            NamedKey::AltL | NamedKey::AltR | NamedKey::AltGr => Some("alt"),
            NamedKey::SuperL | NamedKey::SuperR => Some("super"),
            _ => None,
        }
    }
}

/// A key as it crosses the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// A well-known named key.
    Named(NamedKey),
    /// The base (unshifted) character a printable key produces.
    Char(char),
    /// Fallback: a raw keysym we couldn't otherwise represent.
    Raw(u32),
}

impl Key {
    /// Convenience constructor for a function key.
    pub fn f(n: u8) -> Key {
        Key::Named(NamedKey::F(n))
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Named(n) => write!(f, "k:{}", n.name()),
            Key::Char(c) => write!(f, "c:{c}"),
            Key::Raw(u) => write!(f, "r:{u:x}"),
        }
    }
}

/// Error parsing a wire key string.
#[derive(Debug, thiserror::Error)]
#[error("invalid key string: {0:?}")]
pub struct ParseKeyError(String);

impl FromStr for Key {
    type Err = ParseKeyError;

    fn from_str(s: &str) -> Result<Key, ParseKeyError> {
        if let Some(rest) = s.strip_prefix("k:") {
            return NamedKey::from_name(rest)
                .map(Key::Named)
                .ok_or_else(|| ParseKeyError(s.to_string()));
        }
        if let Some(rest) = s.strip_prefix("c:") {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => return Ok(Key::Char(c)),
                _ => return Err(ParseKeyError(s.to_string())),
            }
        }
        if let Some(rest) = s.strip_prefix("r:") {
            return u32::from_str_radix(rest, 16)
                .map(Key::Raw)
                .map_err(|_| ParseKeyError(s.to_string()));
        }
        Err(ParseKeyError(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(k: Key) {
        let s = k.to_string();
        let back: Key = s.parse().unwrap();
        assert_eq!(k, back, "roundtrip via {s:?}");
    }

    #[test]
    fn named_keys_roundtrip() {
        for k in [
            Key::Named(NamedKey::Enter),
            Key::Named(NamedKey::Escape),
            Key::Named(NamedKey::Up),
            Key::Named(NamedKey::PageDown),
            Key::Named(NamedKey::ShiftL),
            Key::Named(NamedKey::CtrlR),
            Key::Named(NamedKey::AltGr),
            Key::Named(NamedKey::SuperL),
            Key::f(1),
            Key::f(12),
            Key::f(24),
        ] {
            roundtrip(k);
        }
    }

    #[test]
    fn char_keys_roundtrip_including_unicode() {
        for c in ['a', 'Z', '1', '/', ' ', '€', 'ß', '名'] {
            roundtrip(Key::Char(c));
        }
    }

    #[test]
    fn raw_keys_roundtrip() {
        roundtrip(Key::Raw(0x1008ff11));
        roundtrip(Key::Raw(0));
    }

    #[test]
    fn function_key_bounds() {
        assert_eq!(NamedKey::from_name("f0"), None);
        assert_eq!(NamedKey::from_name("f25"), None);
        assert_eq!(NamedKey::from_name("f5"), Some(NamedKey::F(5)));
    }

    #[test]
    fn modifier_tokens_collapse_left_right() {
        assert_eq!(NamedKey::ShiftR.modifier_token(), Some("shift"));
        assert_eq!(NamedKey::CtrlL.modifier_token(), Some("ctrl"));
        assert_eq!(NamedKey::AltGr.modifier_token(), Some("alt"));
        assert_eq!(NamedKey::Enter.modifier_token(), None);
    }

    #[test]
    fn rejects_garbage() {
        assert!("".parse::<Key>().is_err());
        assert!("k:nope".parse::<Key>().is_err());
        assert!("c:ab".parse::<Key>().is_err()); // more than one char
        assert!("x:1".parse::<Key>().is_err());
    }
}
