//! AArch64 release-mode optimization helpers for `TierName::Sota`:
//! - **Granlund–Montgomery magic-multiply unsigned division** (replaces 12-cycle `udiv` with
//!   3-cycle `umulh` + shift for arbitrary non-power-of-2 64-bit constant divisors)
//! - **Tail-Call Optimization (`TCO`) site detection** (`r = call f(args); return r` -> `b f`)
//! - **Dense switch jump-table planner** (O(1) PC-relative `ldrsw` + `br` dispatch).

use blaze_lir::{
    Block, Op, Ty, View,
    func::{Conv, ParamKind, SigId},
};
use blaze_own::{Alloc, Vec};

#[inline]
fn idx(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Plan for replacing 64-bit unsigned division `n / d` (`d >= 3`, non-power-of-2) with `umulh`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MagicUdiv64 {
    /// `d > u64::MAX / 2`: `n / d == u64::from(n >= d)` (single `cmp + cset hs`!).
    CompareGe(u64),
    /// Direct multiply-high: `umulh(n, magic) >> shift`.
    MulShift { magic: u64, shift: u32 },
    /// Overflowing-magic form: `q = umulh(n, magic); (((n - q) >> 1) + q) >> post_shift`.
    MulAddShift { magic: u64, post_shift: u32 },
}

/// Computes the exact Granlund–Montgomery magic multiplier for 64-bit unsigned division by `d`.
///
/// Returns `None` for `d == 0`, `d == 1`, or powers of 2 (which strength-reduce to `lsr`).
/// Exact for every `n` in `0..=u64::MAX`.
pub(crate) fn compute_magic_udiv64(d: u64) -> Option<MagicUdiv64> {
    if d <= 2 || d.is_power_of_two() {
        return None;
    }
    if d > (u64::MAX / 2) {
        return Some(MagicUdiv64::CompareGe(d));
    }
    let d128 = u128::from(d);
    let max_n = u128::from(u64::MAX);
    // Largest n <= 2^64 - 1 with n ≡ -1 (mod d): nc = floor(2^64 / d) * d - 1.
    let two64 = 1u128 << 64;
    let nc = two64 - (two64 % d128) - 1;

    // 1. Try direct `umulh(n, m) >> shift` (`m < 2^64`).
    for shift in 0..63u32 {
        let two_p = 1u128 << (64 + shift);
        let rem = two_p % d128;
        let delta = d128 - rem;
        let m = (two_p + delta) / d128;
        if m <= max_n && nc.checked_mul(delta).is_some_and(|p| p < two_p) {
            if let Ok(magic) = u64::try_from(m) {
                return Some(MagicUdiv64::MulShift { magic, shift });
            }
        }
    }

    // 2. Overflowing-magic form (`2^64 <= m < 2^65`, `shift >= 1`).
    for shift in 1..64u32 {
        let two_p = 1u128 << (64 + shift);
        let rem = two_p % d128;
        let delta = d128 - rem;
        let m = (two_p + delta) / d128;
        if m > max_n && m < (two64 << 1) && nc.checked_mul(delta).is_some_and(|p| p < two_p) {
            if let Ok(magic) = u64::try_from(m - two64) {
                return Some(MagicUdiv64::MulAddShift {
                    magic,
                    post_shift: shift - 1,
                });
            }
        }
    }
    None
}

/// Marks direct/indirect call instructions eligible for Tail-Call Optimization (`b` / `br` after
/// epilogue instead of `bl`/`blr` + epilogue + `ret`).
///
/// A call at `inst` is tail-callable when:
/// - The immediately following instruction in the same block is `Op::Return` returning the call's
///   exact result (or void for void calls).
/// - Callee signature has the same C ABI, no `sret`/`byval`, and matching return types/extensions.
/// - No stack slots are used in the function (guarantees no pointer to a local stack slot can
///   escape into the tail-called callee after the frame is torn down!).
/// - For direct `Op::Call`, `allow_direct_tco` is `true` (direct `Branch26` mode for export).
pub(crate) fn detect_tail_calls<'a>(
    view: &View<'_>,
    allow_direct_tco: bool,
    os: crate::Os,
    alloc: Alloc<'a>,
) -> Vec<bool, Alloc<'a>> {
    let len = view.len();
    let mut is_tail: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for inst in 0..len {
        is_tail.push(matches!(view.op(inst), Op::TailCall | Op::TailCallIndirect));
    }
    // Never implicitly tail-call if the function allocates local stack slots (a slot address might be passed).
    // Explicit `Op::TailCall`/`Op::TailCallIndirect` (RFC 3407 `become`) already dropped all locals and
    // stays marked true above.
    if !view.slots.is_empty() {
        return is_tail;
    }
    let func_sig_id = SigId(view.head.sig);
    let func_sig = view.sig(func_sig_id);
    if Conv::from_byte(func_sig.conv) != Some(Conv::C) {
        return is_tail;
    }
    let has_stacked = |sig: blaze_lir::func::Sig<'_>| -> bool {
        let mut regs = crate::pass::Regs::new(os);
        for p in sig.params {
            if ParamKind::from_byte(p.kind) == Some(ParamKind::Sret) {
                continue;
            }
            if regs.next(Ty(p.ty)).is_none() {
                return true;
            }
        }
        false
    };
    if func_sig.params.iter().any(|p| ParamKind::from_byte(p.kind) != Some(ParamKind::Value)) || has_stacked(func_sig) {
        return is_tail;
    }

    for b in 0..view.blocks() {
        let range = view.block(Block(u32_of(b)));
        if range.len() < 2 {
            continue;
        }
        let term = range.end - 1;
        let prev = term - 1;
        if view.op(term) != Op::Return {
            continue;
        }
        let op = view.op(prev);
        let eligible_op = (op == Op::Call && allow_direct_tco) || op == Op::CallIndirect;
        if !eligible_op {
            continue;
        }
        let Some(&call_at_raw) = view.data.get(prev) else {
            continue;
        };
        let call_at = idx(call_at_raw);
        let Some(&call_sig_raw) = view.extra.get(call_at + 1) else {
            continue;
        };
        let call_sig_id = SigId(call_sig_raw);
        let call_sig = view.sig(call_sig_id);
        if Conv::from_byte(call_sig.conv) != Some(Conv::C) {
            continue;
        }
        if call_sig.params.iter().any(|p| ParamKind::from_byte(p.kind) != Some(ParamKind::Value)) || has_stacked(call_sig) {
            continue;
        }
        // Return signatures must match identically (types, count, sign/zero ext).
        if call_sig.rets.len() != func_sig.rets.len() {
            continue;
        }
        let rets_match = call_sig.rets.iter().zip(func_sig.rets.iter()).all(|(a, b)| {
            a.ty == b.ty && a.kind == b.kind && a.ext == b.ext
        });
        if !rets_match {
            continue;
        }
        let Some(&ret_at_raw) = view.data.get(term) else {
            continue;
        };
        let ret_at = idx(ret_at_raw);
        let Some(&retc_raw) = view.extra.get(ret_at) else {
            continue;
        };
        let retc = idx(retc_raw);
        let returns_call = match retc {
            0 => view.tys.get(prev).copied().map(Ty) == Some(Ty::NONE),
            1 => view.extra.get(ret_at + 1).copied().map(idx) == Some(prev),
            _ => false,
        };
        if returns_call {
            if let Some(slot) = is_tail.get_mut(prev) {
                *slot = true;
            }
        }
    }
    is_tail
}

