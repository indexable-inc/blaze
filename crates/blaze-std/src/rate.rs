//! Pure `#![no_std]`, sans-IO, zero-allocation rate limiting and resilience state machines.
//!
//! Every operation takes an explicit nanosecond timestamp (`now_ns: u64`) or explicit random draw
//! (`rand_u64: u64`): there is no ambient clock, no ambient RNG, and no heap allocation.
//!
//! - [`Gcra`]: Generic Cell Rate Algorithm (virtual-scheduling rate limiter) with `u128`
//!   intermediate arithmetic and exact `retry_after_ns`.
//! - [`TokenBucket`]: exact integer nanosecond token bucket with fractional credit carry and zero
//!   floating-point drift.
//! - [`CircuitBreaker`]: sans-IO three-state (`Closed` / `Open` / `HalfOpen`) failure breaker with
//!   bounded half-open probe concurrency.
//! - [`Backoff`]: deterministic exponential backoff with [`Jitter::None`], [`Jitter::Full`], and
//!   [`Jitter::Equal`] over an explicit caller-supplied `u64` draw.

use core::fmt;
use core::num::{NonZeroU32, NonZeroU64};
use core::time::Duration;

/// Nanoseconds in one second.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Typed configuration refusal returned by rate-limiter and resilience constructors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RateError {
    /// [`GcraOpts::emission_interval_ns`] must be greater than zero.
    ZeroEmissionInterval,
    /// [`GcraOpts::burst`] must be greater than zero.
    ZeroBurst,
    /// `rate_per_sec` must be in `1..=1_000_000_000`.
    InvalidRatePerSec { rate_per_sec: u64 },
    /// [`TokenBucketOpts::capacity`] must be greater than zero.
    ZeroCapacity,
    /// [`TokenBucketOpts::refill_tokens`] must be greater than zero.
    ZeroRefillTokens,
    /// [`TokenBucketOpts::refill_period_ns`] must be greater than zero.
    ZeroRefillPeriod,
    /// [`CircuitOpts::failure_threshold`] must be greater than zero.
    ZeroFailureThreshold,
    /// [`CircuitOpts::success_threshold`] must be greater than zero.
    ZeroSuccessThreshold,
    /// [`CircuitOpts::open_duration_ns`] must be greater than zero.
    ZeroOpenDuration,
    /// [`CircuitOpts::half_open_max_probes`] must be greater than zero.
    ZeroHalfOpenProbes,
    /// [`BackoffOpts::base_ns`] must be greater than zero.
    ZeroBaseDelay,
    /// [`BackoffOpts::max_ns`] must be greater than or equal to [`BackoffOpts::base_ns`].
    MaxBelowBase { base_ns: u64, max_ns: u64 },
    /// [`BackoffOpts::max_attempts`] must be greater than zero.
    ZeroMaxAttempts,
}

impl fmt::Display for RateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RateError::ZeroEmissionInterval => {
                f.write_str("bstd::rate: GCRA emission_interval_ns must be greater than zero")
            }
            RateError::ZeroBurst => {
                f.write_str("bstd::rate: GCRA burst must be greater than zero")
            }
            RateError::InvalidRatePerSec { rate_per_sec } => write!(
                f,
                "bstd::rate: rate_per_sec {rate_per_sec} must be in 1..={NANOS_PER_SEC}"
            ),
            RateError::ZeroCapacity => {
                f.write_str("bstd::rate: token bucket capacity must be greater than zero")
            }
            RateError::ZeroRefillTokens => {
                f.write_str("bstd::rate: token bucket refill_tokens must be greater than zero")
            }
            RateError::ZeroRefillPeriod => {
                f.write_str("bstd::rate: token bucket refill_period_ns must be greater than zero")
            }
            RateError::ZeroFailureThreshold => {
                f.write_str("bstd::rate: circuit breaker failure_threshold must be greater than zero")
            }
            RateError::ZeroSuccessThreshold => {
                f.write_str("bstd::rate: circuit breaker success_threshold must be greater than zero")
            }
            RateError::ZeroOpenDuration => {
                f.write_str("bstd::rate: circuit breaker open_duration_ns must be greater than zero")
            }
            RateError::ZeroHalfOpenProbes => {
                f.write_str("bstd::rate: circuit breaker half_open_max_probes must be greater than zero")
            }
            RateError::ZeroBaseDelay => {
                f.write_str("bstd::rate: backoff base_ns must be greater than zero")
            }
            RateError::MaxBelowBase { base_ns, max_ns } => write!(
                f,
                "bstd::rate: backoff max_ns ({max_ns}) must be at least base_ns ({base_ns})"
            ),
            RateError::ZeroMaxAttempts => {
                f.write_str("bstd::rate: backoff max_attempts must be greater than zero")
            }
        }
    }
}

