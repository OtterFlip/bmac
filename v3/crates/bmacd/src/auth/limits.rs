// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Sliding-window limits on login attempts, per source address and per user
//! name.

use std::collections::{HashMap, VecDeque};

/// Keys tracked at once; past this, the key idle longest is forgotten.
pub const MAX_KEYS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub limit: usize,
    pub period_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limited {
    pub retry_after_seconds: u64,
    /// The first refusal since the key became limited, so it is audited once.
    pub first: bool,
}

#[derive(Debug, Default)]
struct Hits {
    times: VecDeque<i64>,
    announced: bool,
}

#[derive(Debug)]
pub struct Limiter {
    window: Window,
    keys: HashMap<String, Hits>,
}

impl Limiter {
    pub fn new(window: Window) -> Limiter {
        Limiter {
            window,
            keys: HashMap::new(),
        }
    }

    /// Refuses when `key` already has `limit` hits within the window.
    pub fn check(&mut self, key: &str, now: i64) -> Result<(), Limited> {
        let window = self.window;
        let Some(hits) = self.keys.get_mut(key) else {
            return Ok(());
        };
        while hits
            .times
            .front()
            .is_some_and(|t| *t <= now - window.period_ms)
        {
            hits.times.pop_front();
        }
        if hits.times.len() < window.limit {
            hits.announced = false;
            return Ok(());
        }
        let oldest = hits.times.front().copied().unwrap_or(now);
        let wait_ms = (oldest + window.period_ms - now).max(1000);
        let first = !hits.announced;
        hits.announced = true;
        Err(Limited {
            retry_after_seconds: (wait_ms as u64).div_ceil(1000),
            first,
        })
    }

    pub fn record(&mut self, key: &str, now: i64) {
        if !self.keys.contains_key(key) && self.keys.len() >= MAX_KEYS {
            self.forget_one(now);
        }
        let hits = self.keys.entry(key.to_string()).or_default();
        hits.times.push_back(now);
        while hits.times.len() > self.window.limit {
            hits.times.pop_front();
        }
    }

    fn forget_one(&mut self, now: i64) {
        let period = self.window.period_ms;
        self.keys
            .retain(|_, hits| hits.times.back().is_some_and(|t| *t > now - period));
        if self.keys.len() >= MAX_KEYS
            && let Some(idle) = self
                .keys
                .iter()
                .min_by_key(|(_, hits)| hits.times.back().copied().unwrap_or(i64::MIN))
                .map(|(key, _)| key.clone())
        {
            self.keys.remove(&idle);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_past_the_limit_until_the_window_moves() {
        let mut limiter = Limiter::new(Window {
            limit: 3,
            period_ms: 60_000,
        });
        for t in [0, 1000, 2000] {
            limiter.check("a", t).unwrap();
            limiter.record("a", t);
        }
        let refused = limiter.check("a", 3000).unwrap_err();
        assert_eq!(
            refused,
            Limited {
                retry_after_seconds: 57,
                first: true
            }
        );
        assert!(!limiter.check("a", 3500).unwrap_err().first);
        limiter.check("b", 3000).unwrap();
        limiter.check("a", 60_000).unwrap();
        limiter.record("a", 60_000);
        assert!(
            limiter.check("a", 60_500).unwrap_err().first,
            "announced again after recovering"
        );
    }

    #[test]
    fn the_key_table_is_bounded() {
        let mut limiter = Limiter::new(Window {
            limit: 1,
            period_ms: 1_000_000,
        });
        for n in 0..MAX_KEYS + 5 {
            limiter.record(&n.to_string(), n as i64);
        }
        assert_eq!(limiter.len(), MAX_KEYS);
        assert!(
            limiter.check("0", 20_000).is_ok(),
            "the oldest key was forgotten"
        );
        assert!(limiter.check(&(MAX_KEYS + 4).to_string(), 20_000).is_err());
    }
}
