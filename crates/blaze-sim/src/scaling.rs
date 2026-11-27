//! Empirical complexity scaling oracle: pure `#![no_std]`, sans-IO verification that
//! deterministic work units (`steps`) and allocation bytes (`bytes`) grow within declared
//! asymptotic bounds (`O(1)`, `O(log N)`, `O(N)`, `O(N log N)`, `O(N^2)`).
//!
//! Complements static `#[blaze::cost(...)]` contracts by catching `trusted` contracts, hash
//! collisions, or worklist blowups that drift superlinearly — without ever reading wall-clock time
//! (`Instant::now`), so scaling checks are 100% deterministic across machines and CI load.

#![blaze::sans_io]

use core::error::Error;
use core::fmt;

#[cfg(test)]
mod tests;

/// Asymptotic growth bound for one measurement axis (time steps or allocated bytes).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum ScalingBound {
    /// Constant $O(1)$ growth.
    O1,
    /// Logarithmic $O(\log N)$ growth.
    OLogN,
    /// Linear $O(N)$ growth.
    ON,
    /// Linearithmic $O(N \log N)$ growth.
    ONLogN,
    /// Quadratic $O(N^2)$ growth.
    ON2,
}

impl ScalingBound {
    /// Human-readable asymptotic notation for diagnostics.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn name(self) -> &'static str {
        match self {
            ScalingBound::O1 => "O(1)",
            ScalingBound::OLogN => "O(log N)",
            ScalingBound::ON => "O(N)",
            ScalingBound::ONLogN => "O(N log N)",
            ScalingBound::ON2 => "O(N^2)",
        }
    }
}

impl fmt::Display for ScalingBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which resource axis a [`ScalingViolation`] occurred on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Axis {
    /// Deterministic work / operation count (`WorkSample::steps`).
    Time,
    /// Deterministic allocated / peak byte count (`WorkSample::bytes`).
    Space,
}

impl Axis {
    /// Human-readable axis name for diagnostics.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn name(self) -> &'static str {
        match self {
            Axis::Time => "time",
            Axis::Space => "space",
        }
    }
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Deterministic work and allocation measurement returned by a workload at one input size `N`.
///
/// Never record wall-clock durations here: use deterministic operation counters for `steps` and
/// deterministic allocated or peak byte counters (e.g. from an explicit allocator) for `bytes`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WorkSample {
    /// Deterministic work units / operations performed.
    pub steps: u64,
    /// Deterministic bytes allocated (total or peak).
    pub bytes: u64,
}

impl WorkSample {
    /// Zero steps and zero bytes.
    pub const ZERO: WorkSample = WorkSample { steps: 0, bytes: 0 };

    /// Construct a sample with explicit deterministic `steps` and `bytes`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(steps: u64, bytes: u64) -> WorkSample {
        WorkSample { steps, bytes }
    }

    /// Construct a sample measuring `steps` alone (`bytes = 0`).
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn steps_only(steps: u64) -> WorkSample {
        WorkSample { steps, bytes: 0 }
    }
}

/// Configuration for a two-point empirical scaling check (`n_small` vs `n_large`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScalingCheck {
    /// Smaller input size (at least 1).
    pub n_small: usize,
    /// Larger input size (at least `4 * n_small`).
    pub n_large: usize,
    /// Allowed multiplicative slack above the theoretical growth ratio, in percent (e.g. `50`
    /// allows up to `1.50x` the theoretical ratio).
    pub slack_pct: u32,
}

impl ScalingCheck {
    /// Minimum separation factor (`n_large >= 4 * n_small`) required so constant-factor slack
    /// cannot mask a higher complexity class (e.g. at 4x input with 50% slack, `O(N)` allows up to
    /// `6.00x` while `O(N^2)` grows `16.00x`).
    pub const MIN_SEPARATION: usize = 4;

    /// Default multiplicative slack (50% above theoretical growth ratio).
    pub const DEFAULT_SLACK_PCT: u32 = 50;

