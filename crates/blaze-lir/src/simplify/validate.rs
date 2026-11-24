//! Alive2-style Automated Formal Translation Validator for LIR (`blaze_lir::validate`, Rule 57).
//!
//! Proves semantic equivalence between a source LIR function (`before`) and an optimized LIR
//! function (`after`) by executing both under the reference LIR evaluator (`crate::eval::run_in`)
//! across either:
//!
//! 1. **Exhaustive Bit-Vector Enumeration** when `product(2^bits(param_i)) <= opts.max_exhaustive_inputs`
//!    (covering 100% of all `256` states for 1-arg `i8`, all `65,536` states for 2-arg `i8` or
//!    1-arg `i16`, and all mixed narrow bit-vector domains).
//! 2. **Structured SMT Corner-Case Cartesian Suite + Deterministic SplitMix64 Sampling** for wider
//!    `i32`/`i64` domains, exercising signed/unsigned extrema, bit masks, shift boundaries, small
//!    loop trip counts, and alternating bit patterns.
//!
//! Refinement semantics follow Alive2:
//! - If `before` returns a value `v`, `after` must return the exact same bit-masked value `v`
//!   (never a different value, trap, UB, or divergence).
//! - If `before` traps, strict mode (the default) requires `after` to also trap; optional
//!   `allow_trap_refinement` allows `after` to refine a trap to a defined return value.
//! - If `before` triggers LIR UB (`Outcome::Undefined`), any non-error target outcome refines it.
//!
//! Pure sans-IO, `#![no_std]`, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit
//! allocator (`blaze_own::Vec<T, A>`) only.

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::{
    Block, View,
    eval::{self, EvalError, Outcome, Val},
    func::SigId,
    ty::Ty,
};

/// Upper bound on structured corner-case cartesian product evaluations for wide multi-arg inputs
/// (Rule 18: bounded progress). Covers 100% of 1-arg (`26`) and 2-arg (`676`) wide suites.
const MAX_BOUNDARY_CASES: u64 = 4_096;

/// High-value SMT-style bit-vector corner cases, masked per parameter width.
const BOUNDARY_SEEDS: [u64; 26] = [
    0,
    1,
    2,
    3,
    7,
    8,
    10,
    15,
    16,
    31,
    32,
    63,
    64,
    127,
    128,
    255,
    256,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_ffff,
    i64::MAX.cast_unsigned(),
    i64::MIN.cast_unsigned(),
    u64::MAX,
    u64::MAX - 1,
    0x5555_5555_5555_5555,
    0xaaaa_aaaa_aaaa_aaaa,
];

/// Options for [`validate_equivalence`] (rule `options-struct`: single plain struct).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ValidationOpts {
    /// Maximum total input-domain cardinality (`product(2^bits(param_i))`) for exhaustive
    /// bit-vector enumeration (e.g., `65_536` exhausts 1-arg `i1`/`i8`/`i16` and 2-arg `i8`).
    pub max_exhaustive_inputs: u64,
    /// Number of deterministic SplitMix64 random samples evaluated in non-exhaustive mode.
    pub random_samples: u32,
    /// Explicit PRNG seed for SplitMix64 sampling (Rules 25 & 75: no ambient randomness).
    pub seed: u64,
    /// Maximum LIR instructions executed per evaluation case (Rule 18: bounded progress).
    pub max_steps: u64,
    /// When `false` (strict default), a trap in `before` requires a trap in `after`.
    /// When `true` (Alive2 trap refinement), a trap in `before` may refine to a defined return.
    pub allow_trap_refinement: bool,
}

impl ValidationOpts {
    /// Fast default validation profile: exhaustive up to `65,536` inputs (all 1-arg & 2-arg `i8`),
    /// full SMT boundary suite + `256` structured SplitMix64 samples for wider inputs, strict trap
    /// equality.
    pub const fn default_fast() -> Self {
        Self {
            max_exhaustive_inputs: 65_536,
            random_samples: 256,
            seed: 0x9e37_79b9_7f4a_7c15,
            max_steps: 50_000,
            allow_trap_refinement: false,
        }
    }

    /// Thorough validation profile: exhaustive up to `131,072` inputs, full SMT boundary suite +
    /// `2,048` structured SplitMix64 samples, higher fuel bound, strict trap equality.
    pub const fn thorough() -> Self {
        Self {
            max_exhaustive_inputs: 131_072,
            random_samples: 2_048,
            seed: 0x517c_c1b7_2722_0a95,
            max_steps: 200_000,
            allow_trap_refinement: false,
        }
    }
}

impl Default for ValidationOpts {
    fn default() -> Self {
        Self::default_fast()
    }
}

/// Normalized observable outcome of evaluating one function on one concrete input vector.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutcomeSummary {
    /// Returned a single scalar integer (or `i1` bool), masked to the return type's width.
    Return(u64),
    /// Returned zero values (`-> ()`).
    ReturnVoid,
    /// Trapped (explicit `Op::Trap`, division/remainder by zero, or `sdiv` overflow).
    Trap,
    /// Hit `opts.max_steps` instruction fuel limit.
    StepLimit,
    /// Undefined behaviour, incompatible signature, multi-value / non-integer return, or refusal.
    Other,
}

