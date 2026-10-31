//! The connection PIN (R35): set on the **client**, the machine being driven, and typed on
//! the **server**, the machine with the keyboard and mouse.
//!
//! The danger is on the client side. The person at the server already owns that machine;
//! what needs protecting is the client, from any other server on the same network taking
//! it over (owner, 27 Oct 2026: "out of these 10 people, anyone can be server and able to
//! connect with the client and able to drive the machine"). So the client decides who may
//! drive it: it refuses a server that cannot show its PIN.
//!
//! * **Never sent as itself.** The server sends an HMAC of the PIN over the same TLS
//!   binding and nonces as the secret proofs (see `auth`). A listener learns nothing.
//! * **Guessing is limited on the client.** [`MAX_TRIES`] wrong PINs, then a wait of
//!   [`FIRST_LOCK`] that doubles with each further lock.
//! * **Typed once per run on the server.** The server keeps what was typed for each client
//!   in memory, so an automatic reconnect after a Wi-Fi drop or sleep does not ask again.
//!   It is asked again after mouse-rs restarts on the server (decision 1 of 26 Oct).
//! * **Letters and symbols too.** At least [`MIN_LEN`] characters of anything.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Wrong passwords allowed before the client stops accepting any for a while (owner, 27 Oct).
pub const MAX_TRIES: u32 = 3;
/// The first wait after [`MAX_TRIES`] wrong passwords: 1 hour, then 2, 4, 8… (owner, 27 Oct).
pub const FIRST_LOCK: Duration = Duration::from_secs(60 * 60);
/// Shortest PIN.
pub const MIN_LEN: usize = 4;
/// Longest PIN.
pub const MAX_LEN: usize = 64;

/// Refusal reasons on the wire. `wrong_pin:<tries left>` and `pin_locked:<seconds>` carry
/// a number after the colon.
pub const REASON_REQUIRED: &str = "pin_required";
pub const REASON_WRONG: &str = "wrong_pin";
pub const REASON_LOCKED: &str = "pin_locked";

/// True if `pin` is [`MIN_LEN`]..=[`MAX_LEN`] characters with no control characters.
pub fn is_valid(pin: &str) -> bool {
    let n = pin.chars().count();
    (MIN_LEN..=MAX_LEN).contains(&n) && !pin.chars().any(char::is_control)
}

