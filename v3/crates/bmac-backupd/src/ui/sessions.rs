// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The UI's sessions, in memory only (`SECURITY.md`, "Login"): an opaque
//! 256-bit token kept here as a hash, ended after 30 idle minutes or 12
//! hours, and logins limited per address (30 tries in 5 minutes) and for
//! the one administrator (10 failures in 15 minutes from anywhere). A
//! restart ends every session.

use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;

use base64::Engine as _;
use sha2::{Digest, Sha256};

pub const IDLE_MS: i64 = 30 * 60 * 1000;
pub const ABSOLUTE_MS: i64 = 12 * 3600 * 1000;
const ATTEMPT_WINDOW_MS: i64 = 5 * 60 * 1000;
const ATTEMPTS_PER_ADDRESS: usize = 30;
const FAILURE_WINDOW_MS: i64 = 15 * 60 * 1000;
const FAILURES: usize = 10;
/// Sessions open at once; the oldest goes first.
const MAX_SESSIONS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    pub created_ms: i64,
    pub last_used_ms: i64,
}

impl Session {
    pub fn expires_at_ms(&self) -> i64 {
        self.created_ms + ABSOLUTE_MS
    }

    fn live(&self, now_ms: i64) -> bool {
        now_ms - self.last_used_ms < IDLE_MS && now_ms < self.expires_at_ms()
    }
}

#[derive(Default)]
struct Inner {
    sessions: BTreeMap<[u8; 32], Session>,
    attempts: BTreeMap<IpAddr, VecDeque<i64>>,
    failures: VecDeque<i64>,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<Inner>,
}

fn hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn trim(times: &mut VecDeque<i64>, window_ms: i64, now_ms: i64) {
    while times.front().is_some_and(|t| now_ms - t >= window_ms) {
        times.pop_front();
    }
}

/// Whether a bearer value has the shape of one of these tokens.
pub fn is_token_shaped(token: &str) -> bool {
    token.len() == 43
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Sessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Counts a login attempt from `address`; the seconds to wait when it
    /// is over a limit, in which case the password isn't checked at all.
    pub fn admit(&self, address: IpAddr, now_ms: i64) -> Result<(), u64> {
        let mut inner = self.lock();
        trim(&mut inner.failures, FAILURE_WINDOW_MS, now_ms);
        if inner.failures.len() >= FAILURES {
            let first = inner.failures.front().copied().unwrap_or(now_ms);
            return Err(((first + FAILURE_WINDOW_MS - now_ms) / 1000).max(1) as u64);
        }
        inner
            .attempts
            .retain(|_, times| times.back().is_some_and(|t| now_ms - t < ATTEMPT_WINDOW_MS));
        let times = inner.attempts.entry(address).or_default();
        trim(times, ATTEMPT_WINDOW_MS, now_ms);
        if times.len() >= ATTEMPTS_PER_ADDRESS {
            let first = times.front().copied().unwrap_or(now_ms);
            return Err(((first + ATTEMPT_WINDOW_MS - now_ms) / 1000).max(1) as u64);
        }
        times.push_back(now_ms);
        Ok(())
    }

    pub fn failed(&self, now_ms: i64) {
        self.lock().failures.push_back(now_ms);
    }

    /// A new session and its token, which is never kept.
    pub fn start(&self, now_ms: i64) -> Result<(String, Session), String> {
        let token =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(crate::grants::random(32)?);
        let session = Session {
            created_ms: now_ms,
            last_used_ms: now_ms,
        };
        let mut inner = self.lock();
        inner.sessions.retain(|_, s| s.live(now_ms));
        while inner.sessions.len() >= MAX_SESSIONS {
            let oldest = inner
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.created_ms)
                .map(|(k, _)| *k);
            if let Some(k) = oldest {
                inner.sessions.remove(&k);
            }
        }
        inner.sessions.insert(hash(&token), session);
        Ok((token, session))
    }

    /// The live session `token` names, now used.
    pub fn touch(&self, token: &str, now_ms: i64) -> Option<Session> {
        let key = hash(token);
        let mut inner = self.lock();
        let session = inner.sessions.get_mut(&key)?;
        if !session.live(now_ms) {
            inner.sessions.remove(&key);
            return None;
        }
        session.last_used_ms = now_ms;
        Some(*session)
    }

    pub fn end(&self, token: &str) -> bool {
        self.lock().sessions.remove(&hash(token)).is_some()
    }

    /// Ends every session but `keep`'s, as after a password change.
    pub fn end_others(&self, keep: &str) {
        let key = hash(keep);
        self.lock().sessions.retain(|k, _| *k == key);
    }

    pub fn end_all(&self) {
        self.lock().sessions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: i64 = 1_800_000_000_000;

    #[test]
    fn sessions_end_when_idle_or_old() {
        let sessions = Sessions::default();
        let (token, _) = sessions.start(AT).unwrap();
        assert!(is_token_shaped(&token));
        assert!(sessions.touch(&token, AT + IDLE_MS - 1).is_some());
        assert!(sessions.touch(&token, AT + 2 * IDLE_MS - 2).is_some());
        assert!(sessions.touch(&token, AT + 3 * IDLE_MS).is_none());

        let (token, _) = sessions.start(AT).unwrap();
        let mut t = AT;
        while t + IDLE_MS / 2 < AT + ABSOLUTE_MS {
            t += IDLE_MS / 2;
            assert!(sessions.touch(&token, t).is_some());
        }
        assert!(sessions.touch(&token, AT + ABSOLUTE_MS).is_none());
        assert!(sessions.touch("x".repeat(43).as_str(), AT).is_none());

        let (a, _) = sessions.start(AT).unwrap();
        let (b, _) = sessions.start(AT).unwrap();
        sessions.end_others(&a);
        assert!(sessions.touch(&a, AT).is_some());
        assert!(sessions.touch(&b, AT).is_none());
        assert!(sessions.end(&a));
        assert!(sessions.touch(&a, AT).is_none());
    }

    #[test]
    fn logins_are_limited_per_address_and_for_the_administrator() {
        let sessions = Sessions::default();
        let here: IpAddr = "100.64.0.1".parse().unwrap();
        let there: IpAddr = "100.64.0.2".parse().unwrap();
        for _ in 0..ATTEMPTS_PER_ADDRESS {
            sessions.admit(here, AT).unwrap();
        }
        let wait = sessions.admit(here, AT + 1000).unwrap_err();
        assert_eq!(wait, 299);
        sessions.admit(there, AT).unwrap();
        sessions.admit(here, AT + ATTEMPT_WINDOW_MS).unwrap();

        let sessions = Sessions::default();
        for i in 0..FAILURES {
            sessions.admit(there, AT + i as i64).unwrap();
            sessions.failed(AT + i as i64);
        }
        assert!(sessions.admit(here, AT + 60_000).is_err());
        sessions.admit(here, AT + FAILURE_WINDOW_MS).unwrap();
    }
}
