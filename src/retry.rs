//! Exponential-backoff policy shared by the retry loops scattered across the
//! providers.
//!
//! A [`RetryPolicy`] is a pure value: given an attempt number it returns a
//! delay, with no internal mutable state. That is what makes the sequence for
//! a given policy assertable directly in a test, instead of only observable by
//! running a loop and recording what happens.
//!
//! # Attempt indexing
//!
//! `attempt` is **0-based**: `delay_for(0)` is the delay before the *first*
//! retry (i.e. `base`), `delay_for(1)` is `2 * base`, and so on. Callers that
//! count attempts from 1 (as the hand-rolled loops predating this type do)
//! must subtract 1 before calling in.

use std::time::Duration;

/// How a fallible operation spaces its retries: `base * 2^attempt`, clamped to
/// `ceiling`, with an optional attempt count after which the caller should
/// give up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    base: Duration,
    ceiling: Duration,
    /// `None` means retry forever (e.g. the IP-geolocation fallback, which
    /// never has a "give up" state — it just keeps polling at `ceiling`).
    give_up_after: Option<u32>,
}

impl RetryPolicy {
    pub const fn new(base: Duration, ceiling: Duration, give_up_after: Option<u32>) -> Self {
        Self {
            base,
            ceiling,
            give_up_after,
        }
    }

    /// Delay before retry `attempt` (0-based): `base * 2^attempt`, saturating
    /// at `ceiling`. Never panics or overflows regardless of `attempt` —
    /// unlike a raw `1u32 << attempt` (which panics once the shift exceeds 31
    /// bits), this multiplies in `u128` nanoseconds and short-circuits to
    /// `ceiling` the moment the running value would reach or exceed it.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let ceiling_nanos = self.ceiling.as_nanos();
        let base_nanos = self.base.as_nanos();

        // 2^attempt as u128 would itself overflow for attempt >= 128, so cap
        // the shift and let the ceiling comparison below do the rest — any
        // shift that large has long since exceeded any real ceiling.
        let shift = attempt.min(127);
        let multiplier = 1u128 << shift;