    /// Construct a scaling check with [`Self::DEFAULT_SLACK_PCT`] (50% slack).
    ///
    /// # Panics
    ///
    /// Panics if `n_small == 0` or `n_large < 4 * n_small`.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(n_small: usize, n_large: usize) -> ScalingCheck {
        let cfg = ScalingCheck { n_small, n_large, slack_pct: Self::DEFAULT_SLACK_PCT };
        cfg.validate();
        cfg
    }

    /// Override the constant-factor slack percentage.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn with_slack_pct(mut self, slack_pct: u32) -> ScalingCheck {
        self.slack_pct = slack_pct;
        self
    }

    /// Validate that `n_small >= 1` and `n_large >= 4 * n_small`.
    ///
    /// # Panics
    ///
    /// Panics if `n_small == 0` or `n_large < 4 * n_small`.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn validate(self) {
        assert!(self.n_small >= 1, "blaze-sim scaling: `n_small` must be at least 1");
        let min_large = match self.n_small.checked_mul(Self::MIN_SEPARATION) {
            Some(v) => v,
            None => panic!("blaze-sim scaling: `4 * n_small` overflows usize"),
        };
        assert!(
            self.n_large >= min_large,
            "blaze-sim scaling: `n_large` must be at least `4 * n_small`"
        );
    }
}

/// Detailed failure returned when measured growth exceeds the allowed bound on an axis.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScalingViolation {
    /// Which axis (`Time` or `Space`) exceeded its bound.
    pub axis: Axis,
    /// Expected asymptotic bound that was violated.
    pub bound: ScalingBound,
    /// Smaller input size `N`.
    pub n_small: usize,
    /// Larger input size `N`.
    pub n_large: usize,
    /// Measured value at `n_small`.
    pub small_val: u64,
    /// Measured value at `n_large`.
    pub large_val: u64,
    /// Observed growth ratio `ceil(large_val * 100 / small_val)` (or `u64::MAX` when
    /// `small_val == 0 && large_val > 0`).
    pub observed_ratio_x100: u64,
    /// Maximum allowed growth ratio `floor(theoretical_ratio * (100 + slack_pct))`.
    pub max_allowed_ratio_x100: u64,
}

impl fmt::Display for ScalingViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "scaling violation on {} (bound {}): N={} -> {} grew {} -> {} (observed ",
            self.axis, self.bound, self.n_small, self.n_large, self.small_val, self.large_val
        )?;
        if self.small_val == 0 && self.large_val > 0 {
            f.write_str("inf")?;
        } else {
            write!(
                f,
                "{}.{:02}x",
                self.observed_ratio_x100 / 100,
                self.observed_ratio_x100 % 100
            )?;
        }
        write!(
            f,
            " > max allowed {}.{:02}x)",
            self.max_allowed_ratio_x100 / 100,
            self.max_allowed_ratio_x100 % 100
        )
    }
}

impl Error for ScalingViolation {}

/// Fixed-point `max(1.0, log2(n))` in Q16 (`1.0 == 1 << 16`), computed with pure integer
/// shift-and-square arithmetic.
///
/// Exact for every power of two; accurate within `2^-16` for all non-powers of two; monotonically
/// non-decreasing for all `n >= 1`. Clamps `n == 1` to `1.0` so `OLogN` and `ONLogN` never divide
/// by zero.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
const fn log2_q16_clamped(n: usize) -> u128 {
    if n <= 2 {
        return 1u128 << 16;
    }
    let n_u128 = n as u128;
    let int_part = n.ilog2() as u128;
    // Normalize mantissa into [2^60, 2^61), representing [1.0, 2.0) in Q60.
    // Since m < 2^61, m * m < 2^122 < 2^128 so squaring never overflows u128.
    let mut m: u128 = if int_part <= 60 {
        n_u128 << (60 - int_part)
    } else {
        n_u128 >> (int_part - 60)
    };
    let mut frac: u128 = 0;
    let mut bit: u32 = 16;
    while bit > 0 {
        bit -= 1;
        m = (m * m) >> 60;
        if m >= (1u128 << 61) {
            frac |= 1u128 << bit;
            m >>= 1;
        }
    }
    (int_part << 16) | frac
}