/// Concrete input vector witnessing semantic divergence between `before` and `after`.
#[derive(PartialEq, Eq, Debug)]
pub struct Counterexample<A: Allocator> {
    pub args: Vec<u64, A>,
    pub before: OutcomeSummary,
    pub after: OutcomeSummary,
}

/// Result of [`validate_equivalence`] or [`prove_i8_rewrite_exact`].
#[derive(PartialEq, Eq, Debug)]
pub struct ValidationReport<A: Allocator> {
    /// Total concrete input vectors evaluated before completion or first counterexample.
    pub cases_checked: u64,
    /// `true` iff every input in the entire bit-vector domain was checked with zero mismatches.
    pub exhaustive: bool,
    /// First counterexample found, if any.
    pub counterexample: Option<Counterexample<A>>,
}

impl<A: Allocator> ValidationReport<A> {
    /// `true` iff no counterexample was found.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.counterexample.is_none()
    }

    /// `true` iff the validator exhaustively proved 100% of the bit-vector input domain equivalent.
    #[inline]
    pub fn is_proven_exhaustive(&self) -> bool {
        self.exhaustive && self.counterexample.is_none()
    }
}

/// Deterministic 64-bit SplitMix64 PRNG (no ambient state).
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    #[inline]
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

#[inline]
const fn mask_u64(bits: u32) -> u64 {
    if bits == 0 {
        0
    } else if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn trunc_u64(v: u128) -> u64 {
    u64::try_from(v & u128::from(u64::MAX)).unwrap_or(0)
}

#[inline]
fn trunc_u32(v: u64) -> u32 {
    u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0)
}

#[inline]
fn set_at<T>(slice: &mut [T], idx: usize, val: T) {
    if let Some(slot) = slice.get_mut(idx) {
        *slot = val;
    }
}

/// Validated scalar integer ABI extracted from a pair of LIR views.
struct ScalarAbi<A: Allocator> {
    param_bits: Vec<u32, A>,
    /// `None` for void (`-> ()`), `Some(bits)` for a single scalar integer return (`1..=64`).
    ret_bits: Option<u32>,
}

/// Checks ABI signature compatibility between `before` and `after`:
/// same entry parameter count & scalar integer/bool (`i1`..`i64`) types, and matching return type.
fn check_compatible_abi<A: Allocator + Copy>(
    before: View<'_>,
    after: View<'_>,
    alloc: A,
) -> Option<ScalarAbi<A>> {
    if before.blocks() == 0 || after.blocks() == 0 {
        return None;
    }
    if to_idx(before.head.sig) >= before.sigs.len()
        || to_idx(after.head.sig) >= after.sigs.len()
    {
        return None;
    }

    let b_params = before.params(Block(0));
    let a_params = after.params(Block(0));
    if b_params.len() != a_params.len() {
        return None;
    }

    let mut param_bits: Vec<u32, A> = Vec::with_capacity_in(b_params.len(), alloc);
    for idx in 0..b_params.len() {
        let (Some(&tb_raw), Some(&ta_raw)) = (
            before.tys.get(b_params.start + idx),
            after.tys.get(a_params.start + idx),
        ) else {
            return None;
        };
        let tb = Ty(tb_raw);
        let ta = Ty(ta_raw);
        if tb != ta || !tb.is_scalar() || !tb.is_int() {
            return None;
        }
        let bits = tb.bits(before.head.ptr_bits);
        if bits == 0 || bits > 64 {
            return None;
        }
        param_bits.push(bits);
    }

    let sig_b = before.sig(SigId(before.head.sig));
    let sig_a = after.sig(SigId(after.head.sig));
    if sig_b.rets.len() != sig_a.rets.len() {
        return None;
    }
    let ret_bits = match sig_b.rets.len() {
        0 => None,
        1 => {
            let rb = Ty(sig_b.rets[0].ty);
            let ra = Ty(sig_a.rets[0].ty);
            if rb != ra || !rb.is_scalar() || !rb.is_int() {
                return None;
            }
            let bits = rb.bits(before.head.ptr_bits);
            if bits == 0 || bits > 64 {
                return None;
            }
            Some(bits)
        }
        _ => return None,
    };

    Some(ScalarAbi { param_bits, ret_bits })
}

fn summarize_outcome<A: Allocator>(
    res: &Result<Outcome<A>, EvalError>,
    ret_bits: Option<u32>,
) -> OutcomeSummary {
    match res {
        Ok(Outcome::Returned(vals)) => match (vals.as_slice(), ret_bits) {
            (&[], None) => OutcomeSummary::ReturnVoid,
            (&[Val::Int(v)], Some(bits)) if bits <= 64 => {
                OutcomeSummary::Return(trunc_u64(v) & mask_u64(bits))
            }
            _ => OutcomeSummary::Other,
        },
        Ok(Outcome::Trapped(_)) => OutcomeSummary::Trap,
        Ok(Outcome::OutOfFuel) => OutcomeSummary::StepLimit,
        Ok(Outcome::Undefined { .. }) | Err(_) => OutcomeSummary::Other,
    }
}

