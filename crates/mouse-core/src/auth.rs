//! HMAC-SHA256 mutual authentication (B34).
//!
//! Both sides prove they know the shared secret. The client sends a nonce in its
//! Hello; the server answers with its own nonce and a server proof; the client
//! checks that proof before sending its own. Each proof is
//! `HMAC-SHA256(secret, label || binding || client_nonce || server_nonce)`, where
//! the label differs per side (a proof can't be echoed back) and `binding` is
//! keying material exported from this TLS session. A machine in the middle runs two
//! TLS sessions with two different bindings, so proofs relayed through it fail.
//! The transport (async handshake) lives in `net`; this module is the pure,
//! testable crypto core.

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// 32 random bytes, hex-encoded, suitable for an auth challenge nonce.
/// The token a machine shows the development log collector when uploading over Wi-Fi:
/// HMAC of a fixed label under the shared secret, so only a paired machine can upload,
/// and the secret itself never goes over the network.
pub fn log_upload_token(secret: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"mouse-rs log upload v1");
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn generate_nonce_hex() -> String {
    let mut nonce = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce);
    hex_encode(&nonce)
}

/// Which side a proof comes from. Part of the MAC input, so a server proof is never
/// accepted as a client proof or the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Server,
    Client,
}

impl Side {
    fn label(self) -> &'static [u8] {
        match self {
            Side::Server => b"mouse-rs v2 server proof",
            Side::Client => b"mouse-rs v2 client proof",
        }
    }
}

/// Label for the TLS keying-material exporter that yields the session binding.
pub const TLS_EXPORTER_LABEL: &[u8] = b"EXPORTER-mouse-rs-auth";

fn proof_mac(
    secret: &str,
    side: Side,
    binding: &[u8],
    client_nonce_hex: &str,
    server_nonce_hex: &str,
) -> Result<HmacSha256, AuthError> {
    let client_nonce = hex_decode(client_nonce_hex)?;
    let server_nonce = hex_decode(server_nonce_hex)?;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    // Every field is length-prefixed, so no two inputs can run together into the same bytes.
    for part in [side.label(), binding, &client_nonce, &server_nonce] {
        mac.update(&(part.len() as u32).to_be_bytes());
        mac.update(part);
    }
    Ok(mac)
}

/// The proof `side` sends for this session, as lowercase hex.
pub fn proof_hex(
    secret: &str,
    side: Side,
    binding: &[u8],
    client_nonce_hex: &str,
    server_nonce_hex: &str,
) -> Result<String, AuthError> {
    let mac = proof_mac(secret, side, binding, client_nonce_hex, server_nonce_hex)?;
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

/// Constant-time check that `candidate_hex` is `side`'s proof for this session.
pub fn verify_proof(
    secret: &str,
    side: Side,
    binding: &[u8],
    client_nonce_hex: &str,
    server_nonce_hex: &str,
    candidate_hex: &str,
) -> bool {
    match (
        proof_mac(secret, side, binding, client_nonce_hex, server_nonce_hex),
        hex_decode(candidate_hex),
    ) {
        // verify_slice is constant-time on the tag comparison.
        (Ok(mac), Ok(candidate)) => mac.verify_slice(&candidate).is_ok(),
        _ => false,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid hex in nonce/response")]
    BadHex,
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

fn hex_decode(s: &str) -> Result<Vec<u8>, AuthError> {
    if s.len() % 2 != 0 {
        return Err(AuthError::BadHex);
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).ok_or(AuthError::BadHex)?;
        let lo = (bytes[i + 1] as char).to_digit(16).ok_or(AuthError::BadHex)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof(secret: &str, side: Side, binding: &[u8], c: &str, s: &str) -> String {
        proof_hex(secret, side, binding, c, s).unwrap()
    }

    #[test]
    fn correct_secret_verifies() {
        let (c, s) = (generate_nonce_hex(), generate_nonce_hex());
        for side in [Side::Server, Side::Client] {
            let p = proof("a-very-long-shared-secret", side, b"tls", &c, &s);
            assert!(verify_proof("a-very-long-shared-secret", side, b"tls", &c, &s, &p));
        }
    }

    #[test]
    fn wrong_secret_rejected() {
        let (c, s) = (generate_nonce_hex(), generate_nonce_hex());
        let p = proof("right-secret", Side::Client, b"tls", &c, &s);
        assert!(!verify_proof("wrong-secret", Side::Client, b"tls", &c, &s, &p));
    }

    #[test]
    fn a_server_proof_is_not_a_client_proof() {
        let (c, s) = (generate_nonce_hex(), generate_nonce_hex());
        let p = proof("k", Side::Server, b"tls", &c, &s);
        assert!(!verify_proof("k", Side::Client, b"tls", &c, &s, &p));
    }

    #[test]
    fn a_proof_from_another_tls_session_is_rejected() {
        // A machine in the middle holds two sessions with two different bindings.
        let (c, s) = (generate_nonce_hex(), generate_nonce_hex());
        let p = proof("k", Side::Client, b"session-a", &c, &s);
        assert!(!verify_proof("k", Side::Client, b"session-b", &c, &s, &p));
    }

    #[test]
    fn a_proof_for_other_nonces_is_rejected() {
        let (c, s) = (generate_nonce_hex(), generate_nonce_hex());
        let p = proof("k", Side::Client, b"tls", &c, &s);
        assert!(!verify_proof("k", Side::Client, b"tls", &generate_nonce_hex(), &s, &p));
        assert!(!verify_proof("k", Side::Client, b"tls", &c, &generate_nonce_hex(), &p));
    }

    #[test]
    fn tampered_proof_rejected() {
        let (c, s) = (generate_nonce_hex(), generate_nonce_hex());
        let mut p = proof("k", Side::Server, b"", &c, &s);
        // Flip the last hex digit.
        let last = p.pop().unwrap();
        p.push(if last == '0' { '1' } else { '0' });
        assert!(!verify_proof("k", Side::Server, b"", &c, &s, &p));
    }

    #[test]
    fn nonces_are_unique() {
        let a = generate_nonce_hex();
        let b = generate_nonce_hex();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64); // 32 bytes hex
    }

    #[test]
    fn hex_roundtrip() {
        let data = [0x00u8, 0x0f, 0xa5, 0xff];
        let enc = hex_encode(&data);
        assert_eq!(enc, "000fa5ff");
        assert_eq!(hex_decode(&enc).unwrap(), data);
    }
}