/// Exact 128x128 -> 256-bit unsigned product `(hi, lo)`.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
const fn mul_wide_u128(a: u128, b: u128) -> (u128, u128) {
    let a_lo = a as u64 as u128;
    let a_hi = a >> 64;
    let b_lo = b as u64 as u128;
    let b_hi = b >> 64;

    let p0 = a_lo * b_lo;
    let p1 = a_lo * b_hi;
    let p2 = a_hi * b_lo;
    let p3 = a_hi * b_hi;

    let mid = (p0 >> 64) + (p1 as u64 as u128) + (p2 as u64 as u128);
    let lo = (p0 as u64 as u128) | (mid << 64);
    let hi = p3 + (p1 >> 64) + (p2 >> 64) + (mid >> 64);
    (hi, lo)
}

/// Exact rational `(num, den)` of the theoretical growth factor `f(n_large) / f(n_small)`.
/// Both `num` and `den` are strictly positive for `n_small >= 1` and `n_large > n_small`.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
const fn theoretical_growth(bound: ScalingBound, n_small: usize, n_large: usize) -> (u128, u128) {
    match bound {
        ScalingBound::O1 => (1, 1),
        ScalingBound::OLogN => (log2_q16_clamped(n_large), log2_q16_clamped(n_small)),
        ScalingBound::ON => (n_large as u128, n_small as u128),
        ScalingBound::ONLogN => (
            (n_large as u128) * log2_q16_clamped(n_large),
            (n_small as u128) * log2_q16_clamped(n_small),
        ),
        ScalingBound::ON2 => {
            let s = n_small as u128;
            let l = n_large as u128;
            (l * l, s * s)
        }
    }
}

/// Floor of `num * (100 + slack_pct) / den`, clamped to `u64::MAX`.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
const fn floor_max_ratio_x100(num: u128, den: u128, slack_pct: u32) -> u64 {
    let scale = 100u128 + slack_pct as u128;
    let ratio = match num.checked_mul(scale) {
        Some(prod) => prod / den,
        None => {
            // Avoid overflow when `num` is near u128::MAX (e.g. ON2 on huge usize).
            let q = num / den;
            let r = num % den;
            let main = q.saturating_mul(scale);
            let rem_part = match r.checked_mul(scale) {
                Some(p) => p / den,
                None => (r / den).saturating_mul(scale),
            };
            main.saturating_add(rem_part)
        }
    };
    if ratio > u64::MAX as u128 { u64::MAX } else { ratio as u64 }
}

/// Check one measurement axis against `bound` using exact integer ratio arithmetic.
///
/// Returns `Ok(())` when `large / small <= theoretical_growth(bound, n_small, n_large) * (100 + slack_pct) / 100`,
/// or `Err(ScalingViolation)` with exact observed and allowed ratios.
///
/// Zero-to-zero growth (`small == 0 && large == 0`) always passes; zero-to-nonzero growth
/// (`small == 0 && large > 0`) always fails with `observed_ratio_x100 == u64::MAX`.
///
/// # Panics
///
/// Panics if `n_small == 0` or `n_large <= n_small`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn check_axis(
    axis: Axis,
    bound: ScalingBound,
    n_small: usize,
    n_large: usize,
    small: u64,
    large: u64,
    slack_pct: u32,
) -> Result<(), ScalingViolation> {
    assert!(
        n_small >= 1 && n_large > n_small,
        "blaze-sim scaling: expected 1 <= n_small < n_large, got n_small={n_small}, n_large={n_large}"
    );
    if small == 0 {
        if large == 0 {
            return Ok(());
        }
        let (num, den) = theoretical_growth(bound, n_small, n_large);
        return Err(ScalingViolation {
            axis,
            bound,
            n_small,
            n_large,
            small_val: small,
            large_val: large,
            observed_ratio_x100: u64::MAX,
            max_allowed_ratio_x100: floor_max_ratio_x100(num, den, slack_pct),
        });
    }

    let (num, den) = theoretical_growth(bound, n_small, n_large);
    let scale = 100u128 + u128::from(slack_pct);

    // Exact inequality: large / small <= (num / den) * (scale / 100)
    //               <=> (large * 100) * den <= (small * scale) * num
    let lhs = mul_wide_u128(u128::from(large) * 100, den);
    let rhs = mul_wide_u128(u128::from(small) * scale, num);
    if lhs <= rhs {
        return Ok(());
    }

    let observed = (u128::from(large) * 100).div_ceil(u128::from(small));
    let observed_ratio_x100 = u64::try_from(observed).unwrap_or(u64::MAX);
    let max_allowed_ratio_x100 = floor_max_ratio_x100(num, den, slack_pct);

    Err(ScalingViolation {
        axis,
        bound,
        n_small,
        n_large,
        small_val: small,
        large_val: large,
        observed_ratio_x100,
        max_allowed_ratio_x100,
    })
}