impl core::error::Error for RateError {}

/// Outcome of a rate-limit admission check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Request admitted; `remaining_burst` unit-cost requests can still be admitted immediately at
    /// the same timestamp (clamped to `u32::MAX`).
    Allowed { remaining_burst: u32 },
    /// Request rejected; retrying at `now_ns + retry_after_ns` will succeed (assuming no
    /// intervening admissions), or `u64::MAX` when the request cost exceeds bucket capacity.
    Limited { retry_after_ns: u64 },
}

impl Decision {
    /// Returns `true` if the request was admitted.
    #[inline]
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        match self {
            Decision::Allowed { .. } => true,
            Decision::Limited { .. } => false,
        }
    }

    /// Returns the retry delay in nanoseconds if the request was limited.
    #[inline]
    #[must_use]
    pub const fn retry_after_ns(self) -> Option<u64> {
        match self {
            Decision::Allowed { .. } => None,
            Decision::Limited { retry_after_ns } => Some(retry_after_ns),
        }
    }

    /// Returns the retry delay as a [`Duration`] if the request was limited.
    #[inline]
    #[must_use]
    pub const fn retry_after(self) -> Option<Duration> {
        match self {
            Decision::Allowed { .. } => None,
            Decision::Limited { retry_after_ns } => Some(Duration::from_nanos(retry_after_ns)),
        }
    }
}

/// Configuration for [`Gcra`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GcraOpts {
    /// Steady-state spacing between unit-cost requests in nanoseconds.
    pub emission_interval_ns: u64,
    /// Maximum number of unit-cost requests admitted in an instantaneous burst.
    pub burst: u32,
}

/// Generic Cell Rate Algorithm (virtual-scheduling) rate limiter.
///
/// Tracks a single theoretical arrival time (`tat_ns`, the time at which the virtual leaky bucket
/// drains completely) in `u128` nanoseconds so intermediate arithmetic never overflows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gcra {
    emission_interval_ns: NonZeroU64,
    burst: NonZeroU32,
    tat_ns: Option<u128>,
}

impl Gcra {
    /// Construct a new GCRA limiter from explicit options.
    pub fn new(opts: GcraOpts) -> Result<Self, RateError> {
        let emission_interval_ns =
            NonZeroU64::new(opts.emission_interval_ns).ok_or(RateError::ZeroEmissionInterval)?;
        let burst = NonZeroU32::new(opts.burst).ok_or(RateError::ZeroBurst)?;
        Ok(Self { emission_interval_ns, burst, tat_ns: None })
    }

    /// Construct a GCRA limiter from a steady-state rate in requests per second and burst size.
    ///
    /// Uses ceiling division (`1_000_000_000.div_ceil(rate_per_sec)`) so the effective steady-state
    /// rate never exceeds `rate_per_sec`.
    pub fn from_rate_per_sec(rate_per_sec: u64, burst: u32) -> Result<Self, RateError> {
        if rate_per_sec == 0 || rate_per_sec > NANOS_PER_SEC {
            return Err(RateError::InvalidRatePerSec { rate_per_sec });
        }
        let emission_interval_ns = NANOS_PER_SEC.div_ceil(rate_per_sec);
        Self::new(GcraOpts { emission_interval_ns, burst })
    }

    /// Configured emission interval in nanoseconds.
    #[inline]
    #[must_use]
    pub const fn emission_interval_ns(&self) -> u64 {
        self.emission_interval_ns.get()
    }

    /// Configured burst capacity in unit-cost requests.
    #[inline]
    #[must_use]
    pub const fn burst(&self) -> u32 {
        self.burst.get()
    }

    /// Reset virtual-scheduling state back to an idle, full-burst bucket.
    #[inline]
    pub fn reset(&mut self) {
        self.tat_ns = None;
    }

    /// Check and admit a single unit-cost request at `now_ns`.
    #[inline]
    pub fn check(&mut self, now_ns: u64) -> Decision {
        self.check_n(now_ns, 1)
    }

    /// Check and admit a request of `cost` units at `now_ns`.
    #[inline]
    pub fn check_n(&mut self, now_ns: u64, cost: u32) -> Decision {
        self.check_n_inner(now_ns, cost, false)
    }

