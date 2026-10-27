//! The connection PIN (R35): a short number set on the server and typed on the client
//! before a client may connect.
//!
//! * **Never sent as itself.** The client sends an HMAC of the PIN over the same TLS
//!   binding and nonces as the secret proofs (see `auth`), and only after the server has
//!   proved it holds the shared secret. A listener learns nothing, and a fake server
//!   never gets that far.
//! * **Guessing is limited on the server.** [`MAX_TRIES`] wrong PINs, then a wait of
//!   [`FIRST_LOCK`] that doubles with each further lock. Only a machine that already holds
//!   the shared secret can try at all.
//! * **Asked once per run on the client.** The PIN is kept in memory after it is typed,
//!   so an automatic reconnect after a Wi-Fi drop or sleep does not ask again. It is
//!   asked again after mouse-rs restarts (decision 1 of 26 Oct, suggested answer).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::Notify;

/// Wrong PINs allowed before the server makes the client wait.
pub const MAX_TRIES: u32 = 5;
/// The first wait after [`MAX_TRIES`] wrong PINs; each further lock doubles it.
pub const FIRST_LOCK: Duration = Duration::from_secs(5 * 60);
/// PIN length.
pub const PIN_DIGITS: usize = 4;

/// Refusal reasons on the wire. `wrong_pin:<tries left>` and `pin_locked:<seconds>` carry
/// a number after the colon.
pub const REASON_REQUIRED: &str = "pin_required";
pub const REASON_WRONG: &str = "wrong_pin";
pub const REASON_LOCKED: &str = "pin_locked";

/// True if `pin` is exactly [`PIN_DIGITS`] ASCII digits.
pub fn is_valid(pin: &str) -> bool {
    pin.len() == PIN_DIGITS && pin.bytes().all(|b| b.is_ascii_digit())
}

/// A fresh random PIN, for a server that has none yet (the PIN is on by default).
pub fn random_pin() -> String {
    use rand::Rng;
    format!("{:04}", rand::thread_rng().gen_range(0..10_000u32))
}