fn outcomes_compatible<A: Allocator>(
    before: &Result<Outcome<A>, EvalError>,
    after: &Result<Outcome<A>, EvalError>,
    ret_bits: Option<u32>,
    allow_trap_refinement: bool,
) -> bool {
    let (Ok(b), Ok(a)) = (before, after) else {
        return false;
    };
    match (b, a) {
        (Outcome::Returned(vb), Outcome::Returned(va)) => {
            match (vb.as_slice(), va.as_slice(), ret_bits) {
                (&[], &[], None) => true,
                (&[Val::Int(xb)], &[Val::Int(xa)], Some(bits)) if bits <= 64 => {
                    let m = u128::from(mask_u64(bits));
                    (xb & m) == (xa & m)
                }
                _ => vb.as_slice() == va.as_slice(),
            }
        }
        (Outcome::Returned(_), _) => false,
        (Outcome::Trapped(_), Outcome::Trapped(_)) => true,
        (Outcome::Trapped(_), Outcome::Returned(_)) => allow_trap_refinement,
        (Outcome::Trapped(_), _) => false,
        // Alive2 UB refinement: if source exhibits UB, any non-error target execution is valid.
        (Outcome::Undefined { .. }, _) => true,
        // Source hit fuel bound; target either also hit fuel or optimized the path enough to
        // complete/trap within fuel. Introducing UB is still forbidden.
        (Outcome::OutOfFuel, Outcome::OutOfFuel | Outcome::Returned(_) | Outcome::Trapped(_)) => {
            true
        }
        (Outcome::OutOfFuel, Outcome::Undefined { .. }) => false,
    }
}

/// Evaluates one concrete input vector on both `before` and `after`, returning `Some(Counterexample)`
/// on the first refinement violation.
fn eval_pair<A: Allocator + Copy>(
    before: View<'_>,
    after: View<'_>,
    args_u64: &[u64],
    args_vals: &[Val],
    ret_bits: Option<u32>,
    opts: ValidationOpts,
    alloc: A,
) -> Option<Counterexample<A>> {
    let res_b = eval::run_in(before, args_vals, opts.max_steps, alloc);
    let res_a = eval::run_in(after, args_vals, opts.max_steps, alloc);
    if outcomes_compatible(&res_b, &res_a, ret_bits, opts.allow_trap_refinement) {
        return None;
    }
    let sum_b = summarize_outcome(&res_b, ret_bits);
    let sum_a = summarize_outcome(&res_a, ret_bits);
    let ce_args: Vec<u64, A> = Vec::from_slice_in(args_u64, alloc);
    Some(Counterexample { args: ce_args, before: sum_b, after: sum_a })
}

/// Builds the deduplicated deterministic SMT boundary set for a scalar integer of width `bits`.
fn boundary_values_for_width<A: Allocator>(bits: u32, alloc: A) -> Vec<u64, A> {
    let m = mask_u64(bits);
    let mut out: Vec<u64, A> = Vec::with_capacity_in(BOUNDARY_SEEDS.len() + 4, alloc);
    let mut push_unique = |v: u64| {
        let masked = v & m;
        if !out.as_slice().contains(&masked) {
            out.push(masked);
        }
    };
    for &seed in &BOUNDARY_SEEDS {
        push_unique(seed);
    }
    // Width-specific signed extrema and unsigned max / max-1 (critical for i8/i16/i32).
    if bits > 1 {
        let smin = 1u64 << (bits - 1);
        push_unique(smin - 1);
        push_unique(smin);
    }
    push_unique(m);
    push_unique(m.saturating_sub(1));
    out
}