    fn check_n_inner(&mut self, now_ns: u64, cost: u32, plant_extra_burst: bool) -> Decision {
        let now = u128::from(now_ns);
        let interval = u128::from(self.emission_interval_ns.get());
        let burst = u128::from(self.burst.get()) + u128::from(u8::from(plant_extra_burst));
        let burst_allowance = burst * interval;
        let limit = now + burst_allowance;

        if cost == 0 {
            let eff_tat = match self.tat_ns {
                Some(tat) => tat.max(now),
                None => now,
            };
            let rem_time = limit.saturating_sub(eff_tat);
            let remaining_burst = saturating_u128_to_u32(rem_time / interval);
            return Decision::Allowed { remaining_burst };
        }

        if u128::from(cost) > burst {
            return Decision::Limited { retry_after_ns: u64::MAX };
        }

        let increment = u128::from(cost) * interval;
        let base = match self.tat_ns {
            Some(tat) => tat.max(now),
            None => now,
        };
        let new_tat = base + increment;

        if new_tat <= limit {
            self.tat_ns = Some(new_tat);
            let rem_time = limit - new_tat;
            let remaining_burst = saturating_u128_to_u32(rem_time / interval);
            Decision::Allowed { remaining_burst }
        } else {
            let wait = new_tat - limit;
            let retry_after_ns = saturating_u128_to_u64(wait);
            Decision::Limited { retry_after_ns }
        }
    }
}

/// Configuration for [`TokenBucket`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TokenBucketOpts {
    /// Maximum tokens held by the bucket.
    pub capacity: u64,
    /// Number of tokens added per `refill_period_ns`.
    pub refill_tokens: u64,
    /// Refill period in nanoseconds.
    pub refill_period_ns: u64,
}

/// Exact integer nanosecond token bucket with fractional credit carry and zero floating-point drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TokenBucket {
    capacity: NonZeroU64,
    refill_tokens: NonZeroU64,
    refill_period_ns: NonZeroU64,
    tokens: u64,
    last_ns: u64,
    /// Exact fractional credit numerator in `0..refill_period_ns`, where each nanosecond adds
    /// `refill_tokens` credit units and `refill_period_ns` units convert into 1 whole token.
    credit: u128,
}

impl TokenBucket {
    /// Construct a full token bucket initialized at `start_ns`.
    pub fn new(opts: TokenBucketOpts, start_ns: u64) -> Result<Self, RateError> {
        let capacity = NonZeroU64::new(opts.capacity).ok_or(RateError::ZeroCapacity)?;
        let refill_tokens =
            NonZeroU64::new(opts.refill_tokens).ok_or(RateError::ZeroRefillTokens)?;
        let refill_period_ns =
            NonZeroU64::new(opts.refill_period_ns).ok_or(RateError::ZeroRefillPeriod)?;
        Ok(Self {
            capacity,
            refill_tokens,
            refill_period_ns,
            tokens: capacity.get(),
            last_ns: start_ns,
            credit: 0,
        })
    }

    /// Configured maximum token capacity.
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        self.capacity.get()
    }

    fn refill(&mut self, now_ns: u64) {
        if now_ns <= self.last_ns {
            return;
        }
        let elapsed = u128::from(now_ns - self.last_ns);
        self.last_ns = now_ns;

        let cap = self.capacity.get();
        if self.tokens >= cap {
            self.credit = 0;
            return;
        }

        let period = u128::from(self.refill_period_ns.get());
        let rate = u128::from(self.refill_tokens.get());
        let needed_tokens = u128::from(cap - self.tokens);
        let total_credit = self.credit.saturating_add(elapsed.saturating_mul(rate));
        let gained = total_credit / period;

        if gained >= needed_tokens {
            self.tokens = cap;
            self.credit = 0;
        } else {
            self.tokens = self.tokens.saturating_add(saturating_u128_to_u64(gained));
            self.credit = total_credit % period;
        }
    }

    /// Advance the bucket to `now_ns` and return the number of whole tokens currently available.
    pub fn available(&mut self, now_ns: u64) -> u64 {
        self.refill(now_ns);
        self.tokens
    }

    /// Attempt to withdraw `tokens` at `now_ns`.
    ///
    /// On [`Decision::Limited`], `retry_after_ns` is the exact minimum nanosecond delay until
    /// `tokens` whole tokens will be available (or `u64::MAX` if `tokens > capacity`).
    pub fn try_take(&mut self, now_ns: u64, tokens: u64) -> Decision {
        self.refill(now_ns);

        if tokens <= self.tokens {
            self.tokens -= tokens;
            let remaining_burst = saturating_u64_to_u32(self.tokens);
            return Decision::Allowed { remaining_burst };
        }

        if tokens > self.capacity.get() {
            return Decision::Limited { retry_after_ns: u64::MAX };
        }

        let deficit = u128::from(tokens - self.tokens);
        let period = u128::from(self.refill_period_ns.get());
        let rate = u128::from(self.refill_tokens.get());
        let needed_credit = deficit.saturating_mul(period).saturating_sub(self.credit);
        let wait = needed_credit.div_ceil(rate);
        Decision::Limited { retry_after_ns: saturating_u128_to_u64(wait) }
    }
}

