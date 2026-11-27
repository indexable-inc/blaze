use super::{
    Axis, ScalingBound, ScalingCheck, ScalingViolation, WorkSample, assert_scaling, check_axis,
    check_scaling, log2_q16_clamped, verify_scaling,
};

/// Deterministic O(N) work + O(N) space workload, with an optional planted O(N^2) time bug.
fn linear_work(n: usize, planted_quadratic: bool) -> WorkSample {
    let mut steps = 0u64;
    if planted_quadratic {
        for _ in 0..n {
            for _ in 0..n {
                steps += 1;
            }
        }
    } else {
        steps += 12;
        for _ in 0..n {
            steps += 3;
        }
    }
    WorkSample::new(steps, (n as u64) * 16)
}

/// Linear work passes `O(N)` scaling.
#[test]
fn linear_passes_on() {
    let cfg = ScalingCheck::new(64, 256);
    assert_scaling(ScalingBound::ON, ScalingBound::ON, cfg, |n| linear_work(n, false));
}

/// Planted-bug control: an `O(N^2)` regression fails `O(N)` with a `Time` violation.
#[test]
fn the_oracle_catches_planted_quadratic_under_on() {
    let cfg = ScalingCheck::new(64, 256);
    let err = verify_scaling(ScalingBound::ON, ScalingBound::ON, cfg, |n| linear_work(n, true))
        .expect_err("planted O(N^2) must violate O(N)");
    assert_eq!(
        err,
        ScalingViolation {
            axis: Axis::Time,
            bound: ScalingBound::ON,
            n_small: 64,
            n_large: 256,
            small_val: 64 * 64,
            large_val: 256 * 256,
            observed_ratio_x100: 1600,
            max_allowed_ratio_x100: 600,
        }
    );
}

#[test]
#[should_panic(expected = "scaling violation on time (bound O(N))")]
fn assert_scaling_panics_with_detailed_violation() {
    let cfg = ScalingCheck::new(64, 256);
    assert_scaling(ScalingBound::ON, ScalingBound::ON, cfg, |n| linear_work(n, true));
}

/// Deterministic merge-sort-like step counter: `N * ceil(log2(N))` steps, `N * 8` bytes.
fn n_log_n_work(n: usize) -> WorkSample {
    let mut width = 1usize;
    let mut steps = 0u64;
    while width < n {
        steps += n as u64;
        width <<= 1;
    }
    WorkSample::new(steps, (n as u64) * 8)
}

/// An `O(N log N)` pass satisfies `ONLogN`.
#[test]
fn n_log_n_passes_onlogn() {
    let cfg = ScalingCheck::new(64, 16384);
    assert_scaling(ScalingBound::ONLogN, ScalingBound::ON, cfg, n_log_n_work);
}

/// Planted-bug control: `O(N log N)` fails `ON` at sufficient separation (`64 -> 16384`).
#[test]
fn the_oracle_catches_n_log_n_under_on() {
    let cfg = ScalingCheck::new(64, 16384);
    let err = verify_scaling(ScalingBound::ON, ScalingBound::ON, cfg, n_log_n_work)
        .expect_err("O(N log N) must violate O(N) at 64 -> 16384");
    assert_eq!(err.axis, Axis::Time);
    assert_eq!(err.bound, ScalingBound::ON);
    assert_eq!(err.n_small, 64);
    assert_eq!(err.n_large, 16384);
    // 16384 * 14 / (64 * 6) = 597.33x -> ceil = 59734; max allowed = 256 * 1.50 = 38400.
    assert_eq!(err.observed_ratio_x100, 59734);
    assert_eq!(err.max_allowed_ratio_x100, 38400);
}

