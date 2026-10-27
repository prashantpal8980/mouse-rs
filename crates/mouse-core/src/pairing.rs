//! Pairing with a short code instead of copying the shared secret by hand.
//!
//! The server shows a 6-digit code; the user types it on the client. The two run SPAKE2
//! with the code as the password. Someone listening learns nothing they could test a
//! guess against offline, and someone in the middle gets one guess per attempt, which the
//! server limits to [`MAX_FAILS`] before the code is thrown away. Each side then proves it
//! derived the same key, bound to this TLS session, and the server hands over its shared
//! secret sealed under that key. From then on the two connect with the secret as before.
//!
//! The transport lives in `net`; this module is the code itself and the crypto.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

/// How long a code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// Failed attempts before the code is thrown away and a new one is needed.
pub const MAX_FAILS: u32 = 3;
/// Code length.
pub const CODE_DIGITS: usize = 6;

const ID_CLIENT: &[u8] = b"mouse-rs pairing client";
const ID_SERVER: &[u8] = b"mouse-rs pairing server";

// ---------------------------------------------------------------------------
// The code the server is showing
// ---------------------------------------------------------------------------

struct Active {
    code: String,
    expires: Instant,
    fails: u32,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);
/// The name of the last client that paired, for the panel to announce.
static LAST_PAIRED: Mutex<Option<String>> = Mutex::new(None);

fn active() -> std::sync::MutexGuard<'static, Option<Active>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Show a new code, replacing any earlier one.
pub fn start_code() -> String {
    let code = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32));
    *active() = Some(Active {
        code: code.clone(),
        expires: Instant::now() + CODE_TTL,
        fails: 0,
    });
    code
}

/// The code being shown and how long it has left, if one is live.
pub fn current_code() -> Option<(String, Duration)> {
    let mut g = active();
    let now = Instant::now();
    match g.as_ref() {
        Some(a) if a.expires > now => Some((a.code.clone(), a.expires - now)),
        Some(_) => {
            *g = None;
            None
        }
        None => None,
    }
}

/// Stop showing the code.
pub fn cancel_code() {
    *active() = None;
}

/// The code to pair against right now, if any.
pub(crate) fn code_for_attempt() -> Option<String> {
    current_code().map(|(c, _)| c)
}

/// An attempt failed. After [`MAX_FAILS`] the code is gone.
pub(crate) fn note_failure() {
    let mut g = active();
    if let Some(a) = g.as_mut() {
        a.fails += 1;
        if a.fails >= MAX_FAILS {
            *g = None;
        }
    }
}

/// An attempt succeeded: the code is used up.
pub(crate) fn note_success(client: &str) {
    *active() = None;
    *LAST_PAIRED.lock().unwrap_or_else(|e| e.into_inner()) = Some(client.to_owned());
}

/// The client that paired since this was last asked, if any.
pub fn take_last_paired() -> Option<String> {
    LAST_PAIRED.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// True if `code` is exactly [`CODE_DIGITS`] ASCII digits.
pub fn is_valid_code(code: &str) -> bool {
    code.len() == CODE_DIGITS && code.bytes().all(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Crypto
// ---------------------------------------------------------------------------

/// One side's SPAKE2 state, before it has the other side's message.
pub struct Start {
    state: Spake2<Ed25519Group>,
    /// Our outgoing message, hex.
    pub msg: String,
}

pub fn start_client(code: &str) -> Start {
    let (state, msg) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &Identity::new(ID_CLIENT),
        &Identity::new(ID_SERVER),
    );
    Start {
        state,
        msg: hex(&msg),
    }
}

pub fn start_server(code: &str) -> Start {
    let (state, msg) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.as_bytes()),
        &Identity::new(ID_CLIENT),
        &Identity::new(ID_SERVER),
    );
    Start {
        state,
        msg: hex(&msg),
    }
}

/// The key both sides share once each has the other's message, tied to this TLS session
/// and both messages. A wrong code gives a different key on each side, which the
/// confirmations then catch.
pub struct Session {
    key: Vec<u8>,
    transcript: Vec<u8>,
}

impl Start {
    /// Finish with the other side's message. `client_msg` and `server_msg` are the two
    /// messages in that order, whichever side we are.
    pub fn finish(
        self,
        theirs: &str,
        binding: &[u8],
        client_msg: &str,
        server_msg: &str,
    ) -> Result<Session, String> {
        let theirs = unhex(theirs).ok_or("bad pairing message")?;
        let key = self
            .state
            .finish(&theirs)
            .map_err(|e| format!("pairing: {e:?}"))?;
        let mut transcript = Vec::new();
        for part in [binding, client_msg.as_bytes(), server_msg.as_bytes()] {
            transcript.extend_from_slice(&(part.len() as u32).to_be_bytes());
            transcript.extend_from_slice(part);
        }
        Ok(Session { key, transcript })
    }
}