/// Current state of a [`CircuitBreaker`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CircuitState {
    /// Normal operation; all calls are admitted.
    Closed,
    /// Circuit is tripped open until `until_ns`; calls before `until_ns` are rejected.
    Open { until_ns: u64 },
    /// Cooldown elapsed; up to `half_open_max_probes` concurrent probe calls are admitted.
    HalfOpen { in_flight: u32 },
}

/// Configuration for [`CircuitBreaker`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CircuitOpts {
    /// Consecutive failures in [`CircuitState::Closed`] required to trip open.
    pub failure_threshold: u32,
    /// Probe successes in [`CircuitState::HalfOpen`] required to close the circuit again.
    pub success_threshold: u32,
    /// How long the circuit remains [`CircuitState::Open`] in nanoseconds before allowing probes.
    pub open_duration_ns: u64,
    /// Maximum concurrent probe calls admitted while [`CircuitState::HalfOpen`].
    pub half_open_max_probes: u32,
}

/// Sans-IO three-state circuit breaker (`Closed` -> `Open` -> `HalfOpen` -> `Closed` / `Open`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CircuitBreaker {
    failure_threshold: NonZeroU32,
    success_threshold: NonZeroU32,
    open_duration_ns: NonZeroU64,
    half_open_max_probes: NonZeroU32,
    state: CircuitState,
    consecutive_failures: u32,
    half_open_successes: u32,
}

impl CircuitBreaker {
    /// Construct a new circuit breaker starting in [`CircuitState::Closed`].
    pub fn new(opts: CircuitOpts) -> Result<Self, RateError> {
        let failure_threshold =
            NonZeroU32::new(opts.failure_threshold).ok_or(RateError::ZeroFailureThreshold)?;
        let success_threshold =
            NonZeroU32::new(opts.success_threshold).ok_or(RateError::ZeroSuccessThreshold)?;
        let open_duration_ns =
            NonZeroU64::new(opts.open_duration_ns).ok_or(RateError::ZeroOpenDuration)?;
        let half_open_max_probes =
            NonZeroU32::new(opts.half_open_max_probes).ok_or(RateError::ZeroHalfOpenProbes)?;
        Ok(Self {
            failure_threshold,
            success_threshold,
            open_duration_ns,
            half_open_max_probes,
            state: CircuitState::Closed,
            consecutive_failures: 0,
            half_open_successes: 0,
        })
    }

    /// Inspect the current [`CircuitState`].
    #[inline]
    #[must_use]
    pub const fn state(&self) -> CircuitState {
        self.state
    }

    /// Reset the breaker to [`CircuitState::Closed`] with zeroed counters.
    #[inline]
    pub fn reset(&mut self) {
        self.state = CircuitState::Closed;
        self.consecutive_failures = 0;
        self.half_open_successes = 0;
    }

    /// Check whether a call is permitted at `now_ns`, advancing `Open -> HalfOpen` once the
    /// open duration has elapsed and reserving a half-open probe slot when admitted.
    pub fn allow(&mut self, now_ns: u64) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open { until_ns } => {
                if now_ns >= until_ns {
                    self.consecutive_failures = 0;
                    self.half_open_successes = 0;
                    self.state = CircuitState::HalfOpen { in_flight: 1 };
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen { in_flight } => {
                if in_flight < self.half_open_max_probes.get() {
                    self.state = CircuitState::HalfOpen { in_flight: in_flight + 1 };
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Record a successful call completion at `now_ns`.
    pub fn record_success(&mut self, now_ns: u64) {
        let _ = now_ns;
        match self.state {
            CircuitState::Closed => {
                self.consecutive_failures = 0;
            }
            CircuitState::Open { until_ns: _ } => {}
            CircuitState::HalfOpen { in_flight } => {
                let remaining = in_flight.saturating_sub(1);
                self.half_open_successes = self.half_open_successes.saturating_add(1);
                if self.half_open_successes >= self.success_threshold.get() {
                    self.state = CircuitState::Closed;
                    self.consecutive_failures = 0;
                    self.half_open_successes = 0;
                } else {
                    self.state = CircuitState::HalfOpen { in_flight: remaining };
                }
            }
        }
    }

    /// Record a failed call completion at `now_ns`.
    pub fn record_failure(&mut self, now_ns: u64) {
        match self.state {
            CircuitState::Closed => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if self.consecutive_failures >= self.failure_threshold.get() {
                    let until_ns = now_ns.saturating_add(self.open_duration_ns.get());
                    self.state = CircuitState::Open { until_ns };
                    self.consecutive_failures = 0;
                    self.half_open_successes = 0;
                }
            }
            CircuitState::HalfOpen { in_flight: _ } => {
                let until_ns = now_ns.saturating_add(self.open_duration_ns.get());
                self.state = CircuitState::Open { until_ns };
                self.consecutive_failures = 0;
                self.half_open_successes = 0;
            }
            CircuitState::Open { until_ns: _ } => {}
        }
    }
}

/// Jitter strategy applied to exponential backoff delays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Jitter {
    /// Deterministic uncapped jitter-free exponential delay `cap`.
    None,
    /// Full jitter: uniform over `[0, cap]`.
    Full,
    /// Equal jitter: uniform over `[cap / 2, cap]`.
    Equal,
}

/// Configuration for [`Backoff`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BackoffOpts {
    /// Initial backoff delay in nanoseconds (attempt 0).
    pub base_ns: u64,
    /// Upper bound on backoff delay in nanoseconds (`max_ns >= base_ns`).
    pub max_ns: u64,
    /// Total number of backoff delays to produce before returning `None`.
    pub max_attempts: u32,
    /// Jitter distribution over the exponential cap.
    pub jitter: Jitter,
}

/// Deterministic exponential backoff state machine with explicit jitter draw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Backoff {
    base_ns: NonZeroU64,
    max_ns: NonZeroU64,
    max_attempts: NonZeroU32,
    jitter: Jitter,
    attempt: u32,
}