/// Pure integer scaling check for a single counter (`Axis::Time`).
///
/// Equivalent to [`check_axis`] with [`Axis::Time`].
///
/// # Panics
///
/// Panics if `n_small == 0` or `n_large <= n_small`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn check_scaling(
    bound: ScalingBound,
    n_small: usize,
    n_large: usize,
    small: u64,
    large: u64,
    slack_pct: u32,
) -> Result<(), ScalingViolation> {
    check_axis(Axis::Time, bound, n_small, n_large, small, large, slack_pct)
}

/// Check both `steps` (`Axis::Time`) and `bytes` (`Axis::Space`) of two [`WorkSample`]s against
/// their independent bounds under `cfg`.
///
/// # Panics
///
/// Panics if `cfg` fails [`ScalingCheck::validate`].
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn check_work_scaling(
    time_bound: ScalingBound,
    space_bound: ScalingBound,
    cfg: ScalingCheck,
    small: WorkSample,
    large: WorkSample,
) -> Result<(), ScalingViolation> {
    cfg.validate();
    check_axis(
        Axis::Time,
        time_bound,
        cfg.n_small,
        cfg.n_large,
        small.steps,
        large.steps,
        cfg.slack_pct,
    )?;
    check_axis(
        Axis::Space,
        space_bound,
        cfg.n_small,
        cfg.n_large,
        small.bytes,
        large.bytes,
        cfg.slack_pct,
    )
}

/// Run `measure` at `cfg.n_small` and `cfg.n_large`, and check both `steps` (`time_bound`) and
/// `bytes` (`space_bound`).
///
/// Returns `Err(ScalingViolation)` instead of panicking so planted-bug controls can assert the
/// exact violation axis and bound without catching unwinds.
///
/// # Panics
///
/// Panics if `cfg` fails [`ScalingCheck::validate`].
pub fn verify_scaling<F>(
    time_bound: ScalingBound,
    space_bound: ScalingBound,
    cfg: ScalingCheck,
    mut measure: F,
) -> Result<(), ScalingViolation>
where
    F: FnMut(usize) -> WorkSample,
{
    cfg.validate();
    let small = measure(cfg.n_small);
    let large = measure(cfg.n_large);
    check_work_scaling(time_bound, space_bound, cfg, small, large)
}

/// Run `measure` at `cfg.n_small` and `cfg.n_large`, asserting that `steps` satisfies `time_bound`
/// and `bytes` satisfies `space_bound`.
///
/// # Panics
///
/// Panics with a formatted [`ScalingViolation`] if either axis exceeds its bound, or if `cfg`
/// fails [`ScalingCheck::validate`].
pub fn assert_scaling<F>(
    time_bound: ScalingBound,
    space_bound: ScalingBound,
    cfg: ScalingCheck,
    measure: F,
) where
    F: FnMut(usize) -> WorkSample,
{
    if let Err(violation) = verify_scaling(time_bound, space_bound, cfg, measure) {
        panic!("{violation}");
    }
}