impl Session {
    fn mac(&self, label: &[u8], extra: &[u8]) -> Hmac<Sha256> {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts any key length");
        for part in [label, &self.transcript, extra] {
            m.update(&(part.len() as u32).to_be_bytes());
            m.update(part);
        }
        m
    }

    /// The server's confirmation, hex.
    pub fn server_confirm(&self) -> String {
        hex(&self.mac(b"server confirm", b"").finalize().into_bytes())
    }

    /// The client's confirmation, hex.
    pub fn client_confirm(&self) -> String {
        hex(&self.mac(b"client confirm", b"").finalize().into_bytes())
    }

    /// Constant-time comparison of a confirmation.
    pub fn check(&self, want: &str, got: &str) -> bool {
        want.len() == got.len()
            && want
                .bytes()
                .zip(got.bytes())
                .fold(0u8, |a, (x, y)| a | (x ^ y))
                == 0
    }

    /// Seal `secret` for the client: (ciphertext hex, tag hex).
    pub fn seal(&self, secret: &str) -> (String, String) {
        let ct: Vec<u8> = secret
            .bytes()
            .zip(self.pad(secret.len()))
            .map(|(a, b)| a ^ b)
            .collect();
        let tag = self.mac(b"seal", &ct).finalize().into_bytes();
        (hex(&ct), hex(&tag))
    }

    /// Open a sealed secret, checking its tag first.
    pub fn open(&self, ct_hex: &str, tag_hex: &str) -> Result<String, String> {
        let ct = unhex(ct_hex).ok_or("bad sealed secret")?;
        let tag = unhex(tag_hex).ok_or("bad sealed secret")?;
        self.mac(b"seal", &ct)
            .verify_slice(&tag)
            .map_err(|_| "the sealed secret was altered".to_string())?;
        let plain: Vec<u8> = ct
            .iter()
            .zip(self.pad(ct.len()))
            .map(|(a, b)| a ^ b)
            .collect();
        String::from_utf8(plain).map_err(|_| "the sealed secret is not text".to_string())
    }

    /// A keystream of `len` bytes: HMAC blocks over a counter.
    fn pad(&self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 32);
        let mut i = 0u32;
        while out.len() < len {
            out.extend_from_slice(&self.mac(b"pad", &i.to_be_bytes()).finalize().into_bytes());
            i += 1;
        }
        out.truncate(len);
        out
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        client_code: &str,
        server_code: &str,
        binding_c: &[u8],
        binding_s: &[u8],
    ) -> (Session, Session) {
        let c = start_client(client_code);
        let s = start_server(server_code);
        let (cm, sm) = (c.msg.clone(), s.msg.clone());
        let cs = c.finish(&sm, binding_c, &cm, &sm).unwrap();
        let ss = s.finish(&cm, binding_s, &cm, &sm).unwrap();
        (cs, ss)
    }

    #[test]
    fn the_same_code_pairs_and_the_secret_arrives() {
        let (c, s) = run("123456", "123456", b"tls", b"tls");
        assert!(c.check(&c.server_confirm(), &s.server_confirm()));
        assert!(s.check(&s.client_confirm(), &c.client_confirm()));
        let (ct, tag) = s.seal("a-long-shared-secret");
        assert_ne!(ct, hex(b"a-long-shared-secret"), "never sent in the clear");
        assert_eq!(c.open(&ct, &tag).unwrap(), "a-long-shared-secret");
    }

    #[test]
    fn a_wrong_code_fails_the_confirmation() {
        let (c, s) = run("123456", "654321", b"tls", b"tls");
        assert!(!c.check(&c.server_confirm(), &s.server_confirm()));
        let (ct, tag) = s.seal("secret");
        assert!(c.open(&ct, &tag).is_err());
    }

    #[test]
    fn a_machine_in_the_middle_fails_the_confirmation() {
        // Two TLS sessions, two bindings, even with the right code.
        let (c, s) = run("123456", "123456", b"session-a", b"session-b");
        assert!(!c.check(&c.server_confirm(), &s.server_confirm()));
    }

    #[test]
    fn a_code_dies_after_three_failures_or_one_success() {
        let _g = crate::pin::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let code = start_code();
        assert!(is_valid_code(&code));
        assert_eq!(code_for_attempt().as_deref(), Some(code.as_str()));
        for _ in 0..MAX_FAILS {
            note_failure();
        }
        assert_eq!(code_for_attempt(), None);
        start_code();
        note_success("kali");
        assert_eq!(code_for_attempt(), None);
        assert_eq!(take_last_paired().as_deref(), Some("kali"));
        assert_eq!(take_last_paired(), None);
    }
}