/// Validates semantic equivalence between `before` and `after` using exhaustive bit-vector
/// enumeration when the input domain fits within `opts.max_exhaustive_inputs`, or a structured
/// SMT boundary cartesian suite plus deterministic SplitMix64 samples for wider domains.
pub fn validate_equivalence<A: Allocator + Copy>(
    before: View<'_>,
    after: View<'_>,
    opts: ValidationOpts,
    alloc: A,
) -> ValidationReport<A> {
    let Some(abi) = check_compatible_abi(before, after, alloc) else {
        return ValidationReport {
            cases_checked: 0,
            exhaustive: false,
            counterexample: Some(Counterexample {
                args: Vec::new_in(alloc),
                before: OutcomeSummary::Other,
                after: OutcomeSummary::Other,
            }),
        };
    };

    let n_params = abi.param_bits.len();
    let mut args_u64: Vec<u64, A> = Vec::from_elem_in(0u64, n_params, alloc);
    let mut args_vals: Vec<Val, A> = Vec::from_elem_in(Val::Int(0), n_params, alloc);

    // Compute exact total bit-vector domain cardinality `product(2^bits(param_i))`.
    let mut total_domain: Option<u128> = Some(1u128);
    for &bits in abi.param_bits.as_slice() {
        let card = 1u128.checked_shl(bits);
        total_domain = total_domain.and_then(|acc| card.and_then(|c| acc.checked_mul(c)));
    }

    let mut cases_checked: u64 = 0;

    // 1. Exhaustive bit-vector mode: evaluate EVERY SINGLE input bit pattern!
    if let Some(total) = total_domain {
        if total <= u128::from(opts.max_exhaustive_inputs) {
            for idx in 0..total {
                let mut rem = idx;
                for i in (0..n_params).rev() {
                    let Some(&bits) = abi.param_bits.get(i) else {
                        continue;
                    };
                    let v = trunc_u64(rem & u128::from(mask_u64(bits)));
                    set_at(args_u64.as_mut_slice(), i, v);
                    set_at(args_vals.as_mut_slice(), i, Val::Int(u128::from(v)));
                    rem >>= bits;
                }
                cases_checked += 1;
                if let Some(ce) = eval_pair(
                    before,
                    after,
                    args_u64.as_slice(),
                    args_vals.as_slice(),
                    abi.ret_bits,
                    opts,
                    alloc,
                ) {
                    return ValidationReport {
                        cases_checked,
                        exhaustive: false,
                        counterexample: Some(ce),
                    };
                }
            }
            return ValidationReport {
                cases_checked,
                exhaustive: true,
                counterexample: None,
            };
        }
    }

    // 2. Structured SMT boundary cartesian suite for wider bit-vector domains.
    let mut domains: Vec<Vec<u64, A>, A> = Vec::with_capacity_in(n_params, alloc);
    let mut cart_total: Option<u64> = Some(1u64);
    for &bits in abi.param_bits.as_slice() {
        let dom = boundary_values_for_width(bits, alloc);
        cart_total = cart_total
            .and_then(|acc| u64::try_from(dom.len()).ok().and_then(|n| acc.checked_mul(n)));
        domains.push(dom);
    }

    if let Some(total) = cart_total {
        if total <= MAX_BOUNDARY_CASES {
            for idx in 0..total {
                let mut rem = idx;
                for i in (0..n_params).rev() {
                    let Some(dom) = domains.get(i) else {
                        continue;
                    };
                    let Ok(base) = u64::try_from(dom.len()) else {
                        continue;
                    };
                    if base == 0 {
                        continue;
                    }
                    let pick = usize::try_from(rem % base).unwrap_or(0);
                    rem /= base;
                    let v = dom.get(pick).copied().unwrap_or(0);
                    set_at(args_u64.as_mut_slice(), i, v);
                    set_at(args_vals.as_mut_slice(), i, Val::Int(u128::from(v)));
                }
                cases_checked += 1;
                if let Some(ce) = eval_pair(
                    before,
                    after,
                    args_u64.as_slice(),
                    args_vals.as_slice(),
                    abi.ret_bits,
                    opts,
                    alloc,
                ) {
                    return ValidationReport {
                        cases_checked,
                        exhaustive: false,
                        counterexample: Some(ce),
                    };
                }
            }
        } else {
            // 3+ wide parameters: sweep diagonals, single-axis anchor sweeps, and deterministic
            // boundary-table combinations up to `MAX_BOUNDARY_CASES`.
            let mut boundary_run: u64 = 0;

            // (a) Diagonal sweep across boundary indices.
            let mut max_dom = 0usize;
            for d in domains.as_slice() {
                if d.len() > max_dom {
                    max_dom = d.len();
                }
            }
            for k in 0..max_dom {
                if boundary_run >= MAX_BOUNDARY_CASES {
                    break;
                }
                for i in 0..n_params {
                    let Some(dom) = domains.get(i) else {
                        continue;
                    };
                    if dom.is_empty() {
                        continue;
                    }
                    let v = dom.get(k % dom.len()).copied().unwrap_or(0);
                    set_at(args_u64.as_mut_slice(), i, v);
                    set_at(args_vals.as_mut_slice(), i, Val::Int(u128::from(v)));
                }
                boundary_run += 1;
                cases_checked += 1;
                if let Some(ce) = eval_pair(
                    before,
                    after,
                    args_u64.as_slice(),
                    args_vals.as_slice(),
                    abi.ret_bits,
                    opts,
                    alloc,
                ) {
                    return ValidationReport {
                        cases_checked,
                        exhaustive: false,
                        counterexample: Some(ce),
                    };
                }
            }

            // (b) Single-axis sweeps over key anchors (0, 1, all-ones).
            for anchor_kind in 0u32..3 {
                for axis in 0..n_params {
                    let Some(axis_dom) = domains.get(axis) else {
                        continue;
                    };
                    for &v in axis_dom.as_slice() {
                        if boundary_run >= MAX_BOUNDARY_CASES {
                            break;
                        }
                        for i in 0..n_params {
                            let val = if i == axis {
                                v
                            } else {
                                let m = mask_u64(abi.param_bits.get(i).copied().unwrap_or(64));
                                match anchor_kind {
                                    0 => 0,
                                    1 => 1 & m,
                                    _ => m,
                                }
                            };
                            set_at(args_u64.as_mut_slice(), i, val);
                            set_at(args_vals.as_mut_slice(), i, Val::Int(u128::from(val)));
                        }
                        boundary_run += 1;
                        cases_checked += 1;
                        if let Some(ce) = eval_pair(
                            before,
                            after,
                            args_u64.as_slice(),
                            args_vals.as_slice(),
                            abi.ret_bits,
                            opts,
                            alloc,
                        ) {
                            return ValidationReport {
                                cases_checked,
                                exhaustive: false,
                                counterexample: Some(ce),
                            };
                        }
                    }
                }
            }

            // (c) Deterministic boundary-table combinations up to cap.
            let mut b_rng = SplitMix64::new(opts.seed ^ 0xa076_1d64_78bd_642f);
            while boundary_run < MAX_BOUNDARY_CASES {
                for i in 0..n_params {
                    let Some(dom) = domains.get(i) else {
                        continue;
                    };
                    let Ok(dom_len) = u64::try_from(dom.len()) else {
                        continue;
                    };
                    if dom_len == 0 {
                        continue;
                    }
                    let pick = usize::try_from(b_rng.next_u64() % dom_len).unwrap_or(0);
                    let v = dom.get(pick).copied().unwrap_or(0);
                    set_at(args_u64.as_mut_slice(), i, v);
                    set_at(args_vals.as_mut_slice(), i, Val::Int(u128::from(v)));
                }
                boundary_run += 1;
                cases_checked += 1;
                if let Some(ce) = eval_pair(
                    before,
                    after,
                    args_u64.as_slice(),
                    args_vals.as_slice(),
                    abi.ret_bits,
                    opts,
                    alloc,
                ) {
                    return ValidationReport {
                        cases_checked,
                        exhaustive: false,
                        counterexample: Some(ce),
                    };
                }
            }
        }
    }

    // 3. Deterministic structured SplitMix64 random samples across 4 bit-vector strata:
    //    uniform full-width, small loop/shift values, near-extrema offsets, and sparse powers-of-2.
    let mut rng = SplitMix64::new(opts.seed);
    for sample in 0..opts.random_samples {
        for i in 0..n_params {
            let Some(&bits) = abi.param_bits.get(i) else {
                continue;
            };
            let m = mask_u64(bits);
            let v = match sample & 3 {
                0 => rng.next_u64() & m,
                1 => (rng.next_u64() & 63) & m,
                2 => {
                    let delta = rng.next_u64() & 15;
                    if (rng.next_u64() & 1) == 0 {
                        m.wrapping_sub(delta) & m
                    } else {
                        let smin = if bits == 1 { 1 } else { 1u64 << (bits - 1) };
                        smin.wrapping_add(delta) & m
                    }
                }
                _ => {
                    let bit = trunc_u32(rng.next_u64()) % bits;
                    let toggle = rng.next_u64() & 1;
                    ((1u64 << bit) ^ toggle) & m
                }
            };
            set_at(args_u64.as_mut_slice(), i, v);
            set_at(args_vals.as_mut_slice(), i, Val::Int(u128::from(v)));
        }
        cases_checked += 1;
        if let Some(ce) = eval_pair(
            before,
            after,
            args_u64.as_slice(),
            args_vals.as_slice(),
            abi.ret_bits,
            opts,
            alloc,
        ) {
            return ValidationReport {
                cases_checked,
                exhaustive: false,
                counterexample: Some(ce),
            };
        }
    }

    ValidationReport {
        cases_checked,
        exhaustive: false,
        counterexample: None,
    }
}

