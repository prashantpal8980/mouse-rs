//! The OS-neutral input event, plus conversion to/from the wire [`Message`].
//!
//! Keeping this mapping in one place means the engine never hand-rolls
//! `Message::Key { key: "..." }` strings — it works in typed [`InputEvent`]s and
//! the wire format stays consistent across every OS pairing.

use mouse_core::protocol::{Button, Message};

use crate::keys::Key;

/// A single input action, independent of OS and of whether it was captured
/// locally or received from a peer.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Relative pointer motion (the normal forwarded mouse move).
    MouseMove { dx: i32, dy: i32 },
    /// A mouse button transition.
    MouseButton { button: Button, pressed: bool },
    /// Scroll wheel; positive dy = up, positive dx = right (matches the wire).
    MouseScroll { dx: i32, dy: i32 },
    /// A key transition.
    Key { key: Key, pressed: bool },
}

impl InputEvent {
    /// Encode as a wire [`Message`]. Always succeeds (input events are a subset
    /// of the protocol).
    pub fn to_message(&self) -> Message {
        match self {
            InputEvent::MouseMove { dx, dy } => Message::MouseMove { dx: *dx, dy: *dy },
            InputEvent::MouseButton { button, pressed } => Message::MouseButton {
                button: *button,
                pressed: *pressed,
            },
            InputEvent::MouseScroll { dx, dy } => Message::MouseScroll { dx: *dx, dy: *dy },
            InputEvent::Key { key, pressed } => Message::Key {
                key: key.to_string(),
                pressed: *pressed,
            },
        }
    }

    /// Decode from a wire [`Message`]. Returns `None` for non-input messages
    /// (handshake, heartbeat, clipboard, …) and for an unparseable key string.
    pub fn from_message(msg: &Message) -> Option<InputEvent> {
        match msg {
            Message::MouseMove { dx, dy } => Some(InputEvent::MouseMove { dx: *dx, dy: *dy }),
            Message::MouseButton { button, pressed } => Some(InputEvent::MouseButton {
                button: *button,
                pressed: *pressed,
            }),
            Message::MouseScroll { dx, dy } => Some(InputEvent::MouseScroll { dx: *dx, dy: *dy }),
            Message::Key { key, pressed } => {
                let key = key.parse().ok()?;
                Some(InputEvent::Key {
                    key,
                    pressed: *pressed,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{Key, NamedKey};

    #[test]
    fn input_events_roundtrip_through_wire() {
        let events = vec![
            InputEvent::MouseMove { dx: 4, dy: -9 },
            InputEvent::MouseButton {
                button: Button::Right,
                pressed: true,
            },
            InputEvent::MouseScroll { dx: 0, dy: 3 },
            InputEvent::Key {
                key: Key::Char('q'),
                pressed: true,
            },
            InputEvent::Key {
                key: Key::Named(NamedKey::Enter),
                pressed: false,
            },
            InputEvent::Key {
                key: Key::f(5),
                pressed: true,
            },
        ];
        for ev in events {
            let msg = ev.to_message();
            let back = InputEvent::from_message(&msg).unwrap();
            assert_eq!(ev, back);
        }
    }

    #[test]
    fn non_input_messages_are_ignored() {
        assert!(InputEvent::from_message(&Message::Ping).is_none());
        assert!(InputEvent::from_message(&Message::Leave).is_none());
        assert!(InputEvent::from_message(&Message::Clipboard { text: "x".into() }).is_none());
    }

    #[test]
    fn bad_key_string_is_ignored_not_panicked() {
        let bad = Message::Key {
            key: "x:bogus".into(),
            pressed: true,
        };
        assert!(InputEvent::from_message(&bad).is_none());
    }
}