impl Backoff {
    /// Construct a new backoff state machine at attempt 0.
    pub fn new(opts: BackoffOpts) -> Result<Self, RateError> {
        let base_ns = NonZeroU64::new(opts.base_ns).ok_or(RateError::ZeroBaseDelay)?;
        if opts.max_ns < opts.base_ns {
            return Err(RateError::MaxBelowBase {
                base_ns: opts.base_ns,
                max_ns: opts.max_ns,
            });
        }
        let max_ns = NonZeroU64::new(opts.max_ns).ok_or(RateError::ZeroBaseDelay)?;
        let max_attempts =
            NonZeroU32::new(opts.max_attempts).ok_or(RateError::ZeroMaxAttempts)?;
        Ok(Self { base_ns, max_ns, max_attempts, jitter: opts.jitter, attempt: 0 })
    }

    /// Number of delays yielded so far.
    #[inline]
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns `true` once `max_attempts` delays have been consumed.
    #[inline]
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.attempt >= self.max_attempts.get()
    }

    /// Reset the attempt counter back to 0.
    #[inline]
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Compute the next backoff delay in nanoseconds using caller-supplied randomness `rand_u64`,
    /// or `None` once `max_attempts` have been exhausted.
    pub fn next_delay_ns(&mut self, rand_u64: u64) -> Option<u64> {
        if self.attempt >= self.max_attempts.get() {
            return None;
        }
        let max = self.max_ns.get();
        let cap = if self.attempt >= 64 {
            max
        } else {
            let exp = u128::from(self.base_ns.get()).saturating_mul(1u128 << self.attempt);
            saturating_u128_to_u64(exp.min(u128::from(max)))
        };
        self.attempt += 1;

        let delay = match self.jitter {
            Jitter::None => cap,
            Jitter::Full => uniform_inclusive(rand_u64, cap),
            Jitter::Equal => {
                let half = cap / 2;
                let span = cap - half;
                half + uniform_inclusive(rand_u64, span)
            }
        };
        Some(delay)
    }

    /// Compute the next backoff delay as a [`Duration`].
    #[inline]
    pub fn next_delay(&mut self, rand_u64: u64) -> Option<Duration> {
        self.next_delay_ns(rand_u64).map(Duration::from_nanos)
    }
}

/// Map `rand_u64` uniformly onto `[0, max_inclusive]` without modulo bias or overflow.
#[inline]
fn uniform_inclusive(rand_u64: u64, max_inclusive: u64) -> u64 {
    let count = u128::from(max_inclusive) + 1;
    let scaled = (u128::from(rand_u64) * count) >> 64;
    saturating_u128_to_u64(scaled)
}

#[inline]
fn saturating_u128_to_u64(value: u128) -> u64 {
    match u64::try_from(value) {
        Ok(v) => v,
        Err(_) => u64::MAX,
    }
}

#[inline]
fn saturating_u128_to_u32(value: u128) -> u32 {
    match u32::try_from(value) {
        Ok(v) => v,
        Err(_) => u32::MAX,
    }
}

