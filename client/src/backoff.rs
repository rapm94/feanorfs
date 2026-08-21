//! One explicit exponential-backoff policy shared by the watcher, the live
//! agent controller, the unattended agent runner, and the supervisor's child
//! restart loop.
//!
//! Each caller owns a configured [`ExponentialBackoff`] constant so the four
//! historical retry sequences are preserved exactly. The policy tests in this
//! module characterize those sequences as a regression gate: changing a
//! caller's policy values must be a deliberate, separately approved change.

use std::time::Duration;

/// Largest exponent shift applied by [`ExponentialBackoff`]. Keeps
/// `2^shift` representable in `u128` and the policy bounded even for extreme
/// configured values of `max_shift`.
const MAX_SHIFT: u32 = 127;

/// How the backoff multiplier grows with the failure count.
///
/// For a failure count `n >= 1` the delay is
/// `min(base * 2^shift(n) * (1 + jitter(n)), cap)` where `shift(n)` is:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffGrowth {
    /// `shift(n) = min(n, max_shift)`: the first failure already doubles the
    /// base, so `delay(1) = 2 * base`.
    DoublesFromFirstFailure,
    /// `shift(n) = min(n - 1, max_shift)`: the first failure yields exactly
    /// the base and growth starts with the second failure, so
    /// `delay(1) = base`.
    DoublesFromSecondFailure,
}

/// Delay returned for a zero failure count (the reset state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffReset {
    /// `delay(0) = Duration::ZERO`: a reset is never delayed.
    Immediate,
    /// `delay(0) = min(base, cap)`: the base delay applies even before the
    /// first failure.
    Base,
}

/// Optional deterministic jitter applied on top of the exponential delay.
///
/// The jitter is a pure function of the failure count (a fixed SplitMix64
/// hash), not of a clock or RNG, so jittered sequences are fully reproducible
/// in tests. It only ever lengthens a failure-driven delay by at most
/// `percent` percent and is applied before the cap, so the cap stays absolute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Jitter {
    /// Maximum extra delay as a percentage of the unjittered delay, 0..=100.
    percent: u32,
}

impl Jitter {
    /// Jitter that lengthens a failure-driven delay by at most `percent`
    /// percent.
    ///
    /// # Panics
    ///
    /// Panics if `percent > 100`.
    pub const fn percent(percent: u32) -> Self {
        assert!(percent <= 100, "jitter percent must be at most 100");
        Self { percent }
    }
}

/// Explicit exponential backoff policy.
///
/// Contract:
///
/// ```text
/// delay(0) = 0                                   if reset == Immediate
/// delay(0) = min(base, cap)                      if reset == Base
/// delay(n) = min(base * 2^shift(n) * (1 + jitter(n)), cap)   for n >= 1
/// ```
///
/// `shift(n)` follows the configured [`BackoffGrowth`] convention and is
/// clamped to `max_shift` (and to `MAX_SHIFT`). All arithmetic saturates
/// and the result never exceeds `cap`, even with jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExponentialBackoff {
    base: Duration,
    cap: Duration,
    max_shift: u32,
    growth: BackoffGrowth,
    reset: BackoffReset,
    jitter: Option<Jitter>,
}

impl ExponentialBackoff {
    /// Policy with `base`, an absolute `cap`, the common
    /// [`BackoffGrowth::DoublesFromSecondFailure`] convention, an immediate
    /// reset, `max_shift = 6`, and no jitter.
    pub const fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            max_shift: 6,
            growth: BackoffGrowth::DoublesFromSecondFailure,
            reset: BackoffReset::Immediate,
            jitter: None,
        }
    }

    /// Set the exponent convention. Const, so policies can be module-level
    /// constants.
    pub const fn with_growth(mut self, growth: BackoffGrowth) -> Self {
        self.growth = growth;
        self
    }

    /// Set the zero-failure (reset) delay policy.
    pub const fn with_reset(mut self, reset: BackoffReset) -> Self {
        self.reset = reset;
        self
    }

    /// Bound the exponent: `shift(n)` is clamped to `max_shift` before the
    /// multiplier is computed.
    pub const fn with_max_shift(mut self, max_shift: u32) -> Self {
        self.max_shift = max_shift;
        self
    }

    /// Enable deterministic jitter (see [`Jitter`]).
    pub const fn with_jitter(mut self, jitter: Option<Jitter>) -> Self {
        self.jitter = jitter;
        self
    }

    /// Delay before the next attempt after `failures` consecutive failures.
    pub fn delay(&self, failures: u32) -> Duration {
        if failures == 0 {
            return match self.reset {
                BackoffReset::Immediate => Duration::ZERO,
                BackoffReset::Base => self.base.min(self.cap),
            };
        }
        let shift = match self.growth {
            BackoffGrowth::DoublesFromFirstFailure => failures.min(self.max_shift),
            BackoffGrowth::DoublesFromSecondFailure => {
                failures.saturating_sub(1).min(self.max_shift)
            }
        }
        .min(MAX_SHIFT);
        let mut nanos = self.base.as_nanos().saturating_mul(1u128 << shift);
        if let Some(jitter) = self.jitter {
            nanos = jitter.apply(nanos, failures);
        }
        Duration::from_nanos(nanos.min(self.cap.as_nanos()) as u64)
    }
}