/// The client's proof that it knows `pin`, bound to this TLS session and these nonces.
pub fn proof_hex(pin: &str, binding: &[u8], client_nonce: &str, server_nonce: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(pin.as_bytes()).expect("HMAC accepts any key length");
    for part in [
        b"mouse-rs pin v1".as_slice(),
        binding,
        client_nonce.as_bytes(),
        server_nonce.as_bytes(),
    ] {
        mac.update(&(part.len() as u32).to_be_bytes());
        mac.update(part);
    }
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Constant-time check of a client's PIN proof.
pub fn verify(
    pin: &str,
    binding: &[u8],
    client_nonce: &str,
    server_nonce: &str,
    candidate: &str,
) -> bool {
    let want = proof_hex(pin, binding, client_nonce, server_nonce);
    want.len() == candidate.len()
        && want
            .bytes()
            .zip(candidate.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

// ---------------------------------------------------------------------------
// Server: try limits
// ---------------------------------------------------------------------------

/// Wrong-PIN accounting on the server. One for the whole process, not per address: a
/// guesser can change address, it cannot change which server it is guessing against.
#[derive(Debug, Default)]
pub struct Limiter {
    wrong: u32,
    locks: u32,
    locked_until: Option<Instant>,
}

/// What the server should do with a PIN attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    /// Wrong; this many tries left before a lock.
    Wrong(u32),
    /// Locked; try again after this long.
    Locked(Duration),
}

impl Limiter {
    /// Still locked at `now`? Then how long is left.
    pub fn locked_for(&self, now: Instant) -> Option<Duration> {
        self.locked_until
            .and_then(|t| t.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    /// Record an attempt whose correctness is `correct`.
    pub fn attempt(&mut self, correct: bool, now: Instant) -> Verdict {
        if let Some(left) = self.locked_for(now) {
            return Verdict::Locked(left);
        }
        if correct {
            self.wrong = 0;
            self.locks = 0;
            self.locked_until = None;
            return Verdict::Accept;
        }
        self.wrong += 1;
        if self.wrong >= MAX_TRIES {
            let wait = FIRST_LOCK * 2u32.saturating_pow(self.locks.min(10));
            self.locks += 1;
            self.wrong = 0;
            self.locked_until = Some(now + wait);
            return Verdict::Locked(wait);
        }
        Verdict::Wrong(MAX_TRIES - self.wrong)
    }
}

static LIMITER: Mutex<Option<Limiter>> = Mutex::new(None);

/// [`Limiter::attempt`] on the process-wide limiter.
pub fn server_attempt(correct: bool) -> Verdict {
    let mut g = LIMITER.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(Limiter::default)
        .attempt(correct, Instant::now())
}

/// The refusal reason for a verdict other than Accept.
pub fn refusal_reason(v: &Verdict) -> String {
    match v {
        Verdict::Accept => String::new(),
        Verdict::Wrong(left) => format!("{REASON_WRONG}:{left}"),
        Verdict::Locked(d) => format!("{REASON_LOCKED}:{}", d.as_secs().max(1)),
    }
}

// ---------------------------------------------------------------------------
// Client: the PIN typed this run, and what the panel should ask
// ---------------------------------------------------------------------------

/// What the client panel should show about the PIN.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Prompt {
    /// Nothing to ask.
    #[default]
    None,
    /// The server asks for a PIN and none has been typed.
    Needed,
    /// The PIN typed was wrong; this many tries left.
    Wrong(u32),
    /// Too many wrong PINs; the server accepts none until this moment.
    Locked(Instant),
}

struct ClientState {
    pin: Option<String>,
    prompt: Prompt,
}

static CLIENT: Mutex<ClientState> = Mutex::new(ClientState {
    pin: None,
    prompt: Prompt::None,
});
static ENTERED: Notify = Notify::const_new();

fn client() -> std::sync::MutexGuard<'static, ClientState> {
    CLIENT.lock().unwrap_or_else(|e| e.into_inner())
}

/// The PIN typed this run, if any.
pub fn cached() -> Option<String> {
    client().pin.clone()
}

/// The user typed a PIN: keep it for this run and retry the connection at once.
pub fn enter(pin: String) {
    {
        let mut c = client();
        c.pin = Some(pin);
        c.prompt = Prompt::None;
    }
    ENTERED.notify_waiters();
}

/// Resolves when the user types a PIN. The reconnect loop waits on this alongside its
/// backoff, so a typed PIN connects straight away instead of after the next retry.
pub async fn entered() {
    ENTERED.notified().await
}

/// What the panel should ask right now.
pub fn prompt() -> Prompt {
    client().prompt.clone()
}

/// The handshake succeeded: nothing to ask.
pub fn accepted() {
    client().prompt = Prompt::None;
}

/// The server refused with `reason`. Returns true if it was a PIN refusal, which then
/// sets what the panel asks and forgets a PIN that did not work.
pub fn note_refusal(reason: &str) -> bool {
    let (kind, num) = match reason.split_once(':') {
        Some((k, n)) => (k, n.parse::<u64>().ok()),
        None => (reason, None),
    };
    let prompt = match kind {
        REASON_REQUIRED => Prompt::Needed,
        REASON_WRONG => Prompt::Wrong(num.unwrap_or(0) as u32),
        REASON_LOCKED => Prompt::Locked(Instant::now() + Duration::from_secs(num.unwrap_or(60))),
        _ => return false,
    };
    let mut c = client();
    c.pin = None;
    c.prompt = prompt;
    true
}

/// Tests that touch the process-wide PIN or pairing state take this first.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_four_digits_are_a_pin() {
        assert!(is_valid("0420"));
        assert!(!is_valid("042"));
        assert!(!is_valid("04201"));
        assert!(!is_valid("04a0"));
        assert!(is_valid(&random_pin()));
    }

    #[test]
    fn the_proof_checks_the_pin_and_the_session() {
        let p = proof_hex("1234", b"tls", "c", "s");
        assert!(verify("1234", b"tls", "c", "s", &p));
        assert!(!verify("1235", b"tls", "c", "s", &p), "wrong PIN");
        assert!(
            !verify("1234", b"other", "c", "s", &p),
            "another TLS session"
        );
        assert!(!verify("1234", b"tls", "c", "s2", &p), "other nonces");
    }

    #[test]
    fn five_wrong_tries_lock_and_each_lock_doubles() {
        let mut l = Limiter::default();
        let t0 = Instant::now();
        for left in (1..MAX_TRIES).rev() {
            assert_eq!(l.attempt(false, t0), Verdict::Wrong(left));
        }
        assert_eq!(l.attempt(false, t0), Verdict::Locked(FIRST_LOCK));
        // Even the right PIN is refused while locked.
        assert!(matches!(
            l.attempt(true, t0 + Duration::from_secs(1)),
            Verdict::Locked(_)
        ));
        let t1 = t0 + FIRST_LOCK + Duration::from_secs(1);
        for _ in 1..MAX_TRIES {
            l.attempt(false, t1);
        }
        assert_eq!(l.attempt(false, t1), Verdict::Locked(FIRST_LOCK * 2));
        let t2 = t1 + FIRST_LOCK * 2 + Duration::from_secs(1);
        assert_eq!(l.attempt(true, t2), Verdict::Accept);
        assert_eq!(
            l.attempt(false, t2),
            Verdict::Wrong(MAX_TRIES - 1),
            "a success resets the count"
        );
    }

    #[test]
    fn refusals_set_the_prompt_and_drop_the_pin() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        enter("1111".into());
        assert_eq!(cached().as_deref(), Some("1111"));
        assert!(note_refusal("wrong_pin:3"));
        assert_eq!(prompt(), Prompt::Wrong(3));
        assert_eq!(cached(), None);
        assert!(note_refusal("pin_required"));
        assert_eq!(prompt(), Prompt::Needed);
        assert!(matches!(
            {
                note_refusal("pin_locked:300");
                prompt()
            },
            Prompt::Locked(_)
        ));
        assert!(!note_refusal("bad_secret"));
        accepted();
        assert_eq!(prompt(), Prompt::None);
    }
}
