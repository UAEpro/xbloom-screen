//! PINs, lockout and admin web sessions.
//!
//! * PINs are 4–8 digits, stored only as **PBKDF2-HMAC-SHA256** hashes with a random salt
//!   ([`PinHash`]); comparisons are constant-time.
//! * Wrong guesses are throttled per channel (the touchscreen and the web are separate, so a
//!   remote guesser can't lock out the person at the machine): after
//!   [`FREE_ATTEMPTS`] failures each further one locks the channel for an escalating period,
//!   from 30 s up to 15 min.
//! * A correct admin PIN over the web yields a random session token (cookie + bearer), valid
//!   for [`SESSION_IDLE`] of inactivity.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::util::random_hex;

/// Failures allowed before the lockout starts.
pub const FREE_ATTEMPTS: u32 = 5;
/// First lockout period; doubles per further failure.
pub const BASE_LOCKOUT: Duration = Duration::from_secs(30);
/// Longest lockout period.
pub const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// A web session expires after this long without a request.
pub const SESSION_IDLE: Duration = Duration::from_secs(12 * 3600);
/// Most concurrent web sessions kept (oldest evicted).
pub const MAX_SESSIONS: usize = 16;

/// A salted PBKDF2 hash of a PIN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinHash {
    /// Random salt, hex.
    pub salt: String,
    /// Derived key, hex.
    pub hash: String,
    /// PBKDF2 rounds (kept with the hash so a PIN set on a Pi verifies on an ESP32).
    pub iterations: u32,
}

/// Is `pin` an acceptable PIN (4–8 ASCII digits)?
pub fn valid_pin(pin: &str) -> bool {
    (4..=8).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

fn derive(pin: &str, salt: &str, iterations: u32) -> String {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(pin.as_bytes(), salt.as_bytes(), iterations.max(1), &mut out);
    xbloom_core::driver::hex(&out)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl PinHash {
    /// Hash `pin` with a fresh salt.
    pub fn new(pin: &str, iterations: u32) -> Self {
        let salt = random_hex(16);
        let hash = derive(pin, &salt, iterations);
        Self { salt, hash, iterations }
    }

    /// Constant-time check of `pin` against this hash.
    pub fn verify(&self, pin: &str) -> bool {
        ct_eq(derive(pin, &self.salt, self.iterations).as_bytes(), self.hash.as_bytes())
    }
}

/// The result of a PIN attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// Correct.
    Ok,
    /// Wrong; this many attempts remain before a lockout (0 = the next wrong one locks).
    Wrong {
        /// Attempts left before the lockout.
        remaining: u32,
    },
    /// Too many wrong attempts; try again after this long.
    Locked {
        /// Time until the next attempt is allowed.
        retry_after: Duration,
    },
}

/// Brute-force throttle for one channel.
#[derive(Debug, Default, Clone)]
pub struct Throttle {
    failures: u32,
    locked_until: Option<Instant>,
}

impl Throttle {
    /// Time left on the current lockout, if any.
    pub fn locked_for(&self, now: Instant) -> Option<Duration> {
        self.locked_until.and_then(|t| t.checked_duration_since(now)).filter(|d| !d.is_zero())
    }

    /// Run one attempt: `check` is only evaluated when not locked out.
    pub fn attempt(&mut self, now: Instant, check: impl FnOnce() -> bool) -> Attempt {
        if let Some(retry_after) = self.locked_for(now) {
            return Attempt::Locked { retry_after };
        }
        if check() {
            self.failures = 0;
            self.locked_until = None;
            return Attempt::Ok;
        }
        self.failures += 1;
        if self.failures >= FREE_ATTEMPTS {
            let doublings = (self.failures - FREE_ATTEMPTS).min(5);
            let lock = (BASE_LOCKOUT * 2u32.pow(doublings)).min(MAX_LOCKOUT);
            self.locked_until = Some(now + lock);
            Attempt::Locked { retry_after: lock }
        } else {
            Attempt::Wrong { remaining: FREE_ATTEMPTS - self.failures - 1 }
        }
    }
}

/// Where a PIN is being entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// The touchscreen.
    Device,
    /// The admin web panel.
    Web,
}