impl Jitter {
    /// Lengthen `delay_nanos` by `percent * u(failures)` percent, where
    /// `u` is the deterministic hash value in `[0, 2^32)`.
    fn apply(&self, delay_nanos: u128, failures: u32) -> u128 {
        if self.percent == 0 {
            return delay_nanos;
        }
        let unit = jitter_unit(failures);
        let extra = (delay_nanos * u128::from(self.percent) * u128::from(unit)) / (100u128 << 32);
        delay_nanos.saturating_add(extra)
    }
}

/// Fixed SplitMix64 finalizer over the failure count: an RNG-free
/// pseudo-random value in `[0, 2^32)`.
fn jitter_unit(failures: u32) -> u32 {
    let mut z = u64::from(failures).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 32) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors of the four production policies (client/src/watch.rs,
    // client/src/cli/agent_live.rs, client/src/cli/agent_runner.rs,
    // client/src/cli/supervisor.rs). These lock the exact retry sequences the
    // audit characterized; any deliberate change must update both sides.
    const WATCH: ExponentialBackoff =
        ExponentialBackoff::new(Duration::from_secs(5), Duration::from_secs(300))
            .with_growth(BackoffGrowth::DoublesFromFirstFailure)
            .with_reset(BackoffReset::Immediate);

    const LIVE: ExponentialBackoff =
        ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60))
            .with_growth(BackoffGrowth::DoublesFromFirstFailure)
            .with_reset(BackoffReset::Base);

    const RUNNER: ExponentialBackoff =
        ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60))
            .with_growth(BackoffGrowth::DoublesFromSecondFailure)
            .with_reset(BackoffReset::Immediate);

    const SUPERVISOR: ExponentialBackoff =
        ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60))
            .with_growth(BackoffGrowth::DoublesFromSecondFailure)
            .with_reset(BackoffReset::Base);

    #[test]
    fn watcher_policy_sequence_is_preserved() {
        assert_eq!(WATCH.delay(0), Duration::ZERO);
        assert_eq!(WATCH.delay(1), Duration::from_secs(10));
        assert_eq!(WATCH.delay(2), Duration::from_secs(20));
        assert_eq!(WATCH.delay(3), Duration::from_secs(40));
        assert_eq!(WATCH.delay(4), Duration::from_secs(80));
        assert_eq!(WATCH.delay(5), Duration::from_secs(160));
        assert_eq!(WATCH.delay(6), Duration::from_secs(300));
        assert_eq!(WATCH.delay(7), Duration::from_secs(300));
        assert_eq!(WATCH.delay(u32::MAX), Duration::from_secs(300));
    }

    #[test]
    fn live_agent_policy_sequence_is_preserved() {
        assert_eq!(LIVE.delay(0), Duration::from_secs(1));
        assert_eq!(LIVE.delay(1), Duration::from_secs(2));
        assert_eq!(LIVE.delay(2), Duration::from_secs(4));
        assert_eq!(LIVE.delay(3), Duration::from_secs(8));
        assert_eq!(LIVE.delay(4), Duration::from_secs(16));
        assert_eq!(LIVE.delay(5), Duration::from_secs(32));
        assert_eq!(LIVE.delay(6), Duration::from_secs(60));
        assert_eq!(LIVE.delay(7), Duration::from_secs(60));
        assert_eq!(LIVE.delay(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn runner_policy_sequence_is_preserved() {
        assert_eq!(RUNNER.delay(0), Duration::ZERO);
        assert_eq!(RUNNER.delay(1), Duration::from_secs(1));
        assert_eq!(RUNNER.delay(2), Duration::from_secs(2));
        assert_eq!(RUNNER.delay(3), Duration::from_secs(4));
        assert_eq!(RUNNER.delay(4), Duration::from_secs(8));
        assert_eq!(RUNNER.delay(5), Duration::from_secs(16));
        assert_eq!(RUNNER.delay(6), Duration::from_secs(32));
        assert_eq!(RUNNER.delay(7), Duration::from_secs(60));
        assert_eq!(RUNNER.delay(8), Duration::from_secs(60));
        assert_eq!(RUNNER.delay(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn supervisor_policy_sequence_is_preserved() {
        assert_eq!(SUPERVISOR.delay(0), Duration::from_secs(1));
        assert_eq!(SUPERVISOR.delay(1), Duration::from_secs(1));
        assert_eq!(SUPERVISOR.delay(2), Duration::from_secs(2));
        assert_eq!(SUPERVISOR.delay(3), Duration::from_secs(4));
        assert_eq!(SUPERVISOR.delay(4), Duration::from_secs(8));
        assert_eq!(SUPERVISOR.delay(5), Duration::from_secs(16));
        assert_eq!(SUPERVISOR.delay(6), Duration::from_secs(32));
        assert_eq!(SUPERVISOR.delay(7), Duration::from_secs(60));
        assert_eq!(SUPERVISOR.delay(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn growth_conventions_match_documentation() {
        let policy = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(3600));
        let first = policy.with_growth(BackoffGrowth::DoublesFromFirstFailure);
        assert_eq!(first.delay(1), Duration::from_secs(2));
        assert_eq!(first.delay(2), Duration::from_secs(4));
        assert_eq!(first.delay(3), Duration::from_secs(8));

        let second = policy.with_growth(BackoffGrowth::DoublesFromSecondFailure);
        assert_eq!(second.delay(1), Duration::from_secs(1));
        assert_eq!(second.delay(2), Duration::from_secs(2));
        assert_eq!(second.delay(3), Duration::from_secs(4));
    }

    #[test]
    fn reset_policies_match_documentation() {
        let immediate = ExponentialBackoff::new(Duration::from_secs(3), Duration::from_secs(60));
        assert_eq!(immediate.delay(0), Duration::ZERO);

        let base_delay = immediate.with_reset(BackoffReset::Base);
        assert_eq!(base_delay.delay(0), Duration::from_secs(3));

        // The base reset never exceeds the cap.
        let capped = ExponentialBackoff::new(Duration::from_secs(120), Duration::from_secs(30))
            .with_reset(BackoffReset::Base);
        assert_eq!(capped.delay(0), Duration::from_secs(30));
    }

    #[test]
    fn max_shift_bounds_the_multiplier() {
        let bounded = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(3600))
            .with_growth(BackoffGrowth::DoublesFromFirstFailure)
            .with_max_shift(2);
        assert_eq!(bounded.delay(1), Duration::from_secs(2));
        assert_eq!(bounded.delay(2), Duration::from_secs(4));
        assert_eq!(bounded.delay(3), Duration::from_secs(4));
        assert_eq!(bounded.delay(1000), Duration::from_secs(4));
    }

    #[test]
    fn cap_is_absolute_for_all_failure_counts() {
        for backoff in [WATCH, LIVE, RUNNER, SUPERVISOR] {
            for failures in 0..=64 {
                assert!(
                    backoff.delay(failures) <= backoff.cap,
                    "{backoff:?} exceeded its cap at failure count {failures}"
                );
            }
            assert!(backoff.delay(u32::MAX) <= backoff.cap);
        }
    }

    const JITTERED: ExponentialBackoff =
        ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(3600))
            .with_growth(BackoffGrowth::DoublesFromSecondFailure)
            .with_jitter(Some(Jitter::percent(50)));

    #[test]
    fn jitter_is_deterministic() {
        for failures in 0..=32 {
            assert_eq!(
                JITTERED.delay(failures),
                JITTERED.delay(failures),
                "jitter must be a pure function of the failure count"
            );
        }
    }

    #[test]
    fn jitter_only_lengthens_within_its_percent_budget() {
        let plain = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(3600))
            .with_growth(BackoffGrowth::DoublesFromSecondFailure);
        for failures in 1..=64 {
            let base_delay = plain.delay(failures).as_nanos();
            let jittered = JITTERED.delay(failures).as_nanos();
            assert!(
                jittered >= base_delay,
                "jitter must never shorten a delay at failure count {failures}"
            );
            assert!(
                jittered <= base_delay * 150 / 100,
                "jitter exceeded its 50% budget at failure count {failures}"
            );
        }
    }

    #[test]
    fn jitter_respects_the_cap() {
        let capped = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60))
            .with_growth(BackoffGrowth::DoublesFromFirstFailure)
            .with_jitter(Some(Jitter::percent(100)));
        for failures in 1..=64 {
            assert!(capped.delay(failures) <= Duration::from_secs(60));
        }
    }

    #[test]
    fn jitter_spreads_delays_across_failure_counts() {
        let delays: std::collections::BTreeSet<Duration> =
            (1..=12).map(|failures| JITTERED.delay(failures)).collect();
        assert!(
            delays.len() >= 2,
            "the fixed jitter must not collapse all delays into one value"
        );
    }

    #[test]
    #[should_panic(expected = "jitter percent must be at most 100")]
    fn jitter_rejects_oversized_percent() {
        let _ = Jitter::percent(101);
    }
}