/// The server's proof that it knows `pin`, bound to this TLS session, these nonces and
/// the pairing key (R44).
///
/// The key matters because a PIN is short. A proof made from the PIN alone could be
/// collected by anyone who opened a connection claiming to be the client, then guessed at
/// offline, one PIN after another, with no try limit. Keyed with the pairing as well, it
/// is worthless to anyone who is not one of the two paired computers.
pub fn proof_hex(pair_key: &str, pin: &str, binding: &[u8], client_nonce: &str, server_nonce: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(pair_key.as_bytes()).expect("HMAC accepts any key length");
    for part in [b"mouse-rs pin v3".as_slice(), pin.as_bytes(), binding, client_nonce.as_bytes(), server_nonce.as_bytes()] {
        mac.update(&(part.len() as u32).to_be_bytes());
        mac.update(part);
    }
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time check of a PIN proof.
pub fn verify(pair_key: &str, pin: &str, binding: &[u8], client_nonce: &str, server_nonce: &str, candidate: &str) -> bool {
    let want = proof_hex(pair_key, pin, binding, client_nonce, server_nonce);
    want.len() == candidate.len() && want.bytes().zip(candidate.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

// ---------------------------------------------------------------------------
// Client: checks the PIN, with try limits
// ---------------------------------------------------------------------------

/// Wrong-PIN accounting on the client. One for the whole process, not per server: a
/// guesser can change address, it cannot change which client it is guessing against.
#[derive(Debug, Default)]
pub struct Limiter {
    wrong: u32,
    locks: u32,
    locked_until: Option<Instant>,
}

/// What the client should do with a PIN attempt.
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
        self.locked_until.and_then(|t| t.checked_duration_since(now)).filter(|d| !d.is_zero())
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
/// The client refused its last server over the PIN, so a person is about to type it on
/// the server: retry often rather than backing off.
static AWAITING: AtomicBool = AtomicBool::new(false);

/// [`Limiter::attempt`] on the process-wide limiter.
pub fn check_attempt(correct: bool) -> Verdict {
    let mut g = LIMITER.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(Limiter::default).attempt(correct, Instant::now())
}

/// Still locked? Then how long is left. A server's attempt is not counted while locked.
pub fn locked_for() -> Option<Duration> {
    let g = LIMITER.lock().unwrap_or_else(|e| e.into_inner());
    g.as_ref().and_then(|l| l.locked_for(Instant::now()))
}

/// The refusal reason for a verdict other than Accept.
pub fn refusal_reason(v: &Verdict) -> String {
    match v {
        Verdict::Accept => String::new(),
        Verdict::Wrong(left) => format!("{REASON_WRONG}:{left}"),
        Verdict::Locked(d) => format!("{REASON_LOCKED}:{}", d.as_secs().max(1)),
    }
}

/// Client: whether it is waiting for someone to type the PIN on the server.
pub fn set_awaiting(on: bool) {
    AWAITING.store(on, Ordering::Relaxed);
}

pub fn awaiting() -> bool {
    AWAITING.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Server: the PIN typed for each client this run, and what the panel should ask
// ---------------------------------------------------------------------------

/// What the server panel should ask about one client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// The client asks for its PIN and none has been typed.
    Needed,
    /// The PIN typed was wrong; this many tries left.
    Wrong(u32),
    /// Too many wrong PINs; the client accepts none until this moment.
    Locked(Instant),
}

#[derive(Default)]
struct ServerState {
    /// PIN typed per client, by device ID (R44): a name is only a label, and two clients
    /// can share one.
    typed: HashMap<String, String>,
    /// The client waiting for a PIN, by device ID, with its name for the panel, and why.
    prompt: Option<(String, String, Prompt)>,
}

static SERVER: Mutex<Option<ServerState>> = Mutex::new(None);

fn with_server<T>(f: impl FnOnce(&mut ServerState) -> T) -> T {
    let mut g = SERVER.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(ServerState::default))
}

/// The PIN typed this run for the client with device ID `client`, if any.
pub fn typed_for(client: &str) -> Option<String> {
    with_server(|s| s.typed.get(client).cloned())
}

/// The user typed the PIN for the client being asked about: keep it for this run. The
/// client retries every couple of seconds while it waits, so it connects by itself
/// straight after. Returns that client's name, or None if nobody is being asked about.
pub fn enter(pin: String) -> Option<String> {
    with_server(|s| {
        let (id, name, _) = s.prompt.take()?;
        s.typed.insert(id, pin);
        Some(name)
    })
}

/// Which client is waiting for a PIN (by name), and why.
pub fn prompt() -> Option<(String, Prompt)> {
    with_server(|s| s.prompt.as_ref().map(|(_, name, p)| (name.clone(), p.clone())))
}

/// The client with device ID `client` accepted us: nothing to ask about it.
pub fn accepted(client: &str) {
    with_server(|s| {
        if s.prompt.as_ref().is_some_and(|(c, ..)| c == client) {
            s.prompt = None;
        }
    })
}

/// The client with device ID `client`, called `name`, refused us with `reason`. Returns
/// true if it was a PIN refusal, which then sets what the panel asks and forgets a PIN
/// that did not work.
pub fn note_refusal(client: &str, name: &str, reason: &str) -> bool {
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
    with_server(|s| {
        s.typed.remove(client);
        s.prompt = Some((client.to_owned(), name.to_owned(), prompt));
    });
    true
}

/// Tests that touch the process-wide PIN or pairing state take this first.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_is_four_or_more_characters_of_anything() {
        assert!(is_valid("0420"));
        assert!(is_valid("Tr0ub4dor&3 long one"));
        assert!(!is_valid("042"));
        assert!(!is_valid("ab\ncd"));
        assert!(!is_valid(&"x".repeat(MAX_LEN + 1)));
    }

    #[test]
    fn the_proof_checks_the_pin_and_the_session() {
        let p = proof_hex("key", "1234", b"tls", "c", "s");
        assert!(verify("key", "1234", b"tls", "c", "s", &p));
        assert!(!verify("key", "1235", b"tls", "c", "s", &p), "wrong PIN");
        assert!(!verify("key", "1234", b"other", "c", "s", &p), "another TLS session");
        assert!(!verify("key", "1234", b"tls", "c", "s2", &p), "other nonces");
        assert!(!verify("other-pairing", "1234", b"tls", "c", "s", &p), "another pairing's key");
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
        assert!(matches!(l.attempt(true, t0 + Duration::from_secs(1)), Verdict::Locked(_)));
        let t1 = t0 + FIRST_LOCK + Duration::from_secs(1);
        for _ in 1..MAX_TRIES {
            l.attempt(false, t1);
        }
        assert_eq!(l.attempt(false, t1), Verdict::Locked(FIRST_LOCK * 2));
        let t2 = t1 + FIRST_LOCK * 2 + Duration::from_secs(1);
        assert_eq!(l.attempt(true, t2), Verdict::Accept);
        assert_eq!(l.attempt(false, t2), Verdict::Wrong(MAX_TRIES - 1), "a success resets the count");
    }

    #[test]
    fn refusals_set_the_server_prompt_and_drop_that_clients_pin() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(note_refusal("id-other", "other", "pin_required"));
        assert_eq!(enter("2222".into()).as_deref(), Some("other"));
        assert!(note_refusal("id-kali", "kali", "pin_required"));
        assert_eq!(enter("1111".into()).as_deref(), Some("kali"));
        assert_eq!(typed_for("id-kali").as_deref(), Some("1111"));
        assert!(note_refusal("id-kali", "kali", "wrong_pin:3"));
        assert_eq!(prompt(), Some(("kali".into(), Prompt::Wrong(3))));
        assert_eq!(typed_for("id-kali"), None);
        assert_eq!(typed_for("id-other").as_deref(), Some("2222"), "other clients keep theirs");
        assert!(note_refusal("id-kali", "kali", "pin_required"));
        assert_eq!(prompt(), Some(("kali".into(), Prompt::Needed)));
        assert!(!note_refusal("id-kali", "kali", "bad_secret"));
        enter("4321".into());
        assert_eq!(prompt(), None, "typing the PIN clears the prompt");
    }
}