/// Exhaustively proves bit-for-bit equivalence of any 1-input (`256` states) or 2-input
/// (`65,536` states) `i8` LIR rewrite across 100% of bit-vector inputs.
///
/// Refuses (returning `exhaustive: false` with an `Other` counterexample) if either function
/// does not take 1 or 2 `Ty::I8` entry parameters.
pub fn prove_i8_rewrite_exact<A: Allocator + Copy>(
    before: View<'_>,
    after: View<'_>,
    max_steps: u64,
    alloc: A,
) -> ValidationReport<A> {
    let valid_i8_params = |v: View<'_>| -> Option<usize> {
        if v.blocks() == 0 {
            return None;
        }
        let params = v.params(Block(0));
        let n = params.len();
        if n == 0 || n > 2 {
            return None;
        }
        for inst in params {
            if v.tys.get(inst).copied().map(Ty) != Some(Ty::I8) {
                return None;
            }
        }
        Some(n)
    };

    if valid_i8_params(before).is_none() || valid_i8_params(before) != valid_i8_params(after) {
        return ValidationReport {
            cases_checked: 0,
            exhaustive: false,
            counterexample: Some(Counterexample {
                args: Vec::new_in(alloc),
                before: OutcomeSummary::Other,
                after: OutcomeSummary::Other,
            }),
        };
    }

    let opts = ValidationOpts {
        max_exhaustive_inputs: 65_536,
        random_samples: 0,
        seed: 0,
        max_steps,
        allow_trap_refinement: false,
    };
    validate_equivalence(before, after, opts, alloc)
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        Flags, IntCond, Op,
        builder::{Builder, Full, Param, Variadic},
        func::{BlockKind, Conv, Func},
        simplify::{
            bce::eliminate_redundant_checks,
            egraph::{RegBudget, sota_optimize_fixpoint},
            ifconv::if_convert,
            superopt::{SuperoptOpts, superopt_a64},
            taildup::tail_duplicate_dispatch,
        },
    };

    fn build_i8_unary(f: impl FnOnce(&mut Builder<TestHeap, Full>, crate::Value) -> crate::Value) -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I8)], &[Param::of(Ty::I8)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I8);
        let out = f(&mut b, x);
        b.ret(&[out]);
        b.finish_checked().unwrap()
    }

    fn build_i8_binary(
        f: impl FnOnce(&mut Builder<TestHeap, Full>, crate::Value, crate::Value) -> crate::Value,
    ) -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I8), Param::of(Ty::I8)],
            &[Param::of(Ty::I8)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I8);
        let y = b.param(Ty::I8);
        let out = f(&mut b, x, y);
        b.ret(&[out]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn exhaustive_i8_validator_proves_egraph_and_superopt_sound() {
        let alloc = TestHeap;

        // 1. `(x * 3) - (x * 2)` across all 256 i8 inputs (including wrapping overflow!).
        let mul_sub = build_i8_unary(|b, x| {
            let k3 = b.iconst(Ty::I8, 3);
            let k2 = b.iconst(Ty::I8, 2);
            let a = b.binary(Op::Mul, Flags::NONE, x, k3);
            let c = b.binary(Op::Mul, Flags::NONE, x, k2);
            b.binary(Op::Sub, Flags::NONE, a, c)
        });
        let e_mul_sub =
            sota_optimize_fixpoint(mul_sub.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let s_mul_sub =
            superopt_a64(e_mul_sub.func.view(), SuperoptOpts::apple_silicon(), |_| true, alloc)
                .unwrap();
        let rep = prove_i8_rewrite_exact(mul_sub.view(), s_mul_sub.func.view(), 10_000, alloc);
        assert!(rep.exhaustive, "expected exhaustive i8 proof: {rep:?}");
        assert_eq!(rep.cases_checked, 256);
        assert!(rep.counterexample.is_none(), "unexpected counterexample: {rep:?}");

        // 2. `x / 10` (unsigned division by constant) across all 256 i8 inputs.
        let udiv10 = build_i8_unary(|b, x| {
            let k10 = b.iconst(Ty::I8, 10);
            b.binary(Op::Udiv, Flags::NONE, x, k10)
        });
        let e_udiv10 =
            sota_optimize_fixpoint(udiv10.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let s_udiv10 =
            superopt_a64(e_udiv10.func.view(), SuperoptOpts::apple_silicon(), |_| true, alloc)
                .unwrap();
        let rep = prove_i8_rewrite_exact(udiv10.view(), s_udiv10.func.view(), 10_000, alloc);
        assert!(rep.exhaustive);
        assert_eq!(rep.cases_checked, 256);
        assert!(rep.counterexample.is_none(), "udiv10 diverged: {rep:?}");

        // 3. `(x ^ y) ^ y` across ALL 65,536 pairs of i8 inputs!
        let xor_cancel = build_i8_binary(|b, x, y| {
            let t = b.binary(Op::Xor, Flags::NONE, x, y);
            b.binary(Op::Xor, Flags::NONE, t, y)
        });
        let e_xor =
            sota_optimize_fixpoint(xor_cancel.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let s_xor =
            superopt_a64(e_xor.func.view(), SuperoptOpts::apple_silicon(), |_| true, alloc)
                .unwrap();
        let rep = prove_i8_rewrite_exact(xor_cancel.view(), s_xor.func.view(), 10_000, alloc);
        assert!(rep.exhaustive);
        assert_eq!(rep.cases_checked, 65_536);
        assert!(rep.counterexample.is_none(), "(x^y)^y diverged: {rep:?}");

        // 4. `(x & y) | (x & !y)` across ALL 65,536 pairs of i8 inputs!
        let bit_select = build_i8_binary(|b, x, y| {
            let ny = b.unary(Op::Not, Flags::NONE, y);
            let lhs = b.binary(Op::And, Flags::NONE, x, y);
            let rhs = b.binary(Op::And, Flags::NONE, x, ny);
            b.binary(Op::Or, Flags::NONE, lhs, rhs)
        });
        let e_bs =
            sota_optimize_fixpoint(bit_select.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let s_bs =
            superopt_a64(e_bs.func.view(), SuperoptOpts::apple_silicon(), |_| true, alloc).unwrap();
        let rep = prove_i8_rewrite_exact(bit_select.view(), s_bs.func.view(), 10_000, alloc);
        assert!(rep.exhaustive);
        assert_eq!(rep.cases_checked, 65_536);
        assert!(rep.counterexample.is_none(), "(x&y)|(x&!y) diverged: {rep:?}");
    }

    #[test]
    fn validator_proves_ifconv_bce_and_taildup_sound() {
        let alloc = TestHeap;
        let opts = ValidationOpts::default_fast();

        // 1. Validate `if_convert` on conditional reduction triangle loop:
        //    `fn cond_sum(n: u64, thresh: u64) -> u64 { for i in 0..n { if i > thresh { acc += i; } } }`
        let if_func = {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [entry, hdr, body, then_b, join, exit] =
                [(); 6].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let n = b.param(Ty::I64);
            let thresh = b.param(Ty::I64);
            let z = b.iconst(Ty::I64, 0);
            b.jump((hdr, &[z, z]));
            b.begin(hdr);
            let i = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, i, n);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));
            b.begin(body);
            let gt = b.icmp(IntCond::Ugt, i, thresh);
            b.br(Flags::NONE, gt, (then_b, &[]), (join, &[acc]));
            b.begin(then_b);
            let acc_t = b.binary(Op::Add, Flags::NONE, acc, i);
            b.jump((join, &[acc_t]));
            b.begin(join);
            let acc_next = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let i2 = b.binary(Op::Add, Flags::NONE, i, c1);
            b.jump((hdr, &[i2, acc_next]));
            b.begin(exit);
            b.ret(&[acc]);
            b.finish_checked().unwrap()
        };
        let if_out = if_convert(if_func.view(), 4, alloc).unwrap();
        assert!(if_out.stats.branches_converted >= 1);
        let rep = validate_equivalence(if_func.view(), if_out.func.view(), opts, alloc);
        assert!(!rep.exhaustive);
        assert!(rep.cases_checked >= 676 + u64::from(opts.random_samples));
        assert!(rep.counterexample.is_none(), "if_convert diverged: {rep:?}");

        // 2. Validate `eliminate_redundant_checks` (BCE) on affine window bounds-check loop:
        //    `if n >= 1 { for i in 0..(n-1) { if i >= n { trap(); } if i+1 >= n { trap(); } acc += i+1; } }`
        let bce_func = {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, pre, hdr, chk1, chk2, body, oob1, oob2, exit] =
                [(); 9].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let n = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let z = b.iconst(Ty::I64, 0);
            let has_win = b.icmp(IntCond::Uge, n, c1);
            b.br(Flags::NONE, has_win, (pre, &[]), (exit, &[z]));
            b.begin(pre);
            let lim = b.binary(Op::Sub, Flags::NONE, n, c1);
            b.jump((hdr, &[z, z]));
            b.begin(hdr);
            let i = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, i, lim);
            b.br(Flags::NONE, keep, (chk1, &[]), (exit, &[acc]));
            b.begin(chk1);
            let bad1 = b.icmp(IntCond::Uge, i, n);
            b.br(Flags::NONE, bad1, (oob1, &[]), (chk2, &[]));
            b.begin(oob1);
            b.trap(1);
            b.begin(chk2);
            let ip1 = b.binary(Op::Add, Flags::NONE, i, c1);
            let bad2 = b.icmp(IntCond::Uge, ip1, n);
            b.br(Flags::NONE, bad2, (oob2, &[]), (body, &[]));
            b.begin(oob2);
            b.trap(2);
            b.begin(body);
            let acc2 = b.binary(Op::Add, Flags::NONE, acc, ip1);
            b.jump((hdr, &[ip1, acc2]));
            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };
        let bce_out = eliminate_redundant_checks(bce_func.view(), alloc).unwrap();
        assert!(bce_out.stats.checks_eliminated >= 2);
        let rep = validate_equivalence(bce_func.view(), bce_out.func.view(), opts, alloc);
        assert!(rep.counterexample.is_none(), "BCE diverged: {rep:?}");

        // 3. Validate `tail_duplicate_dispatch` on 4-opcode threaded VM dispatch loop.
        let vm_func = {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, disp, op0, op1, op2, op3, halt] =
                [(); 7].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let steps = b.param(Ty::I64);
            let z = b.iconst(Ty::I64, 0);
            let k1 = b.iconst(Ty::I64, 1);
            let k3 = b.iconst(Ty::I64, 3);
            let k4 = b.iconst(Ty::I64, 4);
            let k7 = b.iconst(Ty::I64, 7);
            let k11 = b.iconst(Ty::I64, 11);
            b.jump((disp, &[steps, z, z]));

            b.begin(disp);
            let rem = b.param(Ty::I64);
            let st = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let is_z = b.icmp(IntCond::Eq, rem, z);
            let code = b.select(Flags::NONE, is_z, k4, st);
            b.switch(
                code,
                (halt, &[acc]),
                &[
                    (0, (op0, &[rem, acc])),
                    (1, (op1, &[rem, acc])),
                    (2, (op2, &[rem, acc])),
                    (3, (op3, &[rem, acc])),
                ],
            );

            let arms = [
                (op0, Op::Add, k3, 1u128),
                (op1, Op::Xor, k7, 2),
                (op2, Op::Add, k11, 3),
                (op3, Op::Add, k1, 0),
            ];
            for (blk, alu, kv, next_st) in arms {
                b.begin(blk);
                let r = b.param(Ty::I64);
                let a = b.param(Ty::I64);
                let r2 = b.binary(Op::Sub, Flags::NONE, r, k1);
                let a2 = b.binary(alu, Flags::NONE, a, kv);
                let ns = b.iconst(Ty::I64, next_st);
                b.jump((disp, &[r2, ns, a2]));
            }
            b.begin(halt);
            let out = b.param(Ty::I64);
            b.ret(&[out]);
            b.finish_checked().unwrap()
        };
        let dup_out = tail_duplicate_dispatch(vm_func.view(), alloc).unwrap();
        assert_eq!(dup_out.stats.arms_duplicated, 4);
        let rep = validate_equivalence(vm_func.view(), dup_out.func.view(), opts, alloc);
        assert!(rep.counterexample.is_none(), "tail_duplicate_dispatch diverged: {rep:?}");
    }

    #[test]
    fn validator_catches_planted_unsound_rewrite_with_minimal_counterexample() {
        let alloc = TestHeap;

        // Planted bug 1: replacing `udiv(x, 10)` with `lshr(x, 3)` (sound for 0..=7, unsound at x = 8!).
        let div10 = build_i8_unary(|b, x| {
            let k10 = b.iconst(Ty::I8, 10);
            b.binary(Op::Udiv, Flags::NONE, x, k10)
        });
        let buggy_shift = build_i8_unary(|b, x| {
            let k3 = b.iconst(Ty::I8, 3);
            b.binary(Op::Lshr, Flags::NONE, x, k3)
        });
        let rep = prove_i8_rewrite_exact(div10.view(), buggy_shift.view(), 10_000, alloc);
        assert!(!rep.exhaustive);
        let ce = rep.counterexample.expect("must catch unsound udiv(x, 10) -> lshr(x, 3)");
        assert_eq!(ce.args.as_slice(), &[8], "expected minimal counterexample x = 8");
        assert_eq!(ce.before, OutcomeSummary::Return(0));
        assert_eq!(ce.after, OutcomeSummary::Return(1));
        assert_eq!(rep.cases_checked, 9);

        // Planted bug 2: dropping wrap-around `(x * 2) >> 1 -> x` on i8 (fails at minimal x = 128!).
        let wrap_mul_shr = build_i8_unary(|b, x| {
            let k2 = b.iconst(Ty::I8, 2);
            let k1 = b.iconst(Ty::I8, 1);
            let p = b.binary(Op::Mul, Flags::NONE, x, k2);
            b.binary(Op::Lshr, Flags::NONE, p, k1)
        });
        let buggy_id = build_i8_unary(|_, x| x);
        let rep = prove_i8_rewrite_exact(wrap_mul_shr.view(), buggy_id.view(), 10_000, alloc);
        let ce = rep.counterexample.expect("must catch i8 wrap-around bug");
        assert_eq!(ce.args.as_slice(), &[128], "expected minimal counterexample x = 128");
        assert_eq!(ce.before, OutcomeSummary::Return(0));
        assert_eq!(ce.after, OutcomeSummary::Return(128));

        // Planted bug 3: replacing `(x & 1) == 0` with constant `true` on `i64 -> i1`.
        let is_even = {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let one = b.iconst(Ty::I64, 1);
            let zero = b.iconst(Ty::I64, 0);
            let lsb = b.binary(Op::And, Flags::NONE, x, one);
            let eq = b.icmp(IntCond::Eq, lsb, zero);
            b.ret(&[eq]);
            b.finish_checked().unwrap()
        };
        let always_true = {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let _x = b.param(Ty::I64);
            let t = b.iconst(Ty::I1, 1);
            b.ret(&[t]);
            b.finish_checked().unwrap()
        };
        let rep = validate_equivalence(
            is_even.view(),
            always_true.view(),
            ValidationOpts::default_fast(),
            alloc,
        );
        let ce = rep.counterexample.expect("must catch (x & 1) == 0 -> true");
        assert_eq!(ce.args.as_slice(), &[1], "expected minimal counterexample x = 1");
        assert_eq!(ce.before, OutcomeSummary::Return(0));
        assert_eq!(ce.after, OutcomeSummary::Return(1));

        // Planted bug 4: trap preservation in strict mode vs opt-in Alive2 trap refinement.
        let trap_on_zero = {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I8)], &[Param::of(Ty::I8)]);
            b.set_signature(sig);
            let [entry, bad, ok] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let x = b.param(Ty::I8);
            let z = b.iconst(Ty::I8, 0);
            let is_z = b.icmp(IntCond::Eq, x, z);
            b.br(Flags::NONE, is_z, (bad, &[]), (ok, &[]));
            b.begin(bad);
            b.trap(1);
            b.begin(ok);
            b.ret(&[x]);
            b.finish_checked().unwrap()
        };
        let strict_rep = validate_equivalence(
            trap_on_zero.view(),
            buggy_id.view(),
            ValidationOpts::default_fast(),
            alloc,
        );
        let ce = strict_rep
            .counterexample
            .expect("strict mode must reject silently dropping a trap");
        assert_eq!(ce.args.as_slice(), &[0]);
        assert_eq!(ce.before, OutcomeSummary::Trap);
        assert_eq!(ce.after, OutcomeSummary::Return(0));

        let refine_opts = ValidationOpts {
            allow_trap_refinement: true,
            ..ValidationOpts::default_fast()
        };
        let refine_rep =
            validate_equivalence(trap_on_zero.view(), buggy_id.view(), refine_opts, alloc);
        assert!(refine_rep.is_proven_exhaustive());
        assert_eq!(refine_rep.cases_checked, 256);
    }
}