/// Checks if a switch terminator qualifies for a dense PC-relative jump table (`O(1)` dispatch).
///
/// Returns `Some((min_val, span))` where `span = (max_val - min_val) as usize + 1` when:
/// - `num_cases >= 4`
/// - `max_val - min_val <= 255` and `span <= num_cases * 2 + 4` (dense enough to be compact).
pub(crate) fn dense_switch_plan(view: &View<'_>, inst: usize) -> Option<(u64, usize)> {
    let at = idx(*view.data.get(inst)?);
    let scrutinee = *view.extra.get(at)?;
    let default_argc = idx(*view.extra.get(at + 2)?);
    let cases_at = at + 3 + default_argc;
    let num_cases = idx(*view.extra.get(cases_at)?);
    if num_cases < 4 {
        return None;
    }
    let width_words = idx(Ty(*view.tys.get(idx(scrutinee))?).const_words());
    if width_words == 0 || width_words > 2 {
        return None;
    }
    let mut pos = cases_at + 1;
    let mut min_v = u64::MAX;
    let mut max_v = 0u64;
    for _ in 0..num_cases {
        let mut case_val = 0u64;
        for w in (0..width_words).rev() {
            case_val = (case_val << 32) | u64::from(*view.extra.get(pos + w)?);
        }
        pos += width_words;
        let case_argc = idx(*view.extra.get(pos + 1)?);
        pos += 2 + case_argc;
        if case_val < min_v {
            min_v = case_val;
        }
        if case_val > max_v {
            max_v = case_val;
        }
    }
    let diff = max_v.checked_sub(min_v)?;
    if diff > 255 {
        return None;
    }
    let span = usize::try_from(diff).ok()? + 1;
    if span <= num_cases * 2 + 4 {
        Some((min_v, span))
    } else {
        None
    }
}

