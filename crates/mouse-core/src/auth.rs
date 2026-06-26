//! HMAC-SHA256 challenge/response authentication.
//!
//! Same scheme as the Python app: the server sends a random nonce, the client
//! replies with `HMAC-SHA256(shared_secret, nonce)`, the server compares in
//! constant time. The transport (async handshake) lives in `net`; this module is
//! the pure, testable crypto core.

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// 32 random bytes, hex-encoded, suitable for an auth challenge nonce.
pub fn generate_nonce_hex() -> String {
    let mut nonce = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce);
    hex_encode(&nonce)
}

/// Compute `HMAC-SHA256(secret, nonce_bytes)` as a lowercase hex string.
/// `nonce_hex` is the hex challenge as received on the wire.
pub fn compute_hmac_hex(secret: &str, nonce_hex: &str) -> Result<String, AuthError> {
    let nonce = hex_decode(nonce_hex)?;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(&nonce);
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

/// Constant-time check that `candidate_hex` is the expected HMAC for `nonce_hex`.
pub fn verify(secret: &str, nonce_hex: &str, candidate_hex: &str) -> bool {
    match (hex_decode(nonce_hex), hex_decode(candidate_hex)) {
        (Ok(nonce), Ok(candidate)) => {
            let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
                .expect("HMAC accepts any key length");
            mac.update(&nonce);
            // verify_slice is constant-time on the tag comparison.
            mac.verify_slice(&candidate).is_ok()
        }
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

    #[test]
    fn correct_secret_verifies() {
        let secret = "a-very-long-shared-secret";
        let nonce = generate_nonce_hex();
        let resp = compute_hmac_hex(secret, &nonce).unwrap();
        assert!(verify(secret, &nonce, &resp));
    }

    #[test]
    fn wrong_secret_rejected() {
        let nonce = generate_nonce_hex();
        let resp = compute_hmac_hex("right-secret", &nonce).unwrap();
        assert!(!verify("wrong-secret", &nonce, &resp));
    }

    #[test]
    fn tampered_response_rejected() {
        let secret = "s";
        let nonce = generate_nonce_hex();
        let mut resp = compute_hmac_hex(secret, &nonce).unwrap();
        // Flip the last hex digit.
        let last = resp.pop().unwrap();
        resp.push(if last == '0' { '1' } else { '0' });
        assert!(!verify(secret, &nonce, &resp));
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