/// Planted-bug control: `O(N^2)` fails `ONLogN`.
#[test]
fn the_oracle_catches_quadratic_under_onlogn() {
    let cfg = ScalingCheck::new(64, 256);
    let err = verify_scaling(ScalingBound::ONLogN, ScalingBound::ON, cfg, |n| {
        WorkSample::new((n as u64) * (n as u64), n as u64)
    })
    .expect_err("O(N^2) must violate O(N log N)");
    assert_eq!(err.axis, Axis::Time);
    assert_eq!(err.bound, ScalingBound::ONLogN);
    assert!(err.observed_ratio_x100 > err.max_allowed_ratio_x100);
}

/// Deterministic binary search over `0..n` counting exact probe steps, with a planted linear-scan
/// fallback bug.
fn search_work(n: usize, planted_linear_scan: bool) -> WorkSample {
    if planted_linear_scan {
        return WorkSample::steps_only(n as u64);
    }
    let mut lo = 0usize;
    let mut hi = n;
    let target = n.saturating_sub(1);
    let mut steps = 0u64;
    while lo < hi {
        steps += 1;
        let mid = lo + (hi - lo) / 2;
        if mid < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    WorkSample::steps_only(steps)
}

/// Binary search passes `OLogN` time and `O1` space.
#[test]
fn log_n_passes_ologn() {
    let cfg = ScalingCheck::new(64, 16384);
    assert_scaling(ScalingBound::OLogN, ScalingBound::O1, cfg, |n| search_work(n, false));
}

/// Planted-bug control: linear scan fails `OLogN` on `Axis::Time`.
#[test]
fn the_oracle_catches_linear_under_ologn() {
    let cfg = ScalingCheck::new(64, 16384);
    let err = verify_scaling(ScalingBound::OLogN, ScalingBound::O1, cfg, |n| {
        search_work(n, true)
    })
    .expect_err("O(N) must violate O(log N)");
    assert_eq!(err.axis, Axis::Time);
    assert_eq!(err.bound, ScalingBound::OLogN);
    // 16384 / 64 = 256.00x; max allowed = (14/6) * 1.50 = 3.50x.
    assert_eq!(err.observed_ratio_x100, 25600);
    assert_eq!(err.max_allowed_ratio_x100, 350);
}

/// Streaming pass: `O(N)` steps and `O(1)` space (fixed scratch buffer), with a planted `O(N)`
/// space allocation bug.
fn streaming_work(n: usize, planted_linear_alloc: bool) -> WorkSample {
    let steps = (n as u64) * 2 + 5;
    let bytes = if planted_linear_alloc { (n as u64) * 8 } else { 64 };
    WorkSample::new(steps, bytes)
}

/// An `O(1)`-space streaming pass satisfies `(ON, O1)` (both constant non-zero and zero bytes).
#[test]
fn streaming_passes_on_time_o1_space() {
    let cfg = ScalingCheck::new(64, 256);
    assert_scaling(ScalingBound::ON, ScalingBound::O1, cfg, |n| streaming_work(n, false));
    assert_scaling(ScalingBound::ON, ScalingBound::O1, cfg, |n| {
        WorkSample::new(n as u64, 0)
    });
}

/// Planted-bug control: an `O(N)`-space allocation fails `(ON, O1)` specifically on `Axis::Space`.
#[test]
fn the_oracle_catches_linear_space_under_o1() {
    let cfg = ScalingCheck::new(64, 256);
    let err = verify_scaling(ScalingBound::ON, ScalingBound::O1, cfg, |n| {
        streaming_work(n, true)
    })
    .expect_err("O(N) space must violate O(1) space");
    assert_eq!(
        err,
        ScalingViolation {
            axis: Axis::Space,
            bound: ScalingBound::O1,
            n_small: 64,
            n_large: 256,
            small_val: 64 * 8,
            large_val: 256 * 8,
            observed_ratio_x100: 400,
            max_allowed_ratio_x100: 150,
        }
    );
}

/// Planted-bug control: spilling from 0 heap bytes at `n_small` to non-zero bytes at `n_large`
/// violates `O(1)` space with infinite observed ratio.
#[test]
fn the_oracle_catches_zero_to_nonzero_spill() {
    let cfg = ScalingCheck::new(64, 256);
    let err = verify_scaling(ScalingBound::ON, ScalingBound::O1, cfg, |n| {
        let bytes = if n <= 64 { 0 } else { (n as u64) * 8 };
        WorkSample::new(n as u64, bytes)
    })
    .expect_err("0 -> >0 byte spill must violate O(1) space");
    assert_eq!(err.axis, Axis::Space);
    assert_eq!(err.bound, ScalingBound::O1);
    assert_eq!(err.observed_ratio_x100, u64::MAX);
    assert_eq!(err.max_allowed_ratio_x100, 150);
    let msg = format!("{err}");
    assert!(
        msg.contains("observed inf > max allowed 1.50x"),
        "unexpected display: {msg}"
    );
}

/// `O(N^2)` passes `ON2`, and planted `O(N^3)` fails `ON2`.
#[test]
fn quadratic_passes_on2_and_cubic_fails_on2() {
    let cfg = ScalingCheck::new(16, 64);
    assert_scaling(ScalingBound::ON2, ScalingBound::O1, cfg, |n| {
        WorkSample::steps_only((n as u64) * (n as u64))
    });
    let err = verify_scaling(ScalingBound::ON2, ScalingBound::O1, cfg, |n| {
        let n = n as u64;
        WorkSample::steps_only(n * n * n)
    })
    .expect_err("O(N^3) must violate O(N^2)");
    assert_eq!(err.axis, Axis::Time);
    assert_eq!(err.bound, ScalingBound::ON2);
    // 4^3 = 64.00x; max allowed = 4^2 * 1.50 = 24.00x.
    assert_eq!(err.observed_ratio_x100, 6400);
    assert_eq!(err.max_allowed_ratio_x100, 2400);
}

#[test]
fn exact_boundary_and_slack_math() {
    // N = 100 -> 400 under ON with 50% slack: exact max ratio = 6.00x.
    assert!(check_scaling(ScalingBound::ON, 100, 400, 1000, 6000, 50).is_ok());
    let err = check_scaling(ScalingBound::ON, 100, 400, 1000, 6001, 50)
        .expect_err("6001/1000 > 6.00x must fail");
    assert_eq!(err.observed_ratio_x100, 601);
    assert_eq!(err.max_allowed_ratio_x100, 600);
    assert!(err.observed_ratio_x100 > err.max_allowed_ratio_x100);

    // Tighter slack (20%) rejects 5.00x on 4x input (max 4.80x).
    let tight = ScalingCheck::new(100, 400).with_slack_pct(20);
    let err = verify_scaling(ScalingBound::ON, ScalingBound::O1, tight, |n| {
        let steps = if n == 100 { 1000 } else { 5000 };
        WorkSample::steps_only(steps)
    })
    .expect_err("5.00x must violate ON with 20% slack (max 4.80x)");
    assert_eq!(err.observed_ratio_x100, 500);
    assert_eq!(err.max_allowed_ratio_x100, 480);

    // Tight fractional ratio: OLogN for 128 -> 1024 (log2: 7 -> 10; 10/7 * 1.50 = 2.142857x).
    // 2142 / 1000 = 2.142 <= 2.142857 passes; 2143 / 1000 = 2.143 > 2.142857 fails with 215 > 214.
    assert!(check_axis(Axis::Time, ScalingBound::OLogN, 128, 1024, 1000, 2142, 50).is_ok());
    let err = check_axis(Axis::Time, ScalingBound::OLogN, 128, 1024, 1000, 2143, 50)
        .expect_err("2.143x must exceed 2.142857x");
    assert_eq!(err.observed_ratio_x100, 215);
    assert_eq!(err.max_allowed_ratio_x100, 214);
}

#[test]
fn non_power_of_two_log2_accuracy_and_monotonicity() {
    // Exact powers of two.
    assert_eq!(log2_q16_clamped(1), 1 << 16);
    assert_eq!(log2_q16_clamped(2), 1 << 16);
    assert_eq!(log2_q16_clamped(4), 2 << 16);
    assert_eq!(log2_q16_clamped(64), 6 << 16);
    assert_eq!(log2_q16_clamped(16384), 14 << 16);

    // Non-powers of two within 1 ULP of Q16 (1/65536).
    // log2(3) = 1.5849625... -> * 65536 = 103872.1...
    let l3 = log2_q16_clamped(3);
    assert!((103871..=103873).contains(&l3), "log2(3) Q16 = {l3}");
    // log2(100) = 6.643856... -> * 65536 = 435411.7...
    let l100 = log2_q16_clamped(100);
    assert!((435410..=435412).contains(&l100), "log2(100) Q16 = {l100}");
    // log2(1000) = 9.965784... -> * 65536 = 653117.6...
    let l1000 = log2_q16_clamped(1000);
    assert!((653116..=653118).contains(&l1000), "log2(1000) Q16 = {l1000}");

    // Monotonicity across 1..=1024.
    let mut prev = log2_q16_clamped(1);
    for n in 2..=1024 {
        let cur = log2_q16_clamped(n);
        assert!(cur >= prev, "log2 decreased at {n}: {cur} < {prev}");
        prev = cur;
    }

    // `n_small == 1` is well-defined on OLogN and ONLogN without divide-by-zero.
    assert!(check_scaling(ScalingBound::OLogN, 1, 16, 1, 4, 50).is_ok());
    assert!(check_scaling(ScalingBound::ONLogN, 1, 16, 1, 64, 50).is_ok());
}

#[test]
fn large_values_never_overflow() {
    // Huge work counts near u64::MAX under ON and ON2.
    assert!(check_scaling(ScalingBound::O1, 64, 256, u64::MAX, u64::MAX, 50).is_ok());
    assert!(
        check_scaling(ScalingBound::ON, 64, 256, u64::MAX / 4, u64::MAX, 50).is_ok()
    );
    // (2^32 / 1)^2 = 2^64 > u64::MAX: passes ON2 without overflow.
    assert!(check_scaling(ScalingBound::ON2, 1, 1 << 32, 1, u64::MAX, 50).is_ok());
    // (2^30 / 2^20)^2 * 1.5 = 1_572_864 << u64::MAX: fails ON2 cleanly without overflow.
    let err = check_scaling(ScalingBound::ON2, 1 << 20, 1 << 30, 1, u64::MAX, 50)
        .expect_err("u64::MAX / 1 must exceed (2^10)^2 * 1.5");
    assert_eq!(err.observed_ratio_x100, u64::MAX);
    assert_eq!(err.max_allowed_ratio_x100, 157_286_400);
    // Extreme usize on ON2 where n_large^2 * scale would overflow u128 if unguarded.
    let err = check_scaling(ScalingBound::ON2, 1, usize::MAX, 0, 1, 50)
        .expect_err("0 -> 1 must still report violation cleanly without overflow");
    assert_eq!(err.observed_ratio_x100, u64::MAX);
    assert_eq!(err.max_allowed_ratio_x100, u64::MAX);
}

#[test]
#[should_panic(expected = "`n_small` must be at least 1")]
fn scaling_check_rejects_zero_small() {
    let _ = ScalingCheck::new(0, 16);
}

#[test]
#[should_panic(expected = "`n_large` must be at least `4 * n_small`")]
fn scaling_check_rejects_insufficient_separation() {
    let _ = ScalingCheck::new(64, 255);
}

#[test]
fn violation_display_is_readable() {
    let v = ScalingViolation {
        axis: Axis::Time,
        bound: ScalingBound::ON,
        n_small: 64,
        n_large: 256,
        small_val: 100,
        large_val: 1600,
        observed_ratio_x100: 1600,
        max_allowed_ratio_x100: 600,
    };
    assert_eq!(
        format!("{v}"),
        "scaling violation on time (bound O(N)): N=64 -> 256 grew 100 -> 1600 (observed 16.00x > max allowed 6.00x)"
    );
}
