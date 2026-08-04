//! Wire protocol: a tagged message enum carried over length-delimited frames.
//!
//! This replaces the Python app's line-delimited JSON. Length framing removes the
//! "partial line blocks the reader forever" failure mode and lets the read side
//! apply a clean timeout per frame — the foundation for dead-link detection.

use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

/// Protocol version, bumped on incompatible changes. Sent in `Hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Mouse buttons we forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// Which lane a connection is. Declared in `Hello`, so one listener can carry both and
/// the default deployment needs no second port opened.
///
/// Input is the serde default on purpose: a peer built before the bulk lane existed
/// sends no `lane` field at all, and must keep being understood as the input
/// connection rather than rejected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    /// Mouse, keyboard, heartbeat, small clipboard items. Latency-critical.
    #[default]
    Input,
    /// Files and large clipboard items. Throughput-oriented, paced, and never allowed
    /// in front of an input frame.
    Bulk,
}

/// Every message that can cross the link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Message {
    // --- handshake ---
    Hello {
        name: String,
        width: i32,
        height: i32,
        version: u32,
        /// Which lane this connection is opening. Absent from an older peer, which is
        /// exactly `Lane::Input`.
        #[serde(default)]
        lane: Lane,
    },
    AuthChallenge {
        nonce: String, // hex-encoded
    },
    AuthResponse {
        hmac: String, // hex-encoded
    },
    AuthOk {
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    // --- control of who owns input ---
    Enter {
        x: i32,
        y: i32,
    },
    Leave,
    LeaveRequest,

    // --- input events ---
    MouseMove {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: Button,
        pressed: bool,
    },
    MouseScroll {
        dx: i32,
        dy: i32,
    },
    Key {
        key: String,
        pressed: bool,
    },

    // --- clipboard ---
    Clipboard {
        text: String,
    },
    ClipboardImage {
        data: String, // base64 PNG
        w: i32,
        h: i32,
    },

    // --- liveness ---
    Ping,
    Pong,
}

/// Codec turning a byte stream into `Message`s and back, using a 4-byte big-endian
/// length prefix and a JSON payload. JSON keeps the wire debuggable; the payload
/// format is isolated here so it can later be swapped for a compact binary codec
/// without touching the rest of the app.
pub struct MessageCodec {
    inner: LengthDelimitedCodec,
}

impl Default for MessageCodec {
    fn default() -> Self {
        // Cap a single frame so a corrupt/hostile length can't trigger a huge alloc.
        // 8 MiB comfortably covers clipboard images (capped well below this).
        let inner = LengthDelimitedCodec::builder()
            .max_frame_length(8 * 1024 * 1024)
            .length_field_type::<u32>()
            .new_codec();
        MessageCodec { inner }
    }
}

impl MessageCodec {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl Decoder for MessageCodec {
    type Item = Message;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, CodecError> {
        match self.inner.decode(src)? {
            Some(frame) => {
                let msg = serde_json::from_slice(&frame)?;
                Ok(Some(msg))
            }
            None => Ok(None),
        }
    }
}

impl Encoder<Message> for MessageCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), CodecError> {
        let payload = serde_json::to_vec(&item)?;
        // Hand the framed length-prefixing to LengthDelimitedCodec.
        let mut frame = BytesMut::with_capacity(payload.len());
        frame.put_slice(&payload);
        self.inner.encode(frame.freeze(), dst)?;
        Ok(())
    }
}

/// Convenience: encode a single message to a fresh buffer (used in tests).
pub fn encode_one(msg: &Message) -> Result<BytesMut, CodecError> {
    let mut codec = MessageCodec::new();
    let mut buf = BytesMut::new();
    codec.encode(msg.clone(), &mut buf)?;
    let _ = buf.remaining();
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_each_variant() {
        let msgs = vec![
            Message::Hello {
                name: "k".into(),
                width: 2560,
                height: 1440,
                version: PROTOCOL_VERSION,
                lane: Lane::Bulk,
            },
            Message::AuthChallenge { nonce: "ab".into() },
            Message::MouseMove { dx: 3, dy: -7 },
            Message::MouseButton {
                button: Button::Left,
                pressed: true,
            },
            Message::Key {
                key: "a".into(),
                pressed: false,
            },
            Message::Ping,
            Message::Pong,
        ];

        let mut codec = MessageCodec::new();
        let mut buf = BytesMut::new();
        for m in &msgs {
            codec.encode(m.clone(), &mut buf).unwrap();
        }
        let mut out = Vec::new();
        while let Some(m) = codec.decode(&mut buf).unwrap() {
            out.push(m);
        }
        assert_eq!(out, msgs);
    }

    #[test]
    fn partial_frame_decodes_to_none_then_completes() {
        let mut codec = MessageCodec::new();
        let full = encode_one(&Message::Ping).unwrap();
        // Feed all but the last byte: decoder must yield None, not block or error.
        let mut buf = BytesMut::from(&full[..full.len() - 1]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
        // Deliver the final byte; now it completes.
        buf.extend_from_slice(&full[full.len() - 1..]);
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Message::Ping));
    }
}
