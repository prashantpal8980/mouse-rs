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

/// A file copied to the clipboard: what it is, and where it came from.
///
/// Only this description crosses the input link. The contents travel on the bulk lane,
/// and only if the receiving machine decides it wants them — which is what lets a 4 GB
/// copy show up in the other machine's history instantly and cost nothing until asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub name: String,
    pub size: u64,
    /// SHA-256 of the contents: the cache key, the integrity check, and the reason a
    /// file the peer already holds needs no transfer at all.
    pub hash: String,
    /// Where it lived on the machine it was copied from. Used to re-read it on request,
    /// and to say something true once it has moved.
    #[serde(default)]
    pub source_path: Option<String>,
    /// Path relative to what the user actually selected, so a copied folder arrives with
    /// its shape intact: `myproject/src/main.rs`, not a flat pile of `main.rs`.
    ///
    /// Empty for a plainly-copied file. Every component is re-sanitised on the
    /// receiving side — this field is peer-controlled, and taken literally it would be
    /// a write outside the cache.
    #[serde(default)]
    pub rel: String,
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
    /// A clipboard image too big to put in front of a mouse packet: the description
    /// only, with the pixels fetched over the bulk lane like a file.
    ///
    /// Small images stay in `ClipboardImage` — a screenshot that crosses in 5 ms does
    /// not need the machinery, and a round trip would make it slower, not faster.
    ClipboardImageRef {
        hash: String,
        w: i32,
        h: i32,
        size: u64,
    },
    /// "I copied these files." Description only — no contents.
    ClipboardFiles {
        files: Vec<FileRef>,
        /// Name of the machine the copy happened on, for "on laptop · click to fetch".
        origin: String,
        /// Directories in the copied tree, so empty ones survive the trip. Defaulted,
        /// so a peer running an older build still parses the message.
        #[serde(default)]
        dirs: Vec<String>,
    },
    /// "Send me the contents of this hash, starting at `offset`." Tiny, so it rides the
    /// input lane; the answer comes back on the bulk lane.
    BlobRequest {
        hash: String,
        offset: u64,
    },
    /// "I cannot produce that." `reason` is shown to the user verbatim.
    BlobUnavailable {
        hash: String,
        reason: String,
    },
    /// "Stop sending that — I don't want it after all."
    ///
    /// Sent by the *receiver*, because cancelling is the receiver's decision but only
    /// the sender can act on it. Without this, Stop would halt the progress display
    /// while the bytes kept arriving and the lane stayed busy.
    BlobCancel {
        hash: String,
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