/// Live auth state: throttles and web sessions (in memory; sessions don't survive a reboot).
#[derive(Debug, Default)]
pub struct AuthState {
    throttles: HashMap<Channel, Throttle>,
    sessions: HashMap<String, Instant>,
}

impl AuthState {
    /// Check `pin` against `hashes` (any match succeeds) through the channel's throttle.
    pub fn check(&mut self, channel: Channel, pin: &str, hashes: &[&PinHash]) -> Attempt {
        let now = Instant::now();
        self.throttles
            .entry(channel)
            .or_default()
            .attempt(now, || valid_pin(pin) && hashes.iter().any(|h| h.verify(pin)))
    }

    /// Current lockout on a channel.
    pub fn locked_for(&self, channel: Channel) -> Option<Duration> {
        self.throttles.get(&channel).and_then(|t| t.locked_for(Instant::now()))
    }

    /// Create a web session; returns its token.
    pub fn new_session(&mut self) -> String {
        let now = Instant::now();
        self.sessions.retain(|_, seen| now.duration_since(*seen) < SESSION_IDLE);
        while self.sessions.len() >= MAX_SESSIONS {
            let oldest = self.sessions.iter().min_by_key(|(_, t)| **t).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => self.sessions.remove(&k),
                None => break,
            };
        }
        let token = random_hex(24);
        self.sessions.insert(token.clone(), now);
        token
    }

    /// Validate (and refresh) a session token.
    pub fn touch_session(&mut self, token: &str) -> bool {
        let now = Instant::now();
        // Constant-time-ish: compare against every live token.
        let found = self.sessions.keys().find(|k| ct_eq(k.as_bytes(), token.as_bytes())).cloned();
        match found {
            Some(k) => match self.sessions.get_mut(&k) {
                Some(seen) if now.duration_since(*seen) < SESSION_IDLE => {
                    *seen = now;
                    true
                }
                _ => {
                    self.sessions.remove(&k);
                    false
                }
            },
            None => false,
        }
    }

    /// End a session.
    pub fn end_session(&mut self, token: &str) {
        self.sessions.remove(token);
    }

    /// End every web session (after a PIN change).
    pub fn end_all_sessions(&mut self) {
        self.sessions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_hash_verifies() {
        let h = PinHash::new("2468", 50);
        assert!(h.verify("2468"));
        assert!(!h.verify("2469"));
        assert_ne!(PinHash::new("2468", 50).salt, h.salt);
    }

    #[test]
    fn pin_format() {
        assert!(valid_pin("0000"));
        assert!(valid_pin("12345678"));
        assert!(!valid_pin("123"));
        assert!(!valid_pin("12a4"));
    }

    #[test]
    fn throttle_escalates_then_resets() {
        let mut t = Throttle::default();
        let now = Instant::now();
        for i in 0..4 {
            assert_eq!(t.attempt(now, || false), Attempt::Wrong { remaining: 3 - i });
        }
        assert_eq!(t.attempt(now, || false), Attempt::Locked { retry_after: BASE_LOCKOUT });
        // Locked: the check isn't even run.
        assert!(matches!(t.attempt(now, || panic!("must not check")), Attempt::Locked { .. }));
        let later = now + BASE_LOCKOUT + Duration::from_millis(1);
        assert_eq!(t.attempt(later, || false), Attempt::Locked { retry_after: BASE_LOCKOUT * 2 });
        let much_later = later + MAX_LOCKOUT;
        assert_eq!(t.attempt(much_later, || true), Attempt::Ok);
        assert_eq!(t.attempt(much_later, || false), Attempt::Wrong { remaining: 3 });
    }

    #[test]
    fn channels_are_independent_and_sessions_work() {
        let mut a = AuthState::default();
        let h = PinHash::new("1111", 10);
        for _ in 0..5 {
            a.check(Channel::Web, "0000", &[&h]);
        }
        assert!(a.locked_for(Channel::Web).is_some());
        assert_eq!(a.check(Channel::Device, "1111", &[&h]), Attempt::Ok);
        let tok = a.new_session();
        assert!(a.touch_session(&tok));
        assert!(!a.touch_session("nope"));
        a.end_session(&tok);
        assert!(!a.touch_session(&tok));
    }
}