        match base_nanos.checked_mul(multiplier) {
            Some(nanos) if nanos < ceiling_nanos => Duration::from_nanos(nanos as u64),
            _ => self.ceiling,
        }
    }

    /// True once `attempt` is at or past the give-up count. Always false when
    /// `give_up_after` is `None` (retry forever).
    pub fn exhausted(&self, attempt: u32) -> bool {
        match self.give_up_after {
            Some(limit) => attempt >= limit,
            None => false,
        }
    }

    /// Double `current`, saturating at `ceiling`. For callers whose backoff
    /// baseline can be redirected mid-sequence by an external signal (a server
    /// `Retry-After` header) rather than derived purely from an attempt count —
    /// they seed `current` from that signal, then advance it through this. Like
    /// [`Self::delay_for`], overflow-safe: doubling in `u128` nanos, clamped.
    pub fn double(&self, current: Duration) -> Duration {
        let ceiling_nanos = self.ceiling.as_nanos();
        match current.as_nanos().checked_mul(2) {
            Some(nanos) if nanos < ceiling_nanos => Duration::from_nanos(nanos as u64),
            _ => self.ceiling,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_from_base() {
        let policy = RetryPolicy::new(Duration::from_secs(1), Duration::from_secs(1000), None);
        assert_eq!(policy.delay_for(0), Duration::from_secs(1));
        assert_eq!(policy.delay_for(1), Duration::from_secs(2));
        assert_eq!(policy.delay_for(2), Duration::from_secs(4));
    }

    #[test]
    fn clamps_to_ceiling_and_stays_there() {
        // 1,2,4,8,16 then overshoots a ceiling of 10.
        let policy = RetryPolicy::new(Duration::from_secs(1), Duration::from_secs(10), None);
        assert_eq!(policy.delay_for(3), Duration::from_secs(8));
        assert_eq!(policy.delay_for(4), Duration::from_secs(10)); // 16 -> clamped
        assert_eq!(policy.delay_for(5), Duration::from_secs(10));
        assert_eq!(policy.delay_for(100), Duration::from_secs(10));
    }

    /// `app.rs`'s `FrameRetry`: base 2s, ceiling 90s, give up after 8 attempts.
    /// Hand-computed against `BASE.saturating_mul(1u32 << attempts.min(6)).min(90)`:
    /// 2, 4, 8, 16, 32, 64, then 2*2^6=128 clamps to 90 from attempt 6 on.
    #[test]
    fn reproduces_frame_retry_sequence() {
        let policy = RetryPolicy::new(Duration::from_secs(2), Duration::from_secs(90), Some(8));
        let expected = [2u64, 4, 8, 16, 32, 64, 90, 90, 90];
        for (attempt, secs) in expected.into_iter().enumerate() {
            assert_eq!(
                policy.delay_for(attempt as u32),
                Duration::from_secs(secs),
                "attempt {attempt}"
            );
        }
    }

    /// `location/ip.rs`: base 60s, ceiling 30 min (1800s), retries forever.
    /// Hand-computed against `backoff = (backoff * 2).min(MAX_RETRY_INTERVAL)`
    /// starting at `RETRY_INTERVAL`: 60, 120, 240, 480, 960, then 1920 clamps
    /// to 1800.
    #[test]
    fn reproduces_ip_fallback_sequence() {
        let policy = RetryPolicy::new(Duration::from_secs(60), Duration::from_secs(30 * 60), None);
        let expected = [60u64, 120, 240, 480, 960, 1800, 1800];
        for (attempt, secs) in expected.into_iter().enumerate() {
            assert_eq!(
                policy.delay_for(attempt as u32),
                Duration::from_secs(secs),
                "attempt {attempt}"
            );
        }
        assert!(!policy.exhausted(u32::MAX));
    }

    /// `meteogate.rs`'s `download_geotiff`: base 400ms, ceiling 120s, gives up
    /// after `DOWNLOAD_ATTEMPTS` (3) total attempts, i.e. 2 retries. Hand-
    /// computed against `delay = (delay * 2).min(MAX_RETRY_AFTER)` starting at
    /// `DOWNLOAD_RETRY_BASE`: 400ms, 800ms (well under the 120s ceiling).
    #[test]
    fn reproduces_meteogate_download_sequence() {
        let policy = RetryPolicy::new(
            Duration::from_millis(400),
            Duration::from_secs(120),
            Some(3),
        );
        assert_eq!(policy.delay_for(0), Duration::from_millis(400));
        assert_eq!(policy.delay_for(1), Duration::from_millis(800));

        assert!(!policy.exhausted(2));
        assert!(policy.exhausted(3));
    }

    #[test]
    fn exhausted_boundary() {
        let policy = RetryPolicy::new(Duration::from_secs(1), Duration::from_secs(1), Some(3));
        assert!(!policy.exhausted(0));
        assert!(!policy.exhausted(1));
        assert!(!policy.exhausted(2));
        assert!(policy.exhausted(3));
        assert!(policy.exhausted(4));
    }

    #[test]
    fn exhausted_never_true_when_unbounded() {
        let policy = RetryPolicy::new(Duration::from_secs(1), Duration::from_secs(1), None);
        assert!(!policy.exhausted(0));
        assert!(!policy.exhausted(u32::MAX));
    }

    #[test]
    fn double_advances_and_clamps() {
        // ceiling 120s, base irrelevant to `double`.
        let policy = RetryPolicy::new(
            Duration::from_millis(400),
            Duration::from_secs(120),
            Some(3),
        );
        assert_eq!(
            policy.double(Duration::from_millis(400)),
            Duration::from_millis(800)
        );
        // Carries forward from an arbitrary (e.g. Retry-After) seed, not a
        // base-derived value: 30s doubles to 60s, then clamps at the 120s ceiling.
        assert_eq!(
            policy.double(Duration::from_secs(30)),
            Duration::from_secs(60)
        );
        assert_eq!(
            policy.double(Duration::from_secs(80)),
            Duration::from_secs(120)
        );
        assert_eq!(
            policy.double(Duration::from_secs(120)),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn double_does_not_overflow() {
        let policy = RetryPolicy::new(Duration::from_secs(1), Duration::from_secs(120), None);
        assert_eq!(policy.double(Duration::MAX), Duration::from_secs(120));
    }

    #[test]
    fn no_overflow_at_max_attempt() {
        let policy = RetryPolicy::new(Duration::from_secs(2), Duration::from_secs(90), Some(8));
        assert_eq!(policy.delay_for(u32::MAX), Duration::from_secs(90));
    }
}