#[inline]
fn saturating_u64_to_u32(value: u64) -> u32 {
    match u32::try_from(value) {
        Ok(v) => v,
        Err(_) => u32::MAX,
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::*;

    #[test]
    fn gcra_burst_and_exact_retry_after() {
        assert_eq!(
            Gcra::new(GcraOpts { emission_interval_ns: 0, burst: 1 }),
            Err(RateError::ZeroEmissionInterval)
        );
        assert_eq!(
            Gcra::new(GcraOpts { emission_interval_ns: 100, burst: 0 }),
            Err(RateError::ZeroBurst)
        );
        assert_eq!(
            Gcra::from_rate_per_sec(0, 1),
            Err(RateError::InvalidRatePerSec { rate_per_sec: 0 })
        );
        assert_eq!(
            Gcra::from_rate_per_sec(NANOS_PER_SEC + 1, 1),
            Err(RateError::InvalidRatePerSec { rate_per_sec: NANOS_PER_SEC + 1 })
        );

        let per_sec = Gcra::from_rate_per_sec(10, 2).expect("10 rps");
        assert_eq!(per_sec.emission_interval_ns(), 100_000_000);
        assert_eq!(per_sec.burst(), 2);

        let mut g = Gcra::new(GcraOpts { emission_interval_ns: 100, burst: 3 }).expect("gcra");
        assert_eq!(g.check(0), Decision::Allowed { remaining_burst: 2 });
        assert_eq!(g.check(0), Decision::Allowed { remaining_burst: 1 });
        assert_eq!(g.check(0), Decision::Allowed { remaining_burst: 0 });
        assert_eq!(g.check(0), Decision::Limited { retry_after_ns: 100 });
        assert_eq!(g.check(99), Decision::Limited { retry_after_ns: 1 });
        assert_eq!(g.check(100), Decision::Allowed { remaining_burst: 0 });
        assert_eq!(g.check(100), Decision::Limited { retry_after_ns: 100 });

        // Cost > burst is never satisfiable; cost == 0 inspects without consuming.
        assert_eq!(g.check_n(100, 4), Decision::Limited { retry_after_ns: u64::MAX });
        assert_eq!(g.check_n(100, 0), Decision::Allowed { remaining_burst: 0 });
        assert_eq!(g.check_n(400, 0), Decision::Allowed { remaining_burst: 3 });

        // Multi-unit request with exact retry_after_ns.
        assert_eq!(g.check_n(400, 2), Decision::Allowed { remaining_burst: 1 });
        assert_eq!(g.check_n(400, 2), Decision::Limited { retry_after_ns: 100 });
        assert_eq!(g.check_n(499, 2), Decision::Limited { retry_after_ns: 1 });
        assert_eq!(g.check_n(500, 2), Decision::Allowed { remaining_burst: 0 });
    }

    #[test]
    fn token_bucket_exact_refill_and_zero_drift() {
        assert_eq!(
            TokenBucket::new(
                TokenBucketOpts { capacity: 0, refill_tokens: 1, refill_period_ns: 1 },
                0,
            ),
            Err(RateError::ZeroCapacity)
        );
        assert_eq!(
            TokenBucket::new(
                TokenBucketOpts { capacity: 1, refill_tokens: 0, refill_period_ns: 1 },
                0,
            ),
            Err(RateError::ZeroRefillTokens)
        );
        assert_eq!(
            TokenBucket::new(
                TokenBucketOpts { capacity: 1, refill_tokens: 1, refill_period_ns: 0 },
                0,
            ),
            Err(RateError::ZeroRefillPeriod)
        );

        // 3 tokens per 1000ns => 1 token every 333.333...ns.
        let mut tb = TokenBucket::new(
            TokenBucketOpts { capacity: 10, refill_tokens: 3, refill_period_ns: 1_000 },
            0,
        )
        .expect("bucket");
        assert_eq!(tb.available(0), 10);
        assert_eq!(tb.try_take(0, 10), Decision::Allowed { remaining_burst: 0 });
        assert_eq!(tb.available(0), 0);

        // Polling every 100ns must accumulate fractional credit with zero drift.
        for step in 1u64..=10 {
            let _ = tb.available(step * 100);
        }
        assert_eq!(tb.available(1_000), 3);

        // Exact retry_after_ns for taking 5 when 3 are available (deficit = 2 => ceil(2000/3) = 667ns).
        assert_eq!(tb.try_take(1_000, 5), Decision::Limited { retry_after_ns: 667 });
        assert_eq!(tb.try_take(1_666, 5), Decision::Limited { retry_after_ns: 1 });
        assert_eq!(tb.try_take(1_667, 5), Decision::Allowed { remaining_burst: 0 });

        // Taking more than capacity returns u64::MAX wait.
        assert_eq!(tb.try_take(10_000, 11), Decision::Limited { retry_after_ns: u64::MAX });
    }

    #[test]
    fn circuit_breaker_transitions() {
        assert_eq!(
            CircuitBreaker::new(CircuitOpts {
                failure_threshold: 0,
                success_threshold: 1,
                open_duration_ns: 10,
                half_open_max_probes: 1,
            }),
            Err(RateError::ZeroFailureThreshold)
        );
        assert_eq!(
            CircuitBreaker::new(CircuitOpts {
                failure_threshold: 1,
                success_threshold: 0,
                open_duration_ns: 10,
                half_open_max_probes: 1,
            }),
            Err(RateError::ZeroSuccessThreshold)
        );
        assert_eq!(
            CircuitBreaker::new(CircuitOpts {
                failure_threshold: 1,
                success_threshold: 1,
                open_duration_ns: 0,
                half_open_max_probes: 1,
            }),
            Err(RateError::ZeroOpenDuration)
        );
        assert_eq!(
            CircuitBreaker::new(CircuitOpts {
                failure_threshold: 1,
                success_threshold: 1,
                open_duration_ns: 10,
                half_open_max_probes: 0,
            }),
            Err(RateError::ZeroHalfOpenProbes)
        );

        let mut cb = CircuitBreaker::new(CircuitOpts {
            failure_threshold: 3,
            success_threshold: 2,
            open_duration_ns: 1_000,
            half_open_max_probes: 2,
        })
        .expect("cb");
        assert_eq!(cb.state(), CircuitState::Closed);

        // Two failures then a success resets the failure streak.
        assert!(cb.allow(10));
        cb.record_failure(11);
        cb.record_failure(12);
        cb.record_success(13);
        assert_eq!(cb.state(), CircuitState::Closed);

        // Three consecutive failures trip Open until 100 + 1000 = 1100.
        cb.record_failure(90);
        cb.record_failure(95);
        cb.record_failure(100);
        assert_eq!(cb.state(), CircuitState::Open { until_ns: 1_100 });
        assert!(!cb.allow(1_099));

        // Cooldown expires at 1100 -> HalfOpen with bounded probes.
        assert!(cb.allow(1_100));
        assert_eq!(cb.state(), CircuitState::HalfOpen { in_flight: 1 });
        assert!(cb.allow(1_101));
        assert_eq!(cb.state(), CircuitState::HalfOpen { in_flight: 2 });
        assert!(!cb.allow(1_102));

        // A failure during HalfOpen immediately re-opens the circuit.
        cb.record_failure(1_105);
        assert_eq!(cb.state(), CircuitState::Open { until_ns: 2_105 });
        assert!(!cb.allow(2_104));

        // Next cooldown: two probe successes close the circuit.
        assert!(cb.allow(2_105));
        cb.record_success(2_110);
        assert_eq!(cb.state(), CircuitState::HalfOpen { in_flight: 0 });
        assert!(cb.allow(2_111));
        cb.record_success(2_112);
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.allow(2_113));
    }

    #[test]
    fn backoff_cap_exhaustion_and_jitter_bounds() {
        assert_eq!(
            Backoff::new(BackoffOpts {
                base_ns: 0,
                max_ns: 100,
                max_attempts: 3,
                jitter: Jitter::None,
            }),
            Err(RateError::ZeroBaseDelay)
        );
        assert_eq!(
            Backoff::new(BackoffOpts {
                base_ns: 100,
                max_ns: 50,
                max_attempts: 3,
                jitter: Jitter::None,
            }),
            Err(RateError::MaxBelowBase { base_ns: 100, max_ns: 50 })
        );
        assert_eq!(
            Backoff::new(BackoffOpts {
                base_ns: 10,
                max_ns: 100,
                max_attempts: 0,
                jitter: Jitter::None,
            }),
            Err(RateError::ZeroMaxAttempts)
        );

        let mut none = Backoff::new(BackoffOpts {
            base_ns: 100,
            max_ns: 350,
            max_attempts: 4,
            jitter: Jitter::None,
        })
        .expect("none");
        assert_eq!(none.attempt(), 0);
        assert_eq!(none.next_delay_ns(123), Some(100));
        assert_eq!(none.next_delay_ns(456), Some(200));
        assert_eq!(none.next_delay_ns(789), Some(350));
        assert_eq!(none.next_delay_ns(0), Some(350));
        assert_eq!(none.attempt(), 4);
        assert!(none.is_exhausted());
        assert_eq!(none.next_delay_ns(0), None);
        none.reset();
        assert_eq!(none.attempt(), 0);
        assert_eq!(none.next_delay_ns(0), Some(100));

        // Full jitter spans [0, cap].
        let mut full = Backoff::new(BackoffOpts {
            base_ns: 100,
            max_ns: 1_000,
            max_attempts: 3,
            jitter: Jitter::Full,
        })
        .expect("full");
        assert_eq!(full.next_delay_ns(0), Some(0));
        assert_eq!(full.next_delay_ns(u64::MAX), Some(200));
        let mid = full.next_delay_ns(1u64 << 63).expect("mid");
        assert!(mid <= 400);

        // Equal jitter spans [cap / 2, cap].
        let mut equal = Backoff::new(BackoffOpts {
            base_ns: 100,
            max_ns: 1_000,
            max_attempts: 3,
            jitter: Jitter::Equal,
        })
        .expect("equal");
        assert_eq!(equal.next_delay_ns(0), Some(50));
        assert_eq!(equal.next_delay_ns(u64::MAX), Some(200));
        let mid_eq = equal.next_delay_ns(1u64 << 63).expect("mid_eq");
        assert!((200..=400).contains(&mid_eq));

        // High attempt count never overflows shift.
        let mut high = Backoff::new(BackoffOpts {
            base_ns: 10,
            max_ns: 1_000,
            max_attempts: 100,
            jitter: Jitter::None,
        })
        .expect("high");
        for _ in 0..70 {
            let d = high.next_delay_ns(0).expect("delay");
            assert!(d <= 1_000);
        }
    }

    fn verify_gcra_case<A: Allocator + Copy>(
        tape: &Tape,
        plant_extra_burst: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let interval = tape.int_in(1u64..=10_000, "interval");
        let burst = tape.int_in(1u32..=10, "burst");
        let mut now = tape.int_in(0u64..=1_000_000, "start_ns");
        let steps = tape.int_in(12usize..=40, "steps");

        let mut limiter = Gcra::new(GcraOpts { emission_interval_ns: interval, burst })
            .map_err(|e| Broke::Harness(format_in!(alloc, "gcra new: {e}").as_str().into()))?;
        let mut admitted = OwnVec::<u64, A>::new_in(alloc);

        for _ in 0..steps {
            let dt = if tape.coin("same_ts") {
                0
            } else {
                tape.int_in(0u64..=interval.saturating_mul(2), "dt")
            };
            now = now.saturating_add(dt);

            let snapshot = limiter;
            match limiter.check_n_inner(now, 1, plant_extra_burst) {
                Decision::Allowed { .. } => {
                    admitted.push(now);
                }
                Decision::Limited { retry_after_ns } => {
                    if retry_after_ns == 0 {
                        return Err(Broke::Oracle(
                            format_in!(alloc, "limited returned zero retry_after_ns at {now}")
                                .as_str()
                                .into(),
                        ));
                    }
                    let mut retry_copy = snapshot;
                    let retry_at = now.saturating_add(retry_after_ns);
                    if !retry_copy.check_n_inner(retry_at, 1, plant_extra_burst).is_allowed() {
                        return Err(Broke::Oracle(
                            format_in!(
                                alloc,
                                "retry at now({now}) + retry_after({retry_after_ns}) = {retry_at} was rejected"
                            )
                            .as_str()
                            .into(),
                        ));
                    }
                    if retry_after_ns > 1 {
                        let mut early_copy = snapshot;
                        let early_at = now.saturating_add(retry_after_ns - 1);
                        if early_copy.check_n_inner(early_at, 1, plant_extra_burst).is_allowed() {
                            return Err(Broke::Oracle(
                                format_in!(
                                    alloc,
                                    "retry_after_ns={retry_after_ns} was not minimal: {early_at} already succeeded"
                                )
                                .as_str()
                                .into(),
                            ));
                        }
                    }
                }
            }
        }

        let slice = admitted.as_slice();
        for (i, &t_start) in slice.iter().enumerate() {
            for (offset, &t_end) in slice.get(i..).unwrap_or(&[]).iter().enumerate() {
                let window = t_end - t_start;
                let count = match u128::try_from(offset + 1) {
                    Ok(v) => v,
                    Err(_) => u128::MAX,
                };
                let max_allowed = u128::from(burst) + u128::from(window) / u128::from(interval);
                if count > max_allowed {
                    return Err(Broke::Oracle(
                        format_in!(
                            alloc,
                            "GCRA admitted {count} requests in window {window}ns (burst={burst}, interval={interval}, max={max_allowed})"
                        )
                        .as_str()
                        .into(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The invariant: GCRA never admits more than burst plus floor(window / emission_interval) requests over any non-decreasing timestamp sequence, and retrying a limited request at now + retry_after_ns always succeeds.
    #[test]
    fn property_gcra_window_bound_and_exact_retry() {
        let h = heap();
        check(Config::cases(256), |tape| verify_gcra_case(tape, false, h.alloc()));
    }

    #[test]
    fn the_oracle_catches_gcra_burst_overflow() {
        let h = heap();
        control(Config::cases(128), false, true, "", |tape, plant| {
            verify_gcra_case(tape, plant, h.alloc())
        });
    }
}
