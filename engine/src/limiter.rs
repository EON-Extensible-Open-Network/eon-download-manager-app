// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! A speed limit: a token bucket every connection draws from. IDM only has
//! quotas ("150 MB every 4 hours"); this is a real bytes-per-second limit,
//! global and per download.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Limiter {
    /// Bytes per second; 0 means no limit. Changeable while running.
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter::new(0)
    }
}

impl Limiter {
    pub fn new(rate: u64) -> Limiter {
        Limiter {
            rate: AtomicU64::new(rate),
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }

    pub fn set(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// How long to wait before `bytes` may pass. Takes them from the bucket
    /// now (going into debt), so concurrent callers queue up fairly.
    pub fn charge(&self, bytes: u64) -> Duration {
        let rate = self.rate();
        if rate == 0 {
            return Duration::ZERO;
        }
        let Ok(mut b) = self.bucket.lock() else {
            return Duration::ZERO;
        };
        let now = Instant::now();
        let rate = rate as f64;
        // At most a quarter-second burst, so a limit feels like a limit.
        b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate).min(rate / 4.0);
        b.last = now;
        b.tokens -= bytes as f64;
        if b.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-b.tokens / rate)
        }
    }

    pub async fn take(&self, bytes: u64) {
        let wait = self.charge(bytes);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_limit_never_waits() {
        let l = Limiter::new(0);
        assert_eq!(l.charge(1 << 30), Duration::ZERO);
    }

    #[test]
    fn a_limit_makes_bytes_wait_their_turn() {
        let l = Limiter::new(1_000_000);
        // A megabyte at a megabyte a second: about a second of debt.
        let wait = l.charge(1_000_000);
        assert!(
            wait >= Duration::from_millis(900) && wait <= Duration::from_millis(1100),
            "{wait:?}"
        );
        // Another megabyte queues behind it.
        let wait = l.charge(1_000_000);
        assert!(wait >= Duration::from_millis(1900), "{wait:?}");
        l.set(0);
        assert_eq!(l.charge(1), Duration::ZERO);
    }
}
