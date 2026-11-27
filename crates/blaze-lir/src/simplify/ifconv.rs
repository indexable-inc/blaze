//! If-Conversion (`Br` / small pure `Switch` -> `Select`) + Conditional Reduction Canonicalization (`blaze_lir::ifconv`).
//!
//! Converts pure, side-effect-free diamond/triangle branches and small pure `Op::Switch`
//! dispatches into branchless `Op::Select` expressions (balanced depth-3 binary tree for dense
//! `0..m` switches) and folds the resulting single-predecessor `Jump` via `coalesce_cfg`:
//!
//! - Eliminates unpredictable branches and indirect jumps inside hot loops (turning ~15-cycle
//!   branch mispredicts into 1-cycle `csel`/`cmovcc` or SIMD `cmhi`+`and`/`bsl`).
//! - Canonicalizes conditional reductions:
//!   `Select(c, acc + x, acc)  ->  acc + Select(c, x, 0)` (and likewise for `Xor`, `Or`, `And`),
//!   exposing filtered Rust loops (`for &x in s { if x > t { acc += x; } }`) directly to
//!   4x-unrolled masked SIMD auto-vectorization!
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, coalesce_cfg, constant_of, dead, each, filled, incoming, reachable},
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Ty, View,
    arith::{compare, iconst_value, mask, sext, wide_value},
    op::{Field, Layout},
    verify,
};

/// Microarchitecture-aware cost model for branch vs. speculative `Op::Select` lowering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IfConvTarget {
    pub max_arm_insts: usize,
    pub max_arm_raw_insts: usize,
    pub max_diamond_insts: usize,
    /// Approximate branch misprediction penalty in cycles (Apple M5 ~14, Graviton 5 / Neoverse V3 ~12).
    pub mispredict_cycles: u16,
    /// Permille (`0..=1000`) threshold above which a branch is considered predictable
    /// (i.e. `taken_permille >= predictable_permille` or `taken_permille <= 1000 - predictable_permille`).
    /// Predictable guards stay branches (0-cycle TAGE prediction on M5 / Graviton 5);
    /// unpredictable branches (`15%..85%` taken) convert to branchless `Select`.
    pub predictable_permille: u16,
}

impl IfConvTarget {
    /// Apple M5 microarchitecture cost profile (~14-cycle branch misprediction penalty).
    pub const APPLE_M5: Self = Self {
        max_arm_insts: 12,
        max_arm_raw_insts: 22,
        max_diamond_insts: 14,
        mispredict_cycles: 14,
        predictable_permille: 900,
    };

    /// AWS Graviton 5 / Arm Neoverse V3 microarchitecture cost profile (~12-cycle branch misprediction penalty).
    pub const GRAVITON5: Self = Self {
        max_arm_insts: 12,
        max_arm_raw_insts: 22,
        max_diamond_insts: 14,
        mispredict_cycles: 12,
        predictable_permille: 900,
    };
}

impl Default for IfConvTarget {
    fn default() -> Self {
        Self::APPLE_M5
    }
}

/// Optional per-block taken probability in permille (`0..=1000`; `u16::MAX` = unknown/static).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct BranchProfile<'a> {
    pub taken_permille_by_block: &'a [u16],
}

/// Configuration options for [`if_convert_with_opts`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IfConvOpts<'a> {
    pub max_rounds: u32,
    pub target: IfConvTarget,
    pub profile: BranchProfile<'a>,
}

/// Telemetry returned by [`if_convert`] and [`if_convert_with_opts`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct IfConvStats {
    pub branches_converted: u32,
    pub reductions_canonicalized: u32,
    pub predictable_branches_preserved: u32,
}

/// Output of [`if_convert`] and [`if_convert_with_opts`].
#[derive(Debug)]
pub struct IfConvOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: IfConvStats,
}

const PERMILLE_SCALE: u16 = 1000;
const TRIVIAL_PREDICTABLE_OPS: usize = 2;
const HIGH_ENTROPY_MIN_PERMILLE: u16 = 250;
const HIGH_ENTROPY_MAX_PERMILLE: u16 = 750;
const HIGH_ENTROPY_ARM_BONUS: usize = 4;
const HIGH_ENTROPY_DIAMOND_BONUS: usize = 4;
const HIGH_ENTROPY_RAW_BONUS: usize = 6;
const MIN_SWITCH_CASES: usize = 2;
const MAX_SWITCH_CASES: usize = 8;
const MAX_SWITCH_ARMS: usize = MAX_SWITCH_CASES + 1;
const MAX_SWITCH_ARM_INSTS: usize = 4;
const MAX_SWITCH_TOTAL_INSTS: usize = 20;
const MAX_SWITCH_DIFF_PARAMS: usize = 2;

#[derive(Clone, Copy)]
struct BranchBudget {
    max_arm_insts: usize,
    max_arm_raw_insts: usize,
    max_diamond_insts: usize,
    is_predictable: bool,
}

#[inline]
fn branch_budget(target: IfConvTarget, taken_permille: u16) -> BranchBudget {
    if taken_permille > PERMILLE_SCALE {
        return BranchBudget {
            max_arm_insts: target.max_arm_insts,
            max_arm_raw_insts: target.max_arm_raw_insts,
            max_diamond_insts: target.max_diamond_insts,
            is_predictable: false,
        };
    }
    let low_threshold = PERMILLE_SCALE.saturating_sub(target.predictable_permille);
    let is_predictable = taken_permille >= target.predictable_permille || taken_permille <= low_threshold;
    if is_predictable {
        return BranchBudget {
            max_arm_insts: target.max_arm_insts,
            max_arm_raw_insts: target.max_arm_raw_insts,
            max_diamond_insts: target.max_diamond_insts,
            is_predictable: true,
        };
    }
    if (HIGH_ENTROPY_MIN_PERMILLE..=HIGH_ENTROPY_MAX_PERMILLE).contains(&taken_permille) {
        BranchBudget {
            max_arm_insts: target.max_arm_insts.saturating_add(HIGH_ENTROPY_ARM_BONUS),
            max_arm_raw_insts: target.max_arm_raw_insts.saturating_add(HIGH_ENTROPY_RAW_BONUS),
            max_diamond_insts: target.max_diamond_insts.saturating_add(HIGH_ENTROPY_DIAMOND_BONUS),
            is_predictable: false,
        }
    } else {
        BranchBudget {
            max_arm_insts: target.max_arm_insts,
            max_arm_raw_insts: target.max_arm_raw_insts,
            max_diamond_insts: target.max_diamond_insts,
            is_predictable: false,
        }
    }
}

enum FindOutcome {
    Matched {
        pat: IfPattern,
        predictable_preserved: u32,
    },
    Done {
        predictable_preserved: u32,
    },
}

struct AppliedPattern<A: Allocator> {
    func: Func<A>,
    reductions_canonicalized: u32,
    taken_by_block: Vec<u16, A>,
}

#[inline]
fn is_zero_cost_const(op: Op) -> bool {
    matches!(op, Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide)
}

#[inline]
fn is_speculable_pure(op: Op, flags: Flags) -> bool {
    if op.info().flags.meanings_only(flags).0 != 0 {
        return false;
    }
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Umulhi
            | Op::Smulhi
            | Op::UaddSat
            | Op::SaddSat
            | Op::UsubSat
            | Op::SsubSat
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotl
            | Op::Rotr
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::Icmp
            | Op::Fcmp
            | Op::Select
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Fpext
            | Op::Fptrunc
            | Op::UitoFp
            | Op::SitoFp
            | Op::Bitcast
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Fcopysign
            | Op::Fneg
            | Op::Fabs
            | Op::Freeze
            | Op::PtrAdd
            | Op::PtrAddr
            | Op::PtrWithAddr
            | Op::Expose
            | Op::FromExposed
    )
}

#[inline]
fn is_selectable_ty(ty: Ty) -> bool {
    ty != Ty::NONE && (ty.is_ptr() || (ty.is_scalar() && (ty.is_int() || ty.is_float())))
}

#[derive(Clone, Copy)]
struct SwitchCaseInfo {
    val: u64,
    join_args_at: usize,
}

#[derive(Clone, Copy)]
struct SwitchPattern {
    head: u32,
    scrut: u32,
    scrut_ty: Ty,
    join: u32,
    default_join_args_at: usize,
    case_count: u8,
    cases: [SwitchCaseInfo; MAX_SWITCH_CASES],
    arm_count: u8,
    arms: [u32; MAX_SWITCH_ARMS],
}

#[derive(Clone, Copy)]
enum IfPattern {
    /// `head` branches on `cond` to the same block `(join, t_args)` / `(join, e_args)`.
    DirectSelect {
        head: u32,
        cond: u32,
        join: u32,
    },
    /// `b` branches on `cond` to `(arm, [])` and `(join, direct_args)`; `arm` jumps to `(join, arm_args)`.
    /// `arm_is_then == true` means `arm` is the `then` branch; `false` means `arm` is the `else` branch.
    Triangle {
        head: u32,
        cond: u32,
        arm: u32,
        join: u32,
        arm_is_then: bool,
    },
    /// `b` branches on `cond` to `(t_arm, [])` and `(e_arm, [])`; both jump to `join`.
    Diamond {
        head: u32,
        cond: u32,
        t_arm: u32,
        e_arm: u32,
        join: u32,
    },
    /// `head` switches on scalar integer `scrut` across `2..=8` cases + default where every arm
    /// is a small pure block jumping to the same `join`.
    Switch(SwitchPattern),
}

#[inline]
fn int_cond_word(cc: IntCond) -> u32 {
    match cc {
        IntCond::Eq => 0,
        IntCond::Ne => 1,
        IntCond::Ult => 2,
        IntCond::Ule => 3,
        IntCond::Ugt => 4,
        IntCond::Uge => 5,
        IntCond::Slt => 6,
        IntCond::Sle => 7,
        IntCond::Sgt => 8,
        IntCond::Sge => 9,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EmitVal {
    /// Already emitted in the destination `Func` (instruction index).
    New(u32),
    /// Defined in `view` in a block laid out after `head`; resolved via `ValFixup::Extra`.
    Pending(u32),
}

#[inline]
fn resolve_val(val_map: &[u32], old_val: u32) -> EmitVal {
    let mapped = at_u32(val_map, to_idx(old_val));
    if mapped == NONE {
        EmitVal::Pending(old_val)
    } else {
        EmitVal::New(mapped)
    }
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn trunc_u32(v: u64) -> u32 {
    u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0)
}

#[inline]
fn at_u32(s: &[u32], i: usize) -> u32 { s.get(i).copied().unwrap_or(0) }
#[inline]
fn at_u16(s: &[u16], i: usize) -> u16 { s.get(i).copied().unwrap_or(0) }

#[inline]
fn at_u8(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

fn push_inst<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, data: u32) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(flags.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(0);
    func.locs.push(0);
    idx
}

fn push_iconst<A: Allocator>(func: &mut Func<A>, ty: Ty, val: u64) -> u32 {
    if ty.bits(64) <= 32 {
        push_inst(func, Op::Iconst, Flags::NONE, ty, trunc_u32(val))
    } else if let Ok(v32) = i32::try_from(val.cast_signed()) {
        push_inst(func, Op::Iconst, Flags::NONE, ty, v32.cast_unsigned())
    } else {
        let at = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, at)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, at)
}

fn push_select<A: Allocator>(func: &mut Func<A>, ty: Ty, cond: u32, t_val: u32, f_val: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(cond);
    func.extra.push(t_val);
    func.extra.push(f_val);
    push_inst(func, Op::Select, Flags::NONE, ty, at)
}

#[derive(Clone, Copy)]
enum ValFixup {
    Data { dst_inst: usize, old_val: u32 },
    Extra { dst_extra: usize, old_val: u32 },
}

fn push_mapped_val<A: Allocator>(
    val_map: &[u32],
    old_val: u32,
    fixups: &mut Vec<ValFixup, A>,
    func: &mut Func<A>,
) {
    let mapped = at_u32(val_map, to_idx(old_val));
    if mapped == NONE {
        let dst_extra = func.extra.len();
        fixups.push(ValFixup::Extra { dst_extra, old_val });
        func.extra.push(0);
    } else {
        func.extra.push(mapped);
    }
}

fn push_emit_val<A: Allocator>(
    val: EmitVal,
    fixups: &mut Vec<ValFixup, A>,
    func: &mut Func<A>,
) {
    match val {
        EmitVal::New(v) => func.extra.push(v),
        EmitVal::Pending(old_val) => {
            let dst_extra = func.extra.len();
            fixups.push(ValFixup::Extra { dst_extra, old_val });
            func.extra.push(0);
        }
    }
}

fn push_icmp_emit<A: Allocator>(
    cc: IntCond,
    lhs: EmitVal,
    rhs: EmitVal,
    fixups: &mut Vec<ValFixup, A>,
    func: &mut Func<A>,
) -> EmitVal {
    let at = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cc));
    push_emit_val(lhs, fixups, func);
    push_emit_val(rhs, fixups, func);
    EmitVal::New(push_inst(func, Op::Icmp, Flags::NONE, Ty::I1, at))
}

fn push_select_emit<A: Allocator>(
    ty: Ty,
    cond: EmitVal,
    t_val: EmitVal,
    f_val: EmitVal,
    fixups: &mut Vec<ValFixup, A>,
    func: &mut Func<A>,
) -> EmitVal {
    let at = to_u32(func.extra.len());
    push_emit_val(cond, fixups, func);
    push_emit_val(t_val, fixups, func);
    push_emit_val(f_val, fixups, func);
    EmitVal::New(push_inst(func, Op::Select, Flags::NONE, ty, at))
}

struct SwitchEmitCtx<'a, A: Allocator> {
    scrut: EmitVal,
    scrut_ty: Ty,
    cond_cache: [Option<EmitVal>; MAX_SWITCH_CASES],
    fixups: &'a mut Vec<ValFixup, A>,
    func: &'a mut Func<A>,
}

impl<A: Allocator> SwitchEmitCtx<'_, A> {
    /// Emits `Select(icmp(cc, scrut, imm), t_val, f_val)`, skipping when `t_val == f_val` and
    /// caching the comparison in `cond_cache[slot]` across differing join parameters.
    fn sel_cmp(
        &mut self,
        slot: usize,
        cc: IntCond,
        imm: u64,
        pty: Ty,
        t_val: EmitVal,
        f_val: EmitVal,
    ) -> EmitVal {
        if t_val == f_val {
            return t_val;
        }
        let cond = match self.cond_cache.get(slot).copied().flatten() {
            Some(existing) => existing,
            None => {
                let c = EmitVal::New(push_iconst(self.func, self.scrut_ty, imm));
                let cmp = push_icmp_emit(cc, self.scrut, c, self.fixups, self.func);
                set_at(&mut self.cond_cache, slot, Some(cmp));
                cmp
            }
        };
        push_select_emit(pty, cond, t_val, f_val, self.fixups, self.func)
    }
}

fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    fixups: &mut Vec<ValFixup, A>,
    func: &mut Func<A>,
) -> u32 {
    let op = view.op(inst);
    let op_raw = at_u8(view.ops, inst);
    let flags = at_u8(view.flags, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let inst_data = at_u32(view.data, inst);
    let out_idx = to_u32(func.ops.len());
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            let mapped = at_u32(val_map, to_idx(inst_data));
            if mapped == NONE {
                fixups.push(ValFixup::Data { dst_inst: to_idx(out_idx), old_val: inst_data });
                0
            } else {
                mapped
            }
        }
        Layout::Data(_) => inst_data,
        Layout::Extra(fields) => {
            let src = to_idx(inst_data);
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        push_mapped_val(val_map, at_u32(view.extra, p), fixups, func);
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            push_mapped_val(val_map, at_u32(view.extra, p + k), fixups, func);
                        }
                        p += n;
                    }
                    Field::Target => {
                        let b = at_u32(view.extra, p);
                        let n_u32 = at_u32(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(b);
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            push_mapped_val(val_map, at_u32(view.extra, p + k), fixups, func);
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let scrutinee = at_u32(view.extra, src);
                        let w = to_idx(Ty(at_u16(view.tys, to_idx(scrutinee))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at_u32(view.extra, p + k));
                            }
                            p += w;
                            let b = at_u32(view.extra, p);
                            let an_u32 = at_u32(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(b);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                push_mapped_val(val_map, at_u32(view.extra, p + k), fixups, func);
                            }
                            p += an;
                        }
                    }
                    Field::Mask | Field::Blocks => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        func.extra.push(at_u32(view.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    func.ops.push(op_raw);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    out_idx
}

fn apply_val_fixups<A: Allocator>(
    func: &mut Func<A>,
    val_map: &[u32],
    fixups: &[ValFixup],
) -> Result<(), SimplifyError> {
    for &fix in fixups {
        match fix {
            ValFixup::Data { dst_inst, old_val } => {
                let mapped = at_u32(val_map, to_idx(old_val));
                if mapped == NONE {
                    return Err(SimplifyError::Refused { inst: dst_inst, what: "unmapped operand during if-conversion" });
                }
                set_at(&mut func.data, dst_inst, mapped);
            }
            ValFixup::Extra { dst_extra, old_val } => {
                let mapped = at_u32(val_map, to_idx(old_val));
                if mapped == NONE {
                    return Err(SimplifyError::Refused { inst: dst_extra, what: "unmapped operand during if-conversion" });
                }
                set_at(&mut func.extra, dst_extra, mapped);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct RawSwitchEdge {
    val: u64,
    block: u32,
    args_at: usize,
    argc: usize,
}

fn try_match_switch(
    view: &View<'_>,
    head: u32,
    term: usize,
    max_arm_raw_insts: usize,
    arm_ok: &impl Fn(u32, u32, usize, usize, usize) -> Option<(u32, usize, usize)>,
) -> Option<SwitchPattern> {
    let at = to_idx(at_u32(view.data, term));
    let scrut = at_u32(view.extra, at);
    let scrut_ty = Ty(at_u16(view.tys, to_idx(scrut)));
    let scrut_bits = scrut_ty.bits(64);
    if !scrut_ty.is_scalar() || !scrut_ty.is_int() || !(8..=64).contains(&scrut_bits) {
        return None;
    }
    let const_words = to_idx(scrut_ty.const_words());
    if const_words != 1 && const_words != 2 {
        return None;
    }
    let default_blk = at_u32(view.extra, at + 1);
    let default_argc = to_idx(at_u32(view.extra, at + 2));
    let default_args_at = at + 3;
    let mut p = default_args_at + default_argc;
    let case_count = to_idx(at_u32(view.extra, p));
    p += 1;
    if !(MIN_SWITCH_CASES..=MAX_SWITCH_CASES).contains(&case_count) {
        return None;
    }

    let mut raw_cases = [RawSwitchEdge::default(); MAX_SWITCH_CASES];
    for i in 0..case_count {
        let val = if const_words == 1 {
            u64::from(at_u32(view.extra, p))
        } else {
            u64::from(at_u32(view.extra, p)) | (u64::from(at_u32(view.extra, p + 1)) << 32)
        };
        p += const_words;
        let blk = at_u32(view.extra, p);
        let argc = to_idx(at_u32(view.extra, p + 1));
        let args_at = p + 2;
        p = args_at + argc;
        set_at(&mut raw_cases, i, RawSwitchEdge { val, block: blk, args_at, argc });
    }

    // Identify the shared `join` block: either the jump target of an arm, or a single direct target.
    let mut join_opt = None;
    if default_argc == 0 {
        if let Some((j, _, _)) = arm_ok(head, default_blk, 0, MAX_SWITCH_ARM_INSTS, max_arm_raw_insts) {
            join_opt = Some(j);
        }
    }
    if join_opt.is_none() {
        for i in 0..case_count {
            let rc = raw_cases.get(i).copied()?;
            if rc.argc == 0 {
                if let Some((j, _, _)) = arm_ok(head, rc.block, 0, MAX_SWITCH_ARM_INSTS, max_arm_raw_insts) {
                    join_opt = Some(j);
                    break;
                }
            }
        }
    }
    if join_opt.is_none() {
        let mut all_same = true;
        for i in 0..case_count {
            let rc = raw_cases.get(i).copied()?;
            if rc.block != default_blk {
                all_same = false;
                break;
            }
        }
        if all_same {
            join_opt = Some(default_blk);
        }
    }
    let join = join_opt?;
    if join == 0 || view.block_kind(Block(join)) != BlockKind::Normal {
        return None;
    }

    let join_params = view.params(Block(join));
    let mut arms = [0u32; MAX_SWITCH_ARMS];
    let mut arm_jats = [0usize; MAX_SWITCH_ARMS];
    let mut arm_count = 0usize;
    let mut total_insts = 0usize;

    let mut resolve_edge = |blk: u32, args_at: usize, argc: usize| -> Option<usize> {
        if blk == join {
            if argc != join_params.len() {
                return None;
            }
            return Some(args_at);
        }
        if argc != 0 {
            return None;
        }
        for idx in 0..arm_count {
            if at_u32(&arms, idx) == blk {
                let jat = arm_jats.get(idx).copied()?;
                return Some(jat + 2);
            }
        }
        let (j, body_len, jat) = arm_ok(head, blk, 0, MAX_SWITCH_ARM_INSTS, max_arm_raw_insts)?;
        if j != join {
            return None;
        }
        let arm_jargc = to_idx(at_u32(view.extra, jat + 1));
        if arm_jargc != join_params.len() {
            return None;
        }
        total_insts += body_len;
        if total_insts > MAX_SWITCH_TOTAL_INSTS || arm_count >= MAX_SWITCH_ARMS {
            return None;
        }
        set_at(&mut arms, arm_count, blk);
        set_at(&mut arm_jats, arm_count, jat);
        arm_count += 1;
        Some(jat + 2)
    };

    let default_join_args_at = resolve_edge(default_blk, default_args_at, default_argc)?;
    let mut cases = [SwitchCaseInfo { val: 0, join_args_at: 0 }; MAX_SWITCH_CASES];
    for i in 0..case_count {
        let rc = raw_cases.get(i).copied()?;
        let join_args_at = resolve_edge(rc.block, rc.args_at, rc.argc)?;
        set_at(&mut cases, i, SwitchCaseInfo { val: rc.val, join_args_at });
    }

    let mut diff_params = 0usize;
    for k in 0..join_params.len() {
        let d_arg = at_u32(view.extra, default_join_args_at + k);
        let mut differs = false;
        for i in 0..case_count {
            let ci = cases.get(i).copied()?;
            if at_u32(view.extra, ci.join_args_at + k) != d_arg {
                differs = true;
                break;
            }
        }
        if differs {
            diff_params += 1;
            if diff_params > MAX_SWITCH_DIFF_PARAMS
                || !is_selectable_ty(Ty(at_u16(view.tys, join_params.start + k)))
            {
                return None;
            }
        }
    }

    Some(SwitchPattern {
        head,
        scrut,
        scrut_ty,
        join,
        default_join_args_at,
        case_count: u8::try_from(case_count).ok()?,
        cases,
        arm_count: u8::try_from(arm_count).ok()?,
        arms,
    })
}

/// Finds one pure triangle/diamond `if` or small pure `switch` eligible for branchless
/// if-conversion, gated by target microarchitecture limits and optional PGO branch entropy.
///
/// Works for ANY `join` block regardless of how many other predecessors `join` has
/// (for example, a loop header with a preheader edge plus all branch/switch arms).
fn find_if_pattern<A: Allocator + Copy>(
    view: &View<'_>,
    target: IfConvTarget,
    profile: BranchProfile<'_>,
    alloc: A,
) -> FindOutcome {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return FindOutcome::Done { predictable_preserved: 0 };
    }

    let mut inst_block = filled(len, 0u32, &alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            set_at(&mut inst_block, inst, b32);
        }
        let term = view.terminator(Block(b32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(p) = preds.get_mut(tb) {
                    if p.scan_for(&b32).is_none() {
                        p.push(b32);
                    }
                }
            }
        });
    }

    let arm_ok = |head: u32,
                  arm: u32,
                  expected_argc: usize,
                  max_insts: usize,
                  max_raw_insts: usize|
     -> Option<(u32, usize, usize)> {
        if arm == 0 || arm == head || view.block_kind(Block(arm)) != BlockKind::Normal {
            return None;
        }
        let arm_preds = preds.get(to_idx(arm))?;
        if arm_preds.len() != 1 || arm_preds.first().copied() != Some(head) {
            return None;
        }
        let arm_params = view.params(Block(arm));
        if arm_params.len() != expected_argc {
            return None;
        }
        let range = view.block(Block(arm));
        let term = range.end.checked_sub(1)?;
        if view.op(term) != Op::Jump {
            return None;
        }
        let body_len = term.checked_sub(arm_params.end)?;
        if body_len > max_raw_insts {
            return None;
        }
        let mut non_const_insts = 0usize;
        for inst in arm_params.end..term {
            let op = view.op(inst);
            if !is_speculable_pure(op, Flags(at_u8(view.flags, inst))) {
                return None;
            }
            if !is_zero_cost_const(op) {
                non_const_insts += 1;
            }
        }
        if non_const_insts > max_insts {
            return None;
        }
        // Ensure no instruction or parameter defined inside `arm` is used directly in any other block.
        for b in 0..blocks {
            let b32 = to_u32(b);
            if b32 == arm {
                continue;
            }
            for inst in view.block(Block(b32)) {
                let mut escaped = false;
                each(view, inst, |item| match item {
                    Item::Value(v) => {
                        if at_u32(&inst_block, to_idx(v)) == arm {
                            escaped = true;
                        }
                    }
                    Item::Edge(edge) => {
                        if let Some((arg_start, arg_len)) = edge.args {
                            for k in 0..arg_len {
                                let v = at_u32(view.extra, arg_start + k);
                                if at_u32(&inst_block, to_idx(v)) == arm {
                                    escaped = true;
                                }
                            }
                        }
                    }
                });
                if escaped {
                    return None;
                }
            }
        }
        let jat = to_idx(at_u32(view.data, term));
        let join = at_u32(view.extra, jat);
        if join == arm {
            return None;
        }
        Some((join, non_const_insts, jat))
    };

    let join_args_selectable = |join: u32, t_args_at: usize, e_args_at: usize| -> bool {
        let params = view.params(Block(join));
        for k in 0..params.len() {
            let ta = at_u32(view.extra, t_args_at + k);
            let ea = at_u32(view.extra, e_args_at + k);
            if ta == ea {
                continue;
            }
            if !is_selectable_ty(Ty(at_u16(view.tys, params.start + k))) {
                return false;
            }
        }
        true
    };

    let mut predictable_preserved = 0u32;

    for b in 0..blocks {
        let head = to_u32(b);
        if view.block_kind(Block(head)) != BlockKind::Normal {
            continue;
        }
        let term = view.terminator(Block(head));
        let term_op = view.op(term);
        if term_op == Op::Switch {
            if let Some(sw) = try_match_switch(view, head, term, target.max_arm_raw_insts, &arm_ok) {
                return FindOutcome::Matched {
                    pat: IfPattern::Switch(sw),
                    predictable_preserved,
                };
            }
            continue;
        }
        if term_op != Op::Br {
            continue;
        }
        let taken = profile
            .taken_permille_by_block
            .get(b)
            .copied()
            .unwrap_or(u16::MAX);
        let budget = branch_budget(target, taken);
        let at = to_idx(at_u32(view.data, term));
        let cond = at_u32(view.extra, at);
        let t_blk = at_u32(view.extra, at + 1);
        let t_argc = to_idx(at_u32(view.extra, at + 2));
        let e_at = at + 3 + t_argc;
        let e_blk = at_u32(view.extra, e_at);
        let e_argc = to_idx(at_u32(view.extra, e_at + 1));
        if t_blk == e_blk {
            let join = t_blk;
            if join != head && view.block_kind(Block(join)) == BlockKind::Normal {
                let params = view.params(Block(join));
                if t_argc == params.len()
                    && e_argc == params.len()
                    && join_args_selectable(join, at + 3, e_at + 2)
                {
                    return FindOutcome::Matched {
                        pat: IfPattern::DirectSelect { head, cond, join },
                        predictable_preserved,
                    };
                }
            }
            continue;
        }

        // 1. Triangle: `head -> t_arm -> e_blk`
        if let Some((join, n1, t_jat)) =
            arm_ok(head, t_blk, t_argc, budget.max_arm_insts, budget.max_arm_raw_insts)
        {
            if join == e_blk {
                let params = view.params(Block(join));
                let arm_argc = to_idx(at_u32(view.extra, t_jat + 1));
                if params.len() == e_argc
                    && arm_argc == params.len()
                    && join_args_selectable(join, t_jat + 2, e_at + 2)
                {
                    if budget.is_predictable && n1 > TRIVIAL_PREDICTABLE_OPS {
                        predictable_preserved += 1;
                        continue;
                    }
                    return FindOutcome::Matched {
                        pat: IfPattern::Triangle {
                            head,
                            cond,
                            arm: t_blk,
                            join,
                            arm_is_then: true,
                        },
                        predictable_preserved,
                    };
                }
            }
        }

        // 2. Triangle: `head -> e_arm -> t_blk`
        if let Some((join, n2, e_jat)) =
            arm_ok(head, e_blk, e_argc, budget.max_arm_insts, budget.max_arm_raw_insts)
        {
            if join == t_blk {
                let params = view.params(Block(join));
                let arm_argc = to_idx(at_u32(view.extra, e_jat + 1));
                if params.len() == t_argc
                    && arm_argc == params.len()
                    && join_args_selectable(join, at + 3, e_jat + 2)
                {
                    if budget.is_predictable && n2 > TRIVIAL_PREDICTABLE_OPS {
                        predictable_preserved += 1;
                        continue;
                    }
                    return FindOutcome::Matched {
                        pat: IfPattern::Triangle {
                            head,
                            cond,
                            arm: e_blk,
                            join,
                            arm_is_then: false,
                        },
                        predictable_preserved,
                    };
                }
            }
        }

        // 3. Diamond: `head -> t_arm -> join` AND `head -> e_arm -> join`
        //    (works for ANY `join`, including multi-predecessor loop headers!).
        if let (Some((j1, n1, t_jat)), Some((j2, n2, e_jat))) = (
            arm_ok(head, t_blk, t_argc, budget.max_arm_insts, budget.max_arm_raw_insts),
            arm_ok(head, e_blk, e_argc, budget.max_arm_insts, budget.max_arm_raw_insts),
        ) {
            if j1 == j2 && n1 + n2 <= budget.max_diamond_insts {
                let params = view.params(Block(j1));
                let t_jargc = to_idx(at_u32(view.extra, t_jat + 1));
                let e_jargc = to_idx(at_u32(view.extra, e_jat + 1));
                if t_jargc == params.len()
                    && e_jargc == params.len()
                    && join_args_selectable(j1, t_jat + 2, e_jat + 2)
                {
                    if budget.is_predictable && n1 + n2 > TRIVIAL_PREDICTABLE_OPS {
                        predictable_preserved += 1;
                        continue;
                    }
                    return FindOutcome::Matched {
                        pat: IfPattern::Diamond {
                            head,
                            cond,
                            t_arm: t_blk,
                            e_arm: e_blk,
                            join: j1,
                        },
                        predictable_preserved,
                    };
                }
            }
        }
    }

    FindOutcome::Done { predictable_preserved }
}

/// If `val` is `op(acc, x)` (or `op(x, acc)`) for an associative integer reduction `Add`/`Xor`/`Or`/`And`
/// with identity `id` (`0` for `Add`/`Xor`/`Or`, `!0` for `And`), returns `Some((op, x, id))`.
fn match_reduction_on_acc(func: &Func<impl Allocator>, val: u32, acc: u32, ty: Ty) -> Option<(Op, u32, u64)> {
    if val == NONE || acc == NONE || !(ty.is_scalar() && ty.is_int() && ty.bits(64) <= 64) {
        return None;
    }
    let vi = to_idx(val);
    let op = Op::from_byte(func.ops.get(vi).copied()?)?;
    if !matches!(op, Op::Add | Op::Xor | Op::Or | Op::And) {
        return None;
    }
    if op.info().flags.meanings_only(Flags(at_u8(&func.flags, vi))).0 != 0 {
        return None;
    }
    let at = to_idx(at_u32(&func.data, vi));
    let (l, r) = (at_u32(&func.extra, at), at_u32(&func.extra, at + 1));
    let other = if l == acc {
        r
    } else if r == acc {
        l
    } else {
        return None;
    };
    let id = if op == Op::And { u64::MAX } else { 0 };
    Some((op, other, id))
}

fn emit_switch_head<A: Allocator + Copy>(
    view: &View<'_>,
    sw: SwitchPattern,
    val_map: &mut [u32],
    fixups: &mut Vec<ValFixup, A>,
    func: &mut Func<A>,
    alloc: A,
) {
    // Inline each distinct arm's pure body once into `head`, stripping promise/hint flags so
    // speculative execution on untaken paths cannot introduce UB.
    for a_idx in 0..usize::from(sw.arm_count) {
        let arm = at_u32(&sw.arms, a_idx);
        let ar = view.block(Block(arm));
        let a_params = view.params(Block(arm));
        let aterm = ar.end.saturating_sub(1);
        for inst in a_params.end..aterm {
            let mapped = copy_remapped(view, inst, val_map, fixups, func);
            set_at(&mut func.flags, to_idx(mapped), Flags::NONE.0);
            set_at(val_map, inst, mapped);
        }
    }

    let m = usize::from(sw.case_count);
    let mut dense_order = [0usize; MAX_SWITCH_CASES];
    let mut dense_seen = [false; MAX_SWITCH_CASES];
    let mut is_dense = true;
    for i in 0..m {
        let Some(ci) = sw.cases.get(i).copied() else {
            is_dense = false;
            break;
        };
        let Ok(slot) = usize::try_from(ci.val) else {
            is_dense = false;
            break;
        };
        if slot >= m || dense_seen.get(slot).copied().unwrap_or(true) {
            is_dense = false;
            break;
        }
        set_at(&mut dense_seen, slot, true);
        set_at(&mut dense_order, slot, i);
    }

    let scrut = resolve_val(val_map, sw.scrut);
    let mut ctx = SwitchEmitCtx {
        scrut,
        scrut_ty: sw.scrut_ty,
        cond_cache: [None; MAX_SWITCH_CASES],
        fixups,
        func,
    };

    let j_params = view.params(Block(sw.join));
    let mut merged: Vec<EmitVal, A> = Vec::with_capacity_in(j_params.len(), alloc);

    for k in 0..j_params.len() {
        let default_val = resolve_val(val_map, at_u32(view.extra, sw.default_join_args_at + k));
        let mut all_same = true;
        for i in 0..m {
            if let Some(ci) = sw.cases.get(i).copied() {
                if resolve_val(val_map, at_u32(view.extra, ci.join_args_at + k)) != default_val {
                    all_same = false;
                    break;
                }
            }
        }
        if all_same {
            merged.push(default_val);
            continue;
        }

        let pty = Ty(at_u16(view.tys, j_params.start + k));
        let res = if is_dense {
            let mut vals = [default_val; MAX_SWITCH_CASES];
            for j in 0..m {
                let case_idx = dense_order.get(j).copied().unwrap_or(0);
                let args_at = sw
                    .cases
                    .get(case_idx)
                    .map_or(sw.default_join_args_at, |c| c.join_args_at);
                let v = resolve_val(val_map, at_u32(view.extra, args_at + k));
                set_at(&mut vals, j, v);
            }
            let [v0, v1, v2, v3, v4, v5, v6, v7] = vals;
            // Balanced depth-3 binary Select tree for dense `0..m` switches:
            // Level 1: 4 independent leaf selects
            let c01 = ctx.sel_cmp(0, IntCond::Eq, 0, pty, v0, v1);
            let c23 = ctx.sel_cmp(1, IntCond::Eq, 2, pty, v2, v3);
            // Level 2 (left half)
            let c03 = ctx.sel_cmp(4, IntCond::Ult, 2, pty, c01, c23);
            let mut tree = if m <= 3 {
                // For m <= 3, v3 == default_val so c23 already yields default_val for scrut >= 3.
                c03
            } else {
                let c45 = ctx.sel_cmp(2, IntCond::Eq, 4, pty, v4, v5);
                let c67 = ctx.sel_cmp(3, IntCond::Eq, 6, pty, v6, v7);
                // Level 2 (right half): for m == 5, v5 == default_val so c45 already yields default_val for scrut >= 5.
                let c47 = if m == 5 {
                    c45
                } else {
                    ctx.sel_cmp(5, IntCond::Ult, 6, pty, c45, c67)
                };
                // Level 3
                ctx.sel_cmp(6, IntCond::Ult, 4, pty, c03, c47)
            };
            if m == MAX_SWITCH_CASES && v7 != default_val {
                tree = ctx.sel_cmp(7, IntCond::Ult, 8, pty, tree, default_val);
            }
            tree
        } else {
            let mut acc = default_val;
            for i in 0..m {
                let Some(ci) = sw.cases.get(i).copied() else {
                    continue;
                };
                let cv = resolve_val(val_map, at_u32(view.extra, ci.join_args_at + k));
                acc = ctx.sel_cmp(i, IntCond::Eq, ci.val, pty, cv, acc);
            }
            acc
        };
        merged.push(res);
    }

    let jat = to_u32(ctx.func.extra.len());
    ctx.func.extra.push(sw.join);
    ctx.func.extra.push(to_u32(merged.len()));
    for &v in &merged {
        push_emit_val(v, ctx.fixups, ctx.func);
    }
    push_inst(ctx.func, Op::Jump, Flags::NONE, Ty::NONE, jat);
}

fn trivial_target(view: &View<'_>, inst: usize) -> u32 {
    match view.op(inst) {
        Op::Br => {
            let mut o = view.operands(inst);
            let cond = o.value();
            let (then, otherwise) = (o.target(), o.target());
            match constant_of(view, cond.0) {
                Some(value) => {
                    if value != 0 {
                        0
                    } else {
                        1
                    }
                }
                None if then.block == otherwise.block && then.args.0 == otherwise.args.0 => 0,
                None => NONE,
            }
        }
        Op::Switch => {
            let mut o = view.operands(inst);
            let scrutinee = o.value();
            let Some(value) = constant_of(view, scrutinee.0) else {
                return NONE;
            };
            let width = Ty(at_u16(view.tys, scrutinee.index())).bits(view.head.ptr_bits);
            let _ = o.target();
            let found = o
                .cases(scrutinee)
                .position(|(words, _)| wide_value(words, width) == value);
            match found {
                Some(case) => to_u32(case).saturating_add(1),
                None => 0,
            }
        }
        _ => NONE,
    }
}

/// Remaps per-block PGO taken-permille biases across `dead` (`fold_trivial = true`) or
/// `coalesce_cfg` (`fold_trivial = false`), preserving each surviving block's terminator bias.
fn remap_taken_for_cleanup<A: Allocator + Copy>(
    view: &View<'_>,
    taken: &[u16],
    fold_trivial: bool,
    alloc: A,
) -> Vec<u16, A> {
    let blocks = view.blocks();
    let len = view.len();
    let mut jump = filled(len, NONE, &alloc);
    if fold_trivial {
        for b in 0..blocks {
            let end = view.terminator(Block(to_u32(b)));
            let chosen = trivial_target(view, end);
            if chosen != NONE {
                set_at(&mut jump, end, chosen);
            }
        }
    }
    let reached = reachable(view, &jump, &alloc);
    let into = incoming(view, &reached, &jump, &alloc);
    let mut merge_next = filled(blocks, NONE, &alloc);
    let mut merged_into_pred = filled(blocks, false, &alloc);
    for a in 0..blocks {
        if !reached.get(a).copied().unwrap_or(false) {
            continue;
        }
        let term = view.terminator(Block(to_u32(a)));
        if view.op(term) != Op::Jump {
            continue;
        }
        let b = at_u32(view.extra, to_idx(at_u32(view.data, term)));
        let b_idx = to_idx(b);
        if b != 0
            && b_idx != a
            && reached.get(b_idx).copied().unwrap_or(false)
            && into.targets_only.get(b_idx).copied().unwrap_or(false)
            && into.edges_into(b_idx).len() == 1
            && !view.block_kind(Block(b)).is_landing()
        {
            set_at(&mut merge_next, a, b);
            set_at(&mut merged_into_pred, b_idx, true);
        }
    }
    let mut out: Vec<u16, A> = Vec::with_capacity_in(blocks, alloc);
    for head in 0..blocks {
        if !reached.get(head).copied().unwrap_or(false)
            || merged_into_pred.get(head).copied().unwrap_or(false)
        {
            continue;
        }
        let mut tail = head;
        for _ in 0..blocks {
            let next = at_u32(&merge_next, tail);
            if next == NONE {
                break;
            }
            tail = to_idx(next);
        }
        let tail_term = view.terminator(Block(to_u32(tail)));
        let bias = if at_u32(&jump, tail_term) != NONE {
            u16::MAX
        } else {
            taken.get(tail).copied().unwrap_or(u16::MAX)
        };
        out.push(bias);
    }
    out
}

fn apply_if_pattern<A: Allocator + Copy>(
    view: &View<'_>,
    pat: IfPattern,
    taken_by_block: &[u16],
    alloc: A,
) -> Result<AppliedPattern<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);
    let mut param_aliases: Vec<(usize, u32), A> = Vec::new_in(alloc);
    let mut canonicalized = 0u32;

    let head = match pat {
        IfPattern::DirectSelect { head, .. }
        | IfPattern::Triangle { head, .. }
        | IfPattern::Diamond { head, .. } => head,
        IfPattern::Switch(sw) => sw.head,
    };

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));
        let term = range.end - 1;

        if b32 == head {
            for inst in range.start..term {
                let mapped = copy_remapped(view, inst, &val_map, &mut fixups, &mut func);
                set_at(&mut val_map, inst, mapped);
            }

            let (cond_old, join, t_args_old, e_args_old) = match pat {
                IfPattern::Switch(sw) => {
                    emit_switch_head(view, sw, &mut val_map, &mut fixups, &mut func, alloc);
                    continue;
                }
                IfPattern::DirectSelect { cond, join, .. } => {
                    let at = to_idx(at_u32(view.data, term));
                    let t_argc = to_idx(at_u32(view.extra, at + 2));
                    let e_at = at + 3 + t_argc;
                    let e_argc = to_idx(at_u32(view.extra, e_at + 1));
                    let mut t_args: Vec<u32, A> = Vec::with_capacity_in(t_argc, alloc);
                    for k in 0..t_argc {
                        t_args.push(at_u32(view.extra, at + 3 + k));
                    }
                    let mut e_args: Vec<u32, A> = Vec::with_capacity_in(e_argc, alloc);
                    for k in 0..e_argc {
                        e_args.push(at_u32(view.extra, e_at + 2 + k));
                    }
                    (cond, join, t_args, e_args)
                }
                IfPattern::Triangle { cond, arm, join, arm_is_then, .. } => {
                    let at = to_idx(at_u32(view.data, term));
                    let t_argc = to_idx(at_u32(view.extra, at + 2));
                    let e_at = at + 3 + t_argc;
                    let e_argc = to_idx(at_u32(view.extra, e_at + 1));
                    let ar = view.block(Block(arm));
                    let a_params = view.params(Block(arm));
                    let aterm = ar.end - 1;
                    let br_arm_args_at = if arm_is_then { at + 3 } else { e_at + 2 };
                    for k in 0..a_params.len() {
                        let incoming = at_u32(view.extra, br_arm_args_at + k);
                        let mapped = at_u32(&val_map, to_idx(incoming));
                        if mapped == NONE {
                            param_aliases.push((a_params.start + k, incoming));
                        }
                        set_at(&mut val_map, a_params.start + k, mapped);
                    }
                    for inst in a_params.end..aterm {
                        let mapped = copy_remapped(view, inst, &val_map, &mut fixups, &mut func);
                        set_at(&mut val_map, inst, mapped);
                    }
                    let jat = to_idx(at_u32(view.data, aterm));
                    let argc = to_idx(at_u32(view.extra, jat + 1));
                    let mut arm_args: Vec<u32, A> = Vec::with_capacity_in(argc, alloc);
                    for k in 0..argc {
                        arm_args.push(at_u32(view.extra, jat + 2 + k));
                    }
                    let mut dir_args: Vec<u32, A> = Vec::with_capacity_in(argc, alloc);
                    if arm_is_then {
                        for k in 0..e_argc {
                            dir_args.push(at_u32(view.extra, e_at + 2 + k));
                        }
                        (cond, join, arm_args, dir_args)
                    } else {
                        for k in 0..t_argc {
                            dir_args.push(at_u32(view.extra, at + 3 + k));
                        }
                        (cond, join, dir_args, arm_args)
                    }
                }
                IfPattern::Diamond { cond, t_arm, e_arm, join, .. } => {
                    let at = to_idx(at_u32(view.data, term));
                    let t_argc = to_idx(at_u32(view.extra, at + 2));
                    let e_at = at + 3 + t_argc;
                    let tr = view.block(Block(t_arm));
                    let t_params = view.params(Block(t_arm));
                    let tterm = tr.end - 1;
                    for k in 0..t_params.len() {
                        let incoming = at_u32(view.extra, at + 3 + k);
                        let mapped = at_u32(&val_map, to_idx(incoming));
                        if mapped == NONE {
                            param_aliases.push((t_params.start + k, incoming));
                        }
                        set_at(&mut val_map, t_params.start + k, mapped);
                    }
                    for inst in t_params.end..tterm {
                        let mapped = copy_remapped(view, inst, &val_map, &mut fixups, &mut func);
                        set_at(&mut val_map, inst, mapped);
                    }
                    let er = view.block(Block(e_arm));
                    let e_params = view.params(Block(e_arm));
                    let eterm = er.end - 1;
                    for k in 0..e_params.len() {
                        let incoming = at_u32(view.extra, e_at + 2 + k);
                        let mapped = at_u32(&val_map, to_idx(incoming));
                        if mapped == NONE {
                            param_aliases.push((e_params.start + k, incoming));
                        }
                        set_at(&mut val_map, e_params.start + k, mapped);
                    }
                    for inst in e_params.end..eterm {
                        let mapped = copy_remapped(view, inst, &val_map, &mut fixups, &mut func);
                        set_at(&mut val_map, inst, mapped);
                    }
                    let tjat = to_idx(at_u32(view.data, tterm));
                    let tlen = to_idx(at_u32(view.extra, tjat + 1));
                    let mut t_args: Vec<u32, A> = Vec::with_capacity_in(tlen, alloc);
                    for k in 0..tlen {
                        t_args.push(at_u32(view.extra, tjat + 2 + k));
                    }
                    let ejat = to_idx(at_u32(view.data, eterm));
                    let elen = to_idx(at_u32(view.extra, ejat + 1));
                    let mut e_args: Vec<u32, A> = Vec::with_capacity_in(elen, alloc);
                    for k in 0..elen {
                        e_args.push(at_u32(view.extra, ejat + 2 + k));
                    }
                    (cond, join, t_args, e_args)
                }
            };

            let cond_new = at_u32(&val_map, to_idx(cond_old));
            let j_params = view.params(Block(join));
            let mut merged_vals: Vec<(u32, Option<u32>), A> = Vec::with_capacity_in(t_args_old.len(), alloc);
            for k in 0..t_args_old.len() {
                let tv_old = at_u32(&t_args_old, k);
                let ev_old = at_u32(&e_args_old, k);
                let tv = at_u32(&val_map, to_idx(tv_old));
                let ev = at_u32(&val_map, to_idx(ev_old));
                if tv_old == ev_old || (tv != NONE && tv == ev) {
                    if tv == NONE {
                        merged_vals.push((0, Some(tv_old)));
                    } else {
                        merged_vals.push((tv, None));
                    }
                    continue;
                }
                let pty = Ty(at_u16(view.tys, j_params.start + k));
                // Canonicalize conditional reductions:
                //   `Select(c, acc + x, acc) -> acc + Select(c, x, 0)`
                //   `Select(c, acc, acc + x) -> acc + Select(c, 0, x)`
                if cond_new != NONE
                    && fixups.is_empty()
                    && let Some((rop, x, id)) = match_reduction_on_acc(&func, tv, ev, pty)
                {
                    let id_c = push_iconst(&mut func, pty, id);
                    let masked_x = push_select(&mut func, pty, cond_new, x, id_c);
                    let upd = push_bin(&mut func, rop, Flags::NONE, pty, ev, masked_x);
                    merged_vals.push((upd, None));
                    canonicalized += 1;
                } else if cond_new != NONE
                    && fixups.is_empty()
                    && let Some((rop, x, id)) = match_reduction_on_acc(&func, ev, tv, pty)
                {
                    let id_c = push_iconst(&mut func, pty, id);
                    let masked_x = push_select(&mut func, pty, cond_new, id_c, x);
                    let upd = push_bin(&mut func, rop, Flags::NONE, pty, tv, masked_x);
                    merged_vals.push((upd, None));
                    canonicalized += 1;
                } else {
                    let sel_at = to_u32(func.extra.len());
                    push_mapped_val(&val_map, cond_old, &mut fixups, &mut func);
                    push_mapped_val(&val_map, tv_old, &mut fixups, &mut func);
                    push_mapped_val(&val_map, ev_old, &mut fixups, &mut func);
                    let sel = push_inst(&mut func, Op::Select, Flags::NONE, pty, sel_at);
                    merged_vals.push((sel, None));
                }
            }
            let jat = to_u32(func.extra.len());
            func.extra.push(join);
            func.extra.push(to_u32(merged_vals.len()));
            for &(v, pending) in &merged_vals {
                if let Some(old_val) = pending {
                    let dst_extra = func.extra.len();
                    fixups.push(ValFixup::Extra { dst_extra, old_val });
                    func.extra.push(0);
                } else {
                    func.extra.push(v);
                }
            }
            push_inst(&mut func, Op::Jump, Flags::NONE, Ty::NONE, jat);
        } else {
            let is_dead_arm = match pat {
                IfPattern::DirectSelect { .. } => false,
                IfPattern::Triangle { arm, .. } => b32 == arm,
                IfPattern::Diamond { t_arm, e_arm, .. } => b32 == t_arm || b32 == e_arm,
                IfPattern::Switch(sw) => {
                    let mut found = false;
                    for i in 0..usize::from(sw.arm_count) {
                        if at_u32(&sw.arms, i) == b32 {
                            found = true;
                            break;
                        }
                    }
                    found
                }
            };
            if is_dead_arm {
                // Now unreachable; `dead` + `coalesce_cfg` removes it immediately after.
                push_inst(&mut func, Op::Unreachable, Flags::NONE, Ty::NONE, 0);
            } else {
                for inst in range {
                    let mapped = copy_remapped(view, inst, &val_map, &mut fixups, &mut func);
                    set_at(&mut val_map, inst, mapped);
                }
            }
        }
    }

    for &(param_old, incoming_old) in &param_aliases {
        let mapped = at_u32(&val_map, to_idx(incoming_old));
        set_at(&mut val_map, param_old, mapped);
    }
    apply_val_fixups(&mut func, &val_map, &fixups)?;
    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    let (d, d_taken) = if taken_by_block.is_empty() {
        (dead(func.view(), alloc)?, Vec::new_in(alloc))
    } else {
        let mut func_taken: Vec<u16, A> = Vec::with_capacity_in(blocks, alloc);
        for b in 0..blocks {
            let bias = if to_u32(b) == head {
                u16::MAX
            } else {
                taken_by_block.get(b).copied().unwrap_or(u16::MAX)
            };
            func_taken.push(bias);
        }
        let remapped = remap_taken_for_cleanup(&func.view(), &func_taken, true, alloc);
        (dead(func.view(), alloc)?, remapped)
    };
    let (c, c_taken) = if d_taken.is_empty() {
        (coalesce_cfg(d.view(), alloc)?, Vec::new_in(alloc))
    } else {
        let remapped = remap_taken_for_cleanup(&d.view(), &d_taken, false, alloc);
        (coalesce_cfg(d.view(), alloc)?, remapped)
    };
    Ok(AppliedPattern {
        func: c,
        reductions_canonicalized: canonicalized,
        taken_by_block: c_taken,
    })
}

const MAX_SELECT_BALANCE_ITERS: u32 = 4;

#[derive(Clone, Copy)]
struct ConstCmp {
    lhs: u32,
    ty: Ty,
    pred: IntCond,
    rhs: u128,
}

#[inline]
const fn swap_int_cond(cc: IntCond) -> IntCond {
    match cc {
        IntCond::Eq => IntCond::Eq,
        IntCond::Ne => IntCond::Ne,
        IntCond::Ult => IntCond::Ugt,
        IntCond::Ule => IntCond::Uge,
        IntCond::Ugt => IntCond::Ult,
        IntCond::Uge => IntCond::Ule,
        IntCond::Slt => IntCond::Sgt,
        IntCond::Sle => IntCond::Sge,
        IntCond::Sgt => IntCond::Slt,
        IntCond::Sge => IntCond::Sle,
    }
}

#[inline]
const fn invert_int_cond(cc: IntCond) -> IntCond {
    match cc {
        IntCond::Eq => IntCond::Ne,
        IntCond::Ne => IntCond::Eq,
        IntCond::Ult => IntCond::Uge,
        IntCond::Ule => IntCond::Ugt,
        IntCond::Ugt => IntCond::Ule,
        IntCond::Uge => IntCond::Ult,
        IntCond::Slt => IntCond::Sge,
        IntCond::Sle => IntCond::Sgt,
        IntCond::Sgt => IntCond::Sle,
        IntCond::Sge => IntCond::Slt,
    }
}

fn extract_iconst(view: &View<'_>, val: u32, width: u32) -> Option<u128> {
    let vi = to_idx(val);
    if vi >= view.len() {
        return None;
    }
    match view.op(vi) {
        Op::Iconst => Some(iconst_value(at_u32(view.data, vi), width)),
        Op::IconstWide => {
            let ty = Ty(at_u16(view.tys, vi));
            let words = to_idx(ty.const_words());
            let at = to_idx(at_u32(view.data, vi));
            let slice = view.extra.get(at..at.checked_add(words)?)?;
            Some(wide_value(slice, width))
        }
        _ => None,
    }
}

fn extract_const_cmp(view: &View<'_>, cond_val: u32) -> Option<ConstCmp> {
    let ci = to_idx(cond_val);
    if ci >= view.len() || view.op(ci) != Op::Icmp {
        return None;
    }
    if Ty(at_u16(view.tys, ci)) != Ty::I1 {
        return None;
    }
    let at = to_idx(at_u32(view.data, ci));
    let pred = IntCond::from_word(at_u32(view.extra, at))?;
    let lhs = at_u32(view.extra, at + 1);
    let rhs = at_u32(view.extra, at + 2);
    let lhs_idx = to_idx(lhs);
    let rhs_idx = to_idx(rhs);
    if lhs_idx >= view.len() || rhs_idx >= view.len() {
        return None;
    }
    let ty = Ty(at_u16(view.tys, lhs_idx));
    if !ty.is_scalar() || !ty.is_int() {
        return None;
    }
    let width = ty.bits(64);
    if !(1..=128).contains(&width) {
        return None;
    }
    if let Some(k) = extract_iconst(view, rhs, width) {
        if view.op(lhs_idx) == Op::Uninit {
            return None;
        }
        return Some(ConstCmp { lhs, ty, pred, rhs: k });
    }
    if let Some(k) = extract_iconst(view, lhs, width) {
        if view.op(rhs_idx) == Op::Uninit {
            return None;
        }
        return Some(ConstCmp { lhs: rhs, ty, pred: swap_int_cond(pred), rhs: k });
    }
    None
}

/// Returns `Some(None)` for the empty set, `Some(Some((lo, hi)))` for `[lo, hi]`, or `None` if
/// `pred` does not represent a single unsigned interval.
fn unsigned_interval(pred: IntCond, k: u128, max_u: u128) -> Option<Option<(u128, u128)>> {
    match pred {
        IntCond::Eq => Some(Some((k, k))),
        IntCond::Ult => Some(k.checked_sub(1).map(|hi| (0, hi))),
        IntCond::Ule => Some(Some((0, k))),
        IntCond::Ugt => Some(if k == max_u { None } else { Some((k + 1, max_u)) }),
        IntCond::Uge => Some(Some((k, max_u))),
        IntCond::Ne | IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge => None,
    }
}

/// Returns `Some(None)` for the empty set, `Some(Some((lo, hi)))` for `[lo, hi]`, or `None` if
/// `pred` does not represent a single signed interval.
fn signed_interval(pred: IntCond, k: i128, min_s: i128, max_s: i128) -> Option<Option<(i128, i128)>> {
    match pred {
        IntCond::Eq => Some(Some((k, k))),
        IntCond::Slt => Some(if k == min_s { None } else { Some((min_s, k - 1)) }),
        IntCond::Sle => Some(Some((min_s, k))),
        IntCond::Sgt => Some(if k == max_s { None } else { Some((k + 1, max_s)) }),
        IntCond::Sge => Some(Some((k, max_s))),
        IntCond::Ne | IntCond::Ult | IntCond::Ule | IntCond::Ugt | IntCond::Uge => None,
    }
}

/// Returns `Some(true)` if for every `width`-bit integer `x`, `x a_pred a_k => x b_pred b_k`.
/// Returns `Some(false)` if for every `width`-bit integer `x`, `x a_pred a_k => !(x b_pred b_k)`.
/// Returns `None` otherwise.
fn cmp_implies(a_pred: IntCond, a_k: u128, b_pred: IntCond, b_k: u128, width: u32) -> Option<bool> {
    if !(1..=128).contains(&width) {
        return None;
    }
    let m = mask(width);
    let a_k = a_k & m;
    let b_k = b_k & m;
    if a_pred == IntCond::Eq {
        return Some(compare(b_pred, a_k, b_k, width));
    }
    if let Some(a_set) = unsigned_interval(a_pred, a_k, m) {
        let Some((a_lo, a_hi)) = a_set else {
            return Some(true);
        };
        if b_pred == IntCond::Ne {
            if a_lo == b_k && a_hi == b_k {
                return Some(false);
            }
            if b_k < a_lo || b_k > a_hi {
                return Some(true);
            }
            return None;
        }
        if let Some(b_set) = unsigned_interval(b_pred, b_k, m) {
            return match b_set {
                None => Some(false),
                Some((b_lo, b_hi)) => {
                    if a_lo >= b_lo && a_hi <= b_hi {
                        Some(true)
                    } else if a_hi < b_lo || a_lo > b_hi {
                        Some(false)
                    } else {
                        None
                    }
                }
            };
        }
    }
    let min_s = sext(1u128 << (width - 1), width);
    let max_s = sext(mask(width - 1), width);
    let a_sk = sext(a_k, width);
    let b_sk = sext(b_k, width);
    if let Some(a_set) = signed_interval(a_pred, a_sk, min_s, max_s) {
        let Some((a_lo, a_hi)) = a_set else {
            return Some(true);
        };
        if b_pred == IntCond::Ne {
            if a_lo == b_sk && a_hi == b_sk {
                return Some(false);
            }
            if b_sk < a_lo || b_sk > a_hi {
                return Some(true);
            }
            return None;
        }
        if let Some(b_set) = signed_interval(b_pred, b_sk, min_s, max_s) {
            return match b_set {
                None => Some(false),
                Some((b_lo, b_hi)) => {
                    if a_lo >= b_lo && a_hi <= b_hi {
                        Some(true)
                    } else if a_hi < b_lo || a_lo > b_hi {
                        Some(false)
                    } else {
                        None
                    }
                }
            };
        }
    }
    None
}

#[derive(Clone, Copy)]
struct BalancedSelect {
    s1: usize,
    s2: usize,
    s3: usize,
    ty: Ty,
    /// First leaf select (emitted first, using `c1`).
    first_cond: u32,
    first_t: u32,
    first_f: u32,
    /// Second leaf select (emitted second, using `c2`).
    second_cond: u32,
    second_t: u32,
    second_f: u32,
    /// Root select condition (`c3`).
    root_cond: u32,
    /// If `true`, `root = Select(c3, first, second)`; else `root = Select(c3, second, first)`.
    first_is_true_arm: bool,
}

fn has_select_chain_candidate(view: &View<'_>) -> bool {
    for b in 0..view.blocks() {
        let mut count = 0u32;
        for inst in view.block(Block(to_u32(b))) {
            if view.op(inst) == Op::Select {
                count += 1;
                if count >= 3 {
                    return true;
                }
            }
        }
    }
    false
}

fn try_match_select_chain(
    view: &View<'_>,
    block_range: core::ops::Range<usize>,
    s1: usize,
    use_count: &[u32],
    sel_depth: &[u32],
    claimed: &[bool],
) -> Option<BalancedSelect> {
    if view.op(s1) != Op::Select
        || at_u8(view.flags, s1) != Flags::NONE.0
        || claimed.get(s1).copied().unwrap_or(true)
    {
        return None;
    }
    let orig_depth = at_u32(sel_depth, s1);
    if orig_depth < 3 {
        return None;
    }
    let ty = Ty(at_u16(view.tys, s1));
    let at1 = to_idx(at_u32(view.data, s1));
    let c1 = at_u32(view.extra, at1);
    let t1 = at_u32(view.extra, at1 + 1);
    let f1 = at_u32(view.extra, at1 + 2);
    let cmp1 = extract_const_cmp(view, c1)?;

    // Check `s2` on the false arm (`s1_leaf_on_true = true`) first, then on the true arm.
    for (s1_leaf_on_true, a, s2_val) in [(true, t1, f1), (false, f1, t1)] {
        let s2 = to_idx(s2_val);
        if s2 < block_range.start
            || s2 >= s1
            || claimed.get(s2).copied().unwrap_or(true)
            || at_u32(use_count, s2) != 1
            || view.op(s2) != Op::Select
            || at_u8(view.flags, s2) != Flags::NONE.0
            || Ty(at_u16(view.tys, s2)) != ty
        {
            continue;
        }
        let at2 = to_idx(at_u32(view.data, s2));
        let c2 = at_u32(view.extra, at2);
        let t2 = at_u32(view.extra, at2 + 1);
        let f2 = at_u32(view.extra, at2 + 2);
        let Some(cmp2) = extract_const_cmp(view, c2) else {
            continue;
        };
        if cmp2.lhs != cmp1.lhs || cmp2.ty != cmp1.ty {
            continue;
        }

        // Check `s3` on the false arm (`s2_leaf_on_true = true`) first, then on the true arm.
        for (s2_leaf_on_true, b, s3_val) in [(true, t2, f2), (false, f2, t2)] {
            let s3 = to_idx(s3_val);
            if s3 < block_range.start
                || s3 >= s2
                || claimed.get(s3).copied().unwrap_or(true)
                || at_u32(use_count, s3) != 1
                || view.op(s3) != Op::Select
                || at_u8(view.flags, s3) != Flags::NONE.0
                || Ty(at_u16(view.tys, s3)) != ty
            {
                continue;
            }
            let at3 = to_idx(at_u32(view.data, s3));
            let c3 = at_u32(view.extra, at3);
            let c = at_u32(view.extra, at3 + 1);
            let d = at_u32(view.extra, at3 + 2);
            let Some(cmp3) = extract_const_cmp(view, c3) else {
                continue;
            };
            if cmp3.lhs != cmp1.lhs || cmp3.ty != cmp1.ty {
                continue;
            }

            let leaf_depth = |v: u32| -> u32 {
                let vi = to_idx(v);
                if block_range.contains(&vi) {
                    at_u32(sel_depth, vi)
                } else {
                    0
                }
            };
            let max_leaf_depth = leaf_depth(a)
                .max(leaf_depth(b))
                .max(leaf_depth(c))
                .max(leaf_depth(d));
            if max_leaf_depth.saturating_add(2) >= orig_depth {
                continue;
            }

            let width = cmp1.ty.bits(64);
            let p1_pred = if s1_leaf_on_true { cmp1.pred } else { invert_int_cond(cmp1.pred) };
            let p2_pred = if s2_leaf_on_true { cmp2.pred } else { invert_int_cond(cmp2.pred) };
            let Some(imp1) = cmp_implies(p1_pred, cmp1.rhs, cmp3.pred, cmp3.rhs, width) else {
                continue;
            };
            let Some(imp2) = cmp_implies(p2_pred, cmp2.rhs, cmp3.pred, cmp3.rhs, width) else {
                continue;
            };
            if imp1 == imp2 {
                continue;
            }

            let (first_t, first_f, second_t, second_f, first_is_true_arm) = if imp1 {
                // `P1 => c3` and `P2 => !c3`:
                //   first  = if P1 { a } else { c }  (true arm of root)
                //   second = if P2 { b } else { d }  (false arm of root)
                let (ft, ff) = if s1_leaf_on_true { (a, c) } else { (c, a) };
                let (st, sf) = if s2_leaf_on_true { (b, d) } else { (d, b) };
                (ft, ff, st, sf, true)
            } else {
                // `P1 => !c3` and `P2 => c3`:
                //   first  = if P1 { a } else { d }  (false arm of root)
                //   second = if P2 { b } else { c }  (true arm of root)
                let (ft, ff) = if s1_leaf_on_true { (a, d) } else { (d, a) };
                let (st, sf) = if s2_leaf_on_true { (b, c) } else { (c, b) };
                (ft, ff, st, sf, false)
            };

            return Some(BalancedSelect {
                s1,
                s2,
                s3,
                ty,
                first_cond: c1,
                first_t,
                first_f,
                second_cond: c2,
                second_t,
                second_f,
                root_cond: c3,
                first_is_true_arm,
            });
        }
    }
    None
}

fn balance_select_chains_once<A: Allocator + Copy>(
    view: &View<'_>,
    alloc: A,
) -> Result<Option<Func<A>>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();

    let mut use_count = filled(len, 0u32, &alloc);
    for inst in 0..len {
        each(view, inst, |item| match item {
            Item::Value(v) => {
                if let Some(slot) = use_count.get_mut(to_idx(v)) {
                    *slot = slot.saturating_add(1);
                }
            }
            Item::Edge(edge) => {
                if let Some((arg_start, arg_len)) = edge.args {
                    for k in 0..arg_len {
                        let v = at_u32(view.extra, arg_start + k);
                        if let Some(slot) = use_count.get_mut(to_idx(v)) {
                            *slot = slot.saturating_add(1);
                        }
                    }
                }
            }
        });
    }

    let mut sel_depth = filled(len, 0u32, &alloc);
    for b in 0..blocks {
        let range = view.block(Block(to_u32(b)));
        let (start, end) = (range.start, range.end);
        for inst in start..end {
            if view.op(inst) == Op::Select {
                let at = to_idx(at_u32(view.data, inst));
                let tv = to_idx(at_u32(view.extra, at + 1));
                let fv = to_idx(at_u32(view.extra, at + 2));
                let td = if tv >= start && tv < end { at_u32(&sel_depth, tv) } else { 0 };
                let fd = if fv >= start && fv < end { at_u32(&sel_depth, fv) } else { 0 };
                set_at(&mut sel_depth, inst, td.max(fd).saturating_add(1));
            }
        }
    }

    let mut claimed = filled(len, false, &alloc);
    let mut skip = filled(len, false, &alloc);
    let mut rewrite_idx = filled(len, NONE, &alloc);
    let mut rewrites: Vec<BalancedSelect, A> = Vec::new_in(alloc);

    for b in 0..blocks {
        let range = view.block(Block(to_u32(b)));
        let (start, end) = (range.start, range.end);
        for inst in (start..end).rev() {
            if let Some(bs) = try_match_select_chain(view, start..end, inst, &use_count, &sel_depth, &claimed) {
                set_at(&mut claimed, bs.s1, true);
                set_at(&mut claimed, bs.s2, true);
                set_at(&mut claimed, bs.s3, true);
                set_at(&mut skip, bs.s2, true);
                set_at(&mut skip, bs.s3, true);
                let ri = to_u32(rewrites.len());
                rewrites.push(bs);
                set_at(&mut rewrite_idx, bs.s1, ri);
            }
        }
    }

    if rewrites.is_empty() {
        return Ok(None);
    }

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(b32)) {
            if skip.get(inst).copied().unwrap_or(false) {
                continue;
            }
            let ri = at_u32(&rewrite_idx, inst);
            if ri != NONE
                && let Some(bs) = rewrites.get(to_idx(ri)).copied()
            {
                let first_at = to_u32(func.extra.len());
                push_mapped_val(&val_map, bs.first_cond, &mut fixups, &mut func);
                push_mapped_val(&val_map, bs.first_t, &mut fixups, &mut func);
                push_mapped_val(&val_map, bs.first_f, &mut fixups, &mut func);
                let first = push_inst(&mut func, Op::Select, Flags::NONE, bs.ty, first_at);

                let second_at = to_u32(func.extra.len());
                push_mapped_val(&val_map, bs.second_cond, &mut fixups, &mut func);
                push_mapped_val(&val_map, bs.second_t, &mut fixups, &mut func);
                push_mapped_val(&val_map, bs.second_f, &mut fixups, &mut func);
                let second = push_inst(&mut func, Op::Select, Flags::NONE, bs.ty, second_at);

                let (root_t, root_f) = if bs.first_is_true_arm { (first, second) } else { (second, first) };
                let root_at = to_u32(func.extra.len());
                push_mapped_val(&val_map, bs.root_cond, &mut fixups, &mut func);
                func.extra.push(root_t);
                func.extra.push(root_f);
                let root = push_inst(&mut func, Op::Select, Flags::NONE, bs.ty, root_at);
                set_at(&mut val_map, inst, root);
            } else {
                let mapped = copy_remapped(view, inst, &val_map, &mut fixups, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    apply_val_fixups(&mut func, &val_map, &fixups)?;
    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(Some(func))
}

/// Rebalances linear 3-deep `Op::Select` cascades into balanced depth-2 `Op::Select` trees when
/// the integer comparison predicates provably partition the domain (`P1 => c3` and `P2 => !c3`, or
/// dual), preserving exact instruction count (3 selects -> 3 selects) while strictly reducing
/// critical-path select depth.
fn balance_select_chains<A: Allocator + Copy>(
    mut cur: Func<A>,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    for _ in 0..MAX_SELECT_BALANCE_ITERS {
        if !has_select_chain_candidate(&cur.view()) {
            break;
        }
        let Some(next) = balance_select_chains_once(&cur.view(), alloc)? else {
            break;
        };
        cur = next;
    }
    Ok(cur)
}

/// Converts same-target branches (`Br(c, (join, t), (join, e))`), pure diamond/triangle branches,
/// and small pure `Op::Switch` dispatches into branchless `Op::Select`s using target
/// microarchitecture limits and optional PGO branch-entropy data (`opts`), canonicalizing
/// conditional reductions and coalescing resulting straight-line blocks.
pub fn if_convert_with_opts<A: Allocator + Copy>(
    view: View<'_>,
    opts: IfConvOpts<'_>,
    alloc: A,
) -> Result<IfConvOutput<A>, SimplifyError> {
    let mut cur = view.to_func_in(alloc);
    let mut stats = IfConvStats::default();
    let mut tracked_taken: Vec<u16, A> = if opts.profile.taken_permille_by_block.is_empty() {
        Vec::new_in(alloc)
    } else {
        let blocks = cur.view().blocks();
        let mut buf: Vec<u16, A> = Vec::with_capacity_in(blocks, alloc);
        for b in 0..blocks {
            let bias = opts
                .profile
                .taken_permille_by_block
                .get(b)
                .copied()
                .unwrap_or(u16::MAX);
            buf.push(bias);
        }
        buf
    };
    for _ in 0..opts.max_rounds {
        let profile = BranchProfile {
            taken_permille_by_block: &tracked_taken,
        };
        let pat = match find_if_pattern(&cur.view(), opts.target, profile, alloc) {
            FindOutcome::Matched { pat, predictable_preserved } => {
                stats.predictable_branches_preserved = predictable_preserved;
                pat
            }
            FindOutcome::Done { predictable_preserved } => {
                stats.predictable_branches_preserved = predictable_preserved;
                break;
            }
        };
        let applied = apply_if_pattern(&cur.view(), pat, &tracked_taken, alloc)?;
        cur = applied.func;
        tracked_taken = applied.taken_by_block;
        stats.branches_converted += 1;
        stats.reductions_canonicalized += applied.reductions_canonicalized;
    }
    cur = balance_select_chains(cur, alloc)?;
    Ok(IfConvOutput { func: cur, stats })
}

/// Converts same-target branches (`Br(c, (join, t), (join, e))`), pure diamond/triangle branches,
/// and small pure `Op::Switch` dispatches into branchless `Op::Select`s (up to `max_rounds`),
/// canonicalizing conditional reductions and coalescing resulting straight-line blocks.
pub fn if_convert<A: Allocator + Copy>(
    view: View<'_>,
    max_rounds: u32,
    alloc: A,
) -> Result<IfConvOutput<A>, SimplifyError> {
    if_convert_with_opts(
        view,
        IfConvOpts {
            max_rounds,
            target: IfConvTarget::APPLE_M5,
            profile: BranchProfile::default(),
        },
        alloc,
    )
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{Conv, RefKind, SlotKind},
    };

    #[test]
    fn if_convert_triangle_canonicalizes_conditional_reduction_and_matches_eval() {
        // `fn cond_sum(n: u64, thresh: u64) -> u64`:
        //   for i in 0..n { if i > thresh { acc += i; } }
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, hdr, body, then_b, join, exit] = [(); 6].map(|_| b.create_block(BlockKind::Normal));
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
        let func = b.finish_checked().unwrap();

        let out = if_convert(func.view(), 4, TestHeap).unwrap();
        assert_eq!(out.stats.branches_converted, 1);
        assert_eq!(out.stats.reductions_canonicalized, 1);
        // Should collapse `body + then_b + join` into ONE block (`entry`, `hdr`, `body`, `exit` = 4 blocks)!
        assert_eq!(out.func.view().blocks(), 4);

        for &(nv, tv) in &[(0u128, 0u128), (10, 5), (20, 0), (20, 100), (17, 7)] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(nv), Val::Int(tv)], 10_000, TestHeap).unwrap() else {
                panic!("orig eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &[Val::Int(nv), Val::Int(tv)], 10_000, TestHeap).unwrap() else {
                panic!("ifconv eval");
            };
            assert_eq!(a, c, "if_convert mismatch at n={nv}, thresh={tv}");
        }
    }

    #[test]
    fn if_convert_bsearch_diamond_into_multi_pred_loop_header_and_matches_eval() {
        // Exact binary-search loop shape: `h_bs(lo, hi)` has 3 predecessors:
        // preheader (`entry`), `lt_blk` (`mid1 = mid + 1; jump h_bs(mid1, hi)`),
        // and `ge_blk` (`jump h_bs(lo, mid)`).
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let arr_slot = b.slot(16 * 8, 3, SlotKind::Temp);
        let [entry, h_bs, b_bs, lt_blk, ge_blk, exit] = [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let key = b.param(Ty::I64);
        let arr = b.slot_addr(arr_slot);
        for k in 0..16u64 {
            let v = b.iconst(Ty::I64, u128::from(k * 3 + 5));
            b.store(Flags::NONE, v, arr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let n = b.iconst(Ty::I64, 16);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        b.jump((h_bs, &[zero, n]));

        b.begin(h_bs);
        let lo = b.param(Ty::I64);
        let hi = b.param(Ty::I64);
        let kbs = b.icmp(IntCond::Ult, lo, hi);
        b.br(Flags::NONE, kbs, (b_bs, &[]), (exit, &[lo]));

        b.begin(b_bs);
        let diff = b.binary(Op::Sub, Flags::NONE, hi, lo);
        let half = b.binary(Op::Lshr, Flags::NONE, diff, one);
        let mid = b.binary(Op::Add, Flags::NONE, lo, half);
        let m_off = b.binary(Op::Shl, Flags::NONE, mid, three);
        let ap = b.ptr_add(Flags::NONE, arr, m_off);
        let mv = b.load(Ty::I64, Flags::NONE, ap, 0, MemAttrs::aligned(3));
        let is_lt = b.icmp(IntCond::Ult, mv, key);
        b.br(Flags::NONE, is_lt, (lt_blk, &[]), (ge_blk, &[]));

        b.begin(lt_blk);
        let mid1 = b.binary(Op::Add, Flags::NONE, mid, one);
        b.jump((h_bs, &[mid1, hi]));

        b.begin(ge_blk);
        b.jump((h_bs, &[lo, mid]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = if_convert(func.view(), 8, TestHeap).unwrap();
        assert!(out.stats.branches_converted >= 1, "expected bsearch diamond to if-convert: {:?}", out.stats);
        assert!(verify(out.func.view()).is_ok());
        // `lt_blk` and `ge_blk` should both be eliminated (6 -> 4 blocks) and 2 `Op::Select`s emitted!
        assert_eq!(out.func.view().blocks(), 4);
        let selects = (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Select).count();
        assert_eq!(selects, 2);

        for &kv in &[0u128, 4, 5, 6, 8, 20, 21, 50, 51, 999] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(kv)], 10_000, TestHeap).unwrap() else {
                panic!("orig bsearch eval");
            };
            let Outcome::Returned(c) = eval::run_in(out.func.view(), &[Val::Int(kv)], 10_000, TestHeap).unwrap() else {
                panic!("ifconv bsearch eval");
            };
            assert_eq!(a, c, "bsearch if_convert mismatch at key={kv}");
        }
    }

    #[test]
    fn if_convert_diamond_with_arm_params_and_matches_eval() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, t_blk, e_blk, join] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let z = b.param(Ty::I64);
        let c = b.icmp(IntCond::Ult, x, y);
        b.br(Flags::NONE, c, (t_blk, &[x]), (e_blk, &[y]));

        b.begin(t_blk);
        let tp = b.param(Ty::I64);
        let tv = b.binary(Op::Add, Flags::NONE, tp, z);
        b.jump((join, &[tv, z]));

        b.begin(e_blk);
        let ep = b.param(Ty::I64);
        let ev = b.binary(Op::Sub, Flags::NONE, ep, z);
        b.jump((join, &[ev, z]));

        b.begin(join);
        let r0 = b.param(Ty::I64);
        let r1 = b.param(Ty::I64);
        let sum = b.binary(Op::Add, Flags::NONE, r0, r1);
        b.ret(&[sum]);
        let func = b.finish_checked().unwrap();

        let out = if_convert(func.view(), 4, TestHeap).unwrap();
        assert_eq!(out.stats.branches_converted, 1);
        assert_eq!(out.func.view().blocks(), 1);
        // `r1` has identical `z` on both arms -> only 1 `Op::Select` emitted!
        let selects = (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Select).count();
        assert_eq!(selects, 1);

        for &(xv, yv, zv) in &[(1u128, 5u128, 3u128), (10, 2, 4), (7, 7, 9)] {
            let args = [Val::Int(xv), Val::Int(yv), Val::Int(zv)];
            assert_eq!(
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                eval::run_in(out.func.view(), &args, 10_000, TestHeap).unwrap(),
            );
        }
    }

    #[test]
    fn if_convert_dense_pure_switch_bytecode_vm_to_branchless_select_tree() {
        // Exact 8-arm `bytecode_vm` loop shape (`0..=6` + default `7` / out-of-range `>= 8`).
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I8), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let ops_slot = b.slot(16, 0, SlotKind::Temp);
        let [entry, dispatch, decode, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        let arms = [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let first_op = b.param(Ty::I8);
        let len = b.param(Ty::I64);
        let init_acc = b.param(Ty::I64);
        let ops = b.slot_addr(ops_slot);
        b.store(Flags::NONE, first_op, ops, 0, MemAttrs::aligned(0));
        for k in 0..13u128 {
            let opc = b.iconst(Ty::I8, k);
            b.store(Flags::NONE, opc, ops, (k as i32) + 1, MemAttrs::aligned(0));
        }
        let zero = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c2 = b.iconst(Ty::I64, 2);
        let c3 = b.iconst(Ty::I64, 3);
        let c5 = b.iconst(Ty::I64, 5);
        let c7 = b.iconst(Ty::I64, 7);
        let c11 = b.iconst(Ty::I64, 11);
        let c13 = b.iconst(Ty::I64, 13);
        let mask_xor = b.iconst(Ty::I64, 0x5a5a);
        let mask_and = b.iconst(Ty::I64, 0xffff);
        b.jump((dispatch, &[zero, init_acc]));

        b.begin(dispatch);
        let pc = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, pc, len);
        b.br(Flags::NONE, keep, (decode, &[]), (exit, &[acc]));

        b.begin(decode);
        let p = b.ptr_add(Flags::NONE, ops, pc);
        let op = b.load(Ty::I8, Flags::NONE, p, 0, MemAttrs::aligned(0));
        let pc2 = b.binary(Op::Add, Flags::NONE, pc, c1);
        b.switch(
            op,
            (arms[7], &[]),
            &[
                (0, (arms[0], &[])),
                (1, (arms[1], &[])),
                (2, (arms[2], &[])),
                (3, (arms[3], &[])),
                (4, (arms[4], &[])),
                (5, (arms[5], &[])),
                (6, (arms[6], &[])),
            ],
        );

        b.begin(arms[0]);
        let a0 = b.binary(Op::Add, Flags::NONE, acc, c1);
        b.jump((dispatch, &[pc2, a0]));

        b.begin(arms[1]);
        let a1 = b.binary(Op::Sub, Flags::NONE, acc, c3);
        b.jump((dispatch, &[pc2, a1]));

        b.begin(arms[2]);
        let a2 = b.binary(Op::Mul, Flags::NONE, acc, c5);
        b.jump((dispatch, &[pc2, a2]));

        b.begin(arms[3]);
        let a3 = b.binary(Op::Xor, Flags::NONE, acc, mask_xor);
        b.jump((dispatch, &[pc2, a3]));

        b.begin(arms[4]);
        let sh4 = b.binary(Op::Shl, Flags::NONE, acc, c2);
        let a4 = b.binary(Op::Add, Flags::NONE, sh4, c7);
        b.jump((dispatch, &[pc2, a4]));

        b.begin(arms[5]);
        let sh5 = b.binary(Op::Lshr, Flags::NONE, acc, c1);
        let a5 = b.binary(Op::Or, Flags::NONE, sh5, c11);
        b.jump((dispatch, &[pc2, a5]));

        b.begin(arms[6]);
        let an6 = b.binary(Op::And, Flags::NONE, acc, mask_and);
        let a6 = b.binary(Op::Add, Flags::NONE, an6, c13);
        b.jump((dispatch, &[pc2, a6]));

        b.begin(arms[7]);
        let sh7 = b.binary(Op::Lshr, Flags::NONE, acc, c3);
        let a7 = b.binary(Op::Xor, Flags::NONE, acc, sh7);
        b.jump((dispatch, &[pc2, a7]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = if_convert(func.view(), 8, TestHeap).unwrap();
        assert!(out.stats.branches_converted >= 1, "expected bytecode_vm switch to if-convert: {:?}", out.stats);
        assert!(verify(out.func.view()).is_ok());
        let switches = (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Switch).count();
        assert_eq!(switches, 0, "expected 0 Op::Switch remaining");
        // Balanced depth-3 binary tree for 8 distinct arms -> exactly 7 `Op::Select`s (`pc2` is identical).
        let selects = (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Select).count();
        assert_eq!(selects, 7, "expected 7 selects in depth-3 tree");
        assert_eq!(out.func.view().blocks(), 4, "expected only entry/dispatch/decode/exit blocks");

        // Verify exact equivalence across every opcode `0..=12` both in isolation (`len=1`) and full loop (`len=14`).
        for opv in 0u128..=12 {
            for &(len_v, acc_v) in &[(1u128, 0x1234_5678u128), (14u128, 42u128)] {
                let args = [Val::Int(opv), Val::Int(len_v), Val::Int(acc_v)];
                let Outcome::Returned(orig) = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap() else {
                    panic!("orig bytecode_vm eval failed");
                };
                let Outcome::Returned(conv) = eval::run_in(out.func.view(), &args, 50_000, TestHeap).unwrap() else {
                    panic!("ifconv bytecode_vm eval failed");
                };
                assert_eq!(orig, conv, "bytecode_vm mismatch at op={opv}, len={len_v}");
            }
        }
    }

    #[test]
    fn if_convert_skips_switch_with_memory_or_call_arms() {
        // 1. Switch arm with `Op::Load` must NOT be if-converted.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I8), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let slot = b.slot(8, 3, SlotKind::Temp);
            let [entry, a0, a1, adef, join] = [(); 5].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let op = b.param(Ty::I8);
            let acc = b.param(Ty::I64);
            let ptr = b.slot_addr(slot);
            b.store(Flags::NONE, acc, ptr, 0, MemAttrs::aligned(3));
            let c1 = b.iconst(Ty::I64, 1);
            b.switch(op, (adef, &[]), &[(0, (a0, &[])), (1, (a1, &[]))]);
            b.begin(a0);
            let loaded = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(3));
            b.jump((join, &[loaded]));
            b.begin(a1);
            let v1 = b.binary(Op::Add, Flags::NONE, acc, c1);
            b.jump((join, &[v1]));
            b.begin(adef);
            b.jump((join, &[acc]));
            b.begin(join);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = if_convert(func.view(), 4, TestHeap).unwrap();
            assert_eq!(out.stats.branches_converted, 0);
            let switches = (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Switch).count();
            assert_eq!(switches, 1);
        }

        // 2. Switch arm with `Op::Store` must NOT be if-converted.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I8), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let slot = b.slot(8, 3, SlotKind::Temp);
            let [entry, a0, a1, adef, join] = [(); 5].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let op = b.param(Ty::I8);
            let acc = b.param(Ty::I64);
            let ptr = b.slot_addr(slot);
            let c1 = b.iconst(Ty::I64, 1);
            b.switch(op, (adef, &[]), &[(0, (a0, &[])), (1, (a1, &[]))]);
            b.begin(a0);
            b.store(Flags::NONE, acc, ptr, 0, MemAttrs::aligned(3));
            b.jump((join, &[acc]));
            b.begin(a1);
            let v1 = b.binary(Op::Add, Flags::NONE, acc, c1);
            b.jump((join, &[v1]));
            b.begin(adef);
            b.jump((join, &[acc]));
            b.begin(join);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = if_convert(func.view(), 4, TestHeap).unwrap();
            assert_eq!(out.stats.branches_converted, 0);
            let switches = (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Switch).count();
            assert_eq!(switches, 1);
        }

        // 3. Switch arm with `Op::Call` must NOT be if-converted.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I8), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let callee = b.reference(RefKind::Func, 0);
            b.set_signature(sig);
            let [entry, a0, a1, adef, join] = [(); 5].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let op = b.param(Ty::I8);
            let acc = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            b.switch(op, (adef, &[]), &[(0, (a0, &[])), (1, (a1, &[]))]);
            b.begin(a0);
            let called = b.call(Flags::NONE, callee, callee_sig, &[acc]);
            b.jump((join, &[called]));
            b.begin(a1);
            let v1 = b.binary(Op::Add, Flags::NONE, acc, c1);
            b.jump((join, &[v1]));
            b.begin(adef);
            b.jump((join, &[acc]));
            b.begin(join);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = if_convert(func.view(), 4, TestHeap).unwrap();
            assert_eq!(out.stats.branches_converted, 0);
            let switches = (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Switch).count();
            assert_eq!(switches, 1);
        }
    }

    #[test]
    fn if_convert_dense_8_case_and_sparse_switch_match_eval() {
        // 1. Dense `m == 8` (`0..=7` in scrambled order) + distinct default for `>= 8`.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, join] = [(); 2].map(|_| b.create_block(BlockKind::Normal));
            let arms = [(); 9].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let op = b.param(Ty::I32);
            let x = b.param(Ty::I64);
            b.switch(
                op,
                (arms[8], &[]),
                &[
                    (7, (arms[7], &[])),
                    (2, (arms[2], &[])),
                    (0, (arms[0], &[])),
                    (5, (arms[5], &[])),
                    (1, (arms[1], &[])),
                    (6, (arms[6], &[])),
                    (3, (arms[3], &[])),
                    (4, (arms[4], &[])),
                ],
            );
            for (idx, &arm) in arms.iter().enumerate() {
                b.begin(arm);
                let k = b.iconst(Ty::I64, (idx as u128 + 1) * 10);
                let v = b.binary(Op::Add, Flags::NONE, x, k);
                b.jump((join, &[v]));
            }
            b.begin(join);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = if_convert(func.view(), 4, TestHeap).unwrap();
            assert_eq!(out.stats.branches_converted, 1);
            assert!(verify(out.func.view()).is_ok());
            assert_eq!(out.func.view().blocks(), 1);
            for &ov in &[0u128, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, u128::from(u32::MAX)] {
                let args = [Val::Int(ov), Val::Int(1000)];
                assert_eq!(
                    eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                    eval::run_in(out.func.view(), &args, 10_000, TestHeap).unwrap(),
                    "dense m=8 mismatch at op={ov}",
                );
            }
        }

        // 2. Sparse `I64` switch with wide constant + deduplicated shared arm.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, a_shared, a_wide, a_def, join] = [(); 5].map(|_| b.create_block(BlockKind::Normal));
            let wide_k = 0x1_0000_0007u128;

            b.begin(entry);
            let op = b.param(Ty::I64);
            let x = b.param(Ty::I64);
            let c7 = b.iconst(Ty::I64, 7);
            let c99 = b.iconst(Ty::I64, 99);
            b.switch(
                op,
                (a_def, &[]),
                &[(10, (a_shared, &[])), (42, (a_shared, &[])), (wide_k, (a_wide, &[]))],
            );

            b.begin(a_shared);
            let vs = b.binary(Op::Add, Flags::NONE, x, c7);
            b.jump((join, &[vs]));

            b.begin(a_wide);
            let vw = b.binary(Op::Xor, Flags::NONE, x, c99);
            b.jump((join, &[vw]));

            b.begin(a_def);
            let vd = b.binary(Op::Sub, Flags::NONE, x, c7);
            b.jump((join, &[vd]));

            b.begin(join);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = if_convert(func.view(), 4, TestHeap).unwrap();
            assert_eq!(out.stats.branches_converted, 1);
            assert!(verify(out.func.view()).is_ok());
            assert_eq!(out.func.view().blocks(), 1);
            for &ov in &[0u128, 10, 41, 42, 43, wide_k, wide_k + 1, u128::from(u64::MAX)] {
                let args = [Val::Int(ov), Val::Int(500)];
                assert_eq!(
                    eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                    eval::run_in(out.func.view(), &args, 10_000, TestHeap).unwrap(),
                    "sparse I64 mismatch at op={ov}",
                );
            }
        }
    }

    #[test]
    fn if_convert_direct_select_same_target_branch_to_select() {
        // 1. Six chained same-target branches (`Br(cond, (join, t_args), (join, e_args))`),
        //    mirroring Zstd repeat-offset resolution after `thread_jumps` + `coalesce_cfg`
        //    folds pure `if cond { a } else { b }` arms, including multi-arg joins with both
        //    differing and identical parameters.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [b0, b1, b2, b3, b4, b5, b6] = [(); 7].map(|_| b.create_block(BlockKind::Normal));

            b.begin(b0);
            let raw_off = b.param(Ty::I64);
            let ll = b.param(Ty::I64);
            let r1 = b.param(Ty::I64);
            let r2 = b.param(Ty::I64);
            let r3 = b.param(Ty::I64);
            let k0 = b.iconst(Ty::I64, 0);
            let k1 = b.iconst(Ty::I64, 1);
            let k2 = b.iconst(Ty::I64, 2);
            let k3 = b.iconst(Ty::I64, 3);
            let k7 = b.iconst(Ty::I64, 7);
            let k13 = b.iconst(Ty::I64, 13);
            let raw_m1 = b.binary(Op::Sub, Flags::NONE, raw_off, k1);
            let c0 = b.icmp(IntCond::Eq, ll, k0);
            b.br(Flags::NONE, c0, (b1, &[raw_off, r1]), (b1, &[raw_m1, r1]));

            b.begin(b1);
            let adj = b.param(Ty::I64);
            let cur_r1 = b.param(Ty::I64);
            let c1 = b.icmp(IntCond::Eq, adj, k0);
            b.br(Flags::NONE, c1, (b2, &[cur_r1]), (b2, &[r2]));

            b.begin(b2);
            let cand1 = b.param(Ty::I64);
            let c2 = b.icmp(IntCond::Eq, adj, k1);
            b.br(Flags::NONE, c2, (b3, &[r2]), (b3, &[cand1]));

            b.begin(b3);
            let cand2 = b.param(Ty::I64);
            let c3 = b.icmp(IntCond::Eq, adj, k2);
            b.br(Flags::NONE, c3, (b4, &[r3, cur_r1]), (b4, &[cand2, r2]));

            b.begin(b4);
            let cand3 = b.param(Ty::I64);
            let new_r2 = b.param(Ty::I64);
            let r1_m1 = b.binary(Op::Sub, Flags::NONE, cur_r1, k1);
            let c4 = b.icmp(IntCond::Eq, adj, k3);
            b.br(Flags::NONE, c4, (b5, &[r1_m1, new_r2]), (b5, &[cand3, new_r2]));

            b.begin(b5);
            let rep_off = b.param(Ty::I64);
            let final_r2 = b.param(Ty::I64);
            let lit_off = b.binary(Op::Sub, Flags::NONE, raw_off, k3);
            let c5 = b.icmp(IntCond::Ugt, raw_off, k3);
            b.br(
                Flags::NONE,
                c5,
                (b6, &[lit_off, lit_off, cur_r1, final_r2]),
                (b6, &[rep_off, rep_off, cur_r1, r3]),
            );

            b.begin(b6);
            let off = b.param(Ty::I64);
            let new_r1 = b.param(Ty::I64);
            let next_r2 = b.param(Ty::I64);
            let next_r3 = b.param(Ty::I64);
            let t0 = b.binary(Op::Mul, Flags::NONE, off, k7);
            let t1 = b.binary(Op::Mul, Flags::NONE, new_r1, k13);
            let t2 = b.binary(Op::Xor, Flags::NONE, t0, t1);
            let t3 = b.binary(Op::Mul, Flags::NONE, next_r2, k3);
            let t4 = b.binary(Op::Add, Flags::NONE, t2, t3);
            let out_val = b.binary(Op::Add, Flags::NONE, t4, next_r3);
            b.ret(&[out_val]);

            let func = b.finish_checked().unwrap();
            let out = if_convert(func.view(), 32, TestHeap).unwrap();
            assert!(
                out.stats.branches_converted >= 6,
                "expected all 6 chained same-target branches converted: {:?}",
                out.stats
            );
            assert!(verify(out.func.view()).is_ok());
            let brs = (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Br).count();
            assert_eq!(brs, 0, "expected zero Op::Br remaining after DirectSelect if-conversion");
            assert_eq!(out.func.view().blocks(), 1, "expected all 7 blocks coalesced into 1");
            let selects = (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Select).count();
            assert_eq!(selects, 9, "expected 9 Op::Selects (skipping identical args)");

            for &raw_v in &[0u128, 1, 2, 3, 4, 5, 100] {
                for &ll_v in &[0u128, 1, 7] {
                    let args = [Val::Int(raw_v), Val::Int(ll_v), Val::Int(11), Val::Int(29), Val::Int(71)];
                    assert_eq!(
                        eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                        eval::run_in(out.func.view(), &args, 10_000, TestHeap).unwrap(),
                        "DirectSelect chain mismatch at raw_off={raw_v}, ll={ll_v}",
                    );
                }
            }
        }

        // 2. Loop-latch same-target branch to a multi-predecessor loop header, also verifying
        //    conditional reduction canonicalization (`Select(c, acc + i, acc) -> acc + Select(c, i, 0)`).
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n = b.param(Ty::I64);
            let thresh = b.param(Ty::I64);
            let z = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            b.jump((hdr, &[z, z]));

            b.begin(hdr);
            let i = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, i, n);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let gt = b.icmp(IntCond::Ugt, i, thresh);
            let acc_add = b.binary(Op::Add, Flags::NONE, acc, i);
            let i2 = b.binary(Op::Add, Flags::NONE, i, c1);
            b.br(Flags::NONE, gt, (hdr, &[i2, acc_add]), (hdr, &[i2, acc]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = if_convert(func.view(), 32, TestHeap).unwrap();
            assert_eq!(out.stats.branches_converted, 1);
            assert_eq!(out.stats.reductions_canonicalized, 1);
            assert!(verify(out.func.view()).is_ok());
            assert_eq!(out.func.view().blocks(), 4);
            let brs = (0..out.func.view().len()).filter(|&idx| out.func.view().op(idx) == Op::Br).count();
            assert_eq!(brs, 1, "expected only the loop-exit Br to remain");
            for &(nv, tv) in &[(0u128, 0u128), (10, 5), (20, 0), (20, 100), (17, 7)] {
                let args = [Val::Int(nv), Val::Int(tv)];
                assert_eq!(
                    eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                    eval::run_in(out.func.view(), &args, 10_000, TestHeap).unwrap(),
                    "loop-latch DirectSelect mismatch at n={nv}, thresh={tv}",
                );
            }
        }
    }

    #[test]
    fn balance_select_chain_rebalances_repeat_offset_cascade() {
        // 1. Canonical Zstd repeat-offset cascade:
        //    Select(x == 0, r0, Select(x == 2, r2, Select(x <u 2, r1, r0_m1)))
        //    -> Select(x <u 2, Select(x == 0, r0, r1), Select(x == 2, r2, r0_m1)).
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r0 = b.param(Ty::I64);
            let r1 = b.param(Ty::I64);
            let r2 = b.param(Ty::I64);
            let r0_m1 = b.param(Ty::I64);
            let k0 = b.iconst(Ty::I64, 0);
            let k2 = b.iconst(Ty::I64, 2);
            let c3 = b.icmp(IntCond::Ult, x, k2);
            let s3 = b.select(Flags::NONE, c3, r1, r0_m1);
            let c2 = b.icmp(IntCond::Eq, x, k2);
            let s2 = b.select(Flags::NONE, c2, r2, s3);
            let c1 = b.icmp(IntCond::Eq, x, k0);
            let s1 = b.select(Flags::NONE, c1, r0, s2);
            b.ret(&[s1]);
            let func = b.finish_checked().unwrap();

            let out = if_convert(func.view(), 4, TestHeap).unwrap();
            let v = out.func.view();
            assert!(verify(v).is_ok());
            let selects = (0..v.len()).filter(|&i| v.op(i) == Op::Select).count();
            assert_eq!(selects, 3);

            let ret_inst = v.terminator(Block(0));
            assert_eq!(v.op(ret_inst), Op::Return);
            let ret_at = to_idx(at_u32(v.data, ret_inst));
            let root = to_idx(at_u32(v.extra, ret_at + 1));
            assert_eq!(v.op(root), Op::Select);
            let root_at = to_idx(at_u32(v.data, root));
            let root_cond = at_u32(v.extra, root_at);
            let root_t = to_idx(at_u32(v.extra, root_at + 1));
            let root_f = to_idx(at_u32(v.extra, root_at + 2));

            let root_cmp = extract_const_cmp(&v, root_cond).expect("root select cond should be x <u 2");
            assert_eq!(root_cmp.lhs, x.0);
            assert_eq!(root_cmp.pred, IntCond::Ult);
            assert_eq!(root_cmp.rhs, 2);

            assert_eq!(v.op(root_t), Op::Select);
            let lo_at = to_idx(at_u32(v.data, root_t));
            let lo_cmp = extract_const_cmp(&v, at_u32(v.extra, lo_at)).expect("lo select cond should be x == 0");
            assert_eq!(lo_cmp.lhs, x.0);
            assert_eq!(lo_cmp.pred, IntCond::Eq);
            assert_eq!(lo_cmp.rhs, 0);
            assert_eq!(at_u32(v.extra, lo_at + 1), r0.0);
            assert_eq!(at_u32(v.extra, lo_at + 2), r1.0);

            assert_eq!(v.op(root_f), Op::Select);
            let hi_at = to_idx(at_u32(v.data, root_f));
            let hi_cmp = extract_const_cmp(&v, at_u32(v.extra, hi_at)).expect("hi select cond should be x == 2");
            assert_eq!(hi_cmp.lhs, x.0);
            assert_eq!(hi_cmp.pred, IntCond::Eq);
            assert_eq!(hi_cmp.rhs, 2);
            assert_eq!(at_u32(v.extra, hi_at + 1), r2.0);
            assert_eq!(at_u32(v.extra, hi_at + 2), r0_m1.0);

            for &xv in &[0u128, 1, 2, 3, 4, 100, u128::from(u64::MAX)] {
                let args = [Val::Int(xv), Val::Int(11), Val::Int(22), Val::Int(33), Val::Int(44)];
                assert_eq!(
                    eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                    eval::run_in(v, &args, 10_000, TestHeap).unwrap(),
                    "balanced select mismatch at x={xv}",
                );
            }
        }

        // 2. Inverted-arm + dual polarity:
        //    Select(x != 2, Select(x != 0, Select(x <u 2, c, d), b), a)
        //    -> Select(x <u 2, Select(x != 0, c, b), Select(x != 2, d, a)).
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let c = b.param(Ty::I64);
            let d = b.param(Ty::I64);
            let k0 = b.iconst(Ty::I64, 0);
            let k2 = b.iconst(Ty::I64, 2);
            let c3 = b.icmp(IntCond::Ult, x, k2);
            let s3 = b.select(Flags::NONE, c3, c, d);
            let c2 = b.icmp(IntCond::Ne, x, k0);
            let s2 = b.select(Flags::NONE, c2, s3, bv);
            let c1 = b.icmp(IntCond::Ne, x, k2);
            let s1 = b.select(Flags::NONE, c1, s2, a);
            b.ret(&[s1]);
            let func = b.finish_checked().unwrap();

            let out = if_convert(func.view(), 4, TestHeap).unwrap();
            let v = out.func.view();
            assert!(verify(v).is_ok());
            let selects = (0..v.len()).filter(|&i| v.op(i) == Op::Select).count();
            assert_eq!(selects, 3);

            let ret_inst = v.terminator(Block(0));
            let ret_at = to_idx(at_u32(v.data, ret_inst));
            let root = to_idx(at_u32(v.extra, ret_at + 1));
            assert_eq!(v.op(root), Op::Select);
            let root_at = to_idx(at_u32(v.data, root));
            let root_cmp = extract_const_cmp(&v, at_u32(v.extra, root_at)).expect("root cond should be x <u 2");
            assert_eq!(root_cmp.lhs, x.0);
            assert_eq!(root_cmp.pred, IntCond::Ult);
            assert_eq!(root_cmp.rhs, 2);

            for &xv in &[0u128, 1, 2, 3, 4, 100, u128::from(u64::MAX)] {
                let args = [Val::Int(xv), Val::Int(11), Val::Int(22), Val::Int(33), Val::Int(44)];
                assert_eq!(
                    eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                    eval::run_in(v, &args, 10_000, TestHeap).unwrap(),
                    "inverted dual balanced select mismatch at x={xv}",
                );
            }
        }
    }

    #[test]
    fn balance_select_chain_skips_when_implication_does_not_hold() {
        // `x == 3` implies `!(x <u 2)` and `x == 2` ALSO implies `!(x <u 2)` (same side, not a
        // domain partition), so the cascade MUST remain unchanged.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);
        let c = b.param(Ty::I64);
        let d = b.param(Ty::I64);
        let k2 = b.iconst(Ty::I64, 2);
        let k3 = b.iconst(Ty::I64, 3);
        let c3 = b.icmp(IntCond::Ult, x, k2);
        let s3 = b.select(Flags::NONE, c3, c, d);
        let c2 = b.icmp(IntCond::Eq, x, k2);
        let s2 = b.select(Flags::NONE, c2, bv, s3);
        let c1 = b.icmp(IntCond::Eq, x, k3);
        let s1 = b.select(Flags::NONE, c1, a, s2);
        b.ret(&[s1]);
        let func = b.finish_checked().unwrap();

        let out = if_convert(func.view(), 4, TestHeap).unwrap();
        let v = out.func.view();
        assert!(verify(v).is_ok());
        let selects = (0..v.len()).filter(|&i| v.op(i) == Op::Select).count();
        assert_eq!(selects, 3);

        let ret_inst = v.terminator(Block(0));
        let ret_at = to_idx(at_u32(v.data, ret_inst));
        let root = to_idx(at_u32(v.extra, ret_at + 1));
        assert_eq!(v.op(root), Op::Select);
        let root_at = to_idx(at_u32(v.data, root));
        let root_cmp = extract_const_cmp(&v, at_u32(v.extra, root_at)).expect("root cond should stay x == 3");
        assert_eq!(root_cmp.lhs, x.0);
        assert_eq!(root_cmp.pred, IntCond::Eq);
        assert_eq!(root_cmp.rhs, 3);
        assert_eq!(at_u32(v.extra, root_at + 1), a.0);

        let mid = to_idx(at_u32(v.extra, root_at + 2));
        assert_eq!(v.op(mid), Op::Select);
        let mid_at = to_idx(at_u32(v.data, mid));
        let mid_cmp = extract_const_cmp(&v, at_u32(v.extra, mid_at)).expect("mid cond should stay x == 2");
        assert_eq!(mid_cmp.lhs, x.0);
        assert_eq!(mid_cmp.pred, IntCond::Eq);
        assert_eq!(mid_cmp.rhs, 2);
        assert_eq!(at_u32(v.extra, mid_at + 1), bv.0);

        let inner = to_idx(at_u32(v.extra, mid_at + 2));
        assert_eq!(v.op(inner), Op::Select);
        let inner_at = to_idx(at_u32(v.data, inner));
        let inner_cmp = extract_const_cmp(&v, at_u32(v.extra, inner_at)).expect("inner cond should stay x <u 2");
        assert_eq!(inner_cmp.lhs, x.0);
        assert_eq!(inner_cmp.pred, IntCond::Ult);
        assert_eq!(inner_cmp.rhs, 2);
        assert_eq!(at_u32(v.extra, inner_at + 1), c.0);
        assert_eq!(at_u32(v.extra, inner_at + 2), d.0);

        for &xv in &[0u128, 1, 2, 3, 4, 100] {
            let args = [Val::Int(xv), Val::Int(11), Val::Int(22), Val::Int(33), Val::Int(44)];
            assert_eq!(
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                eval::run_in(v, &args, 10_000, TestHeap).unwrap(),
                "unchanged select chain mismatch at x={xv}",
            );
        }
    }

    #[test]
    fn if_converts_deep_repcode_ladder_ignoring_iconsts() {
        // 3-level nested repcode ladder with live `Op::Iconst` nodes in every arm:
        //   if off == rep1 { ... }
        //   else if off == rep2 { ... }
        //   else if off == rep3 { ... }
        //   else { ... off + 3 ... }
        // After collapsing the inner two levels, the outer else arm holds 8 non-const ops
        // (`<= MAX_ARM_INSTS`) plus 4 live `Iconst`s (12 raw ops, `<= MAX_ARM_RAW_INSTS`),
        // exercising zero-cost constant exclusion so all 3 levels collapse to 0 `Op::Br`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, b_eq1, b_ne1, b_eq2, b_ne2, b_eq3, b_else, join] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let off = b.param(Ty::I64);
        let rep1 = b.param(Ty::I64);
        let rep2 = b.param(Ty::I64);
        let rep3 = b.param(Ty::I64);
        let c1 = b.icmp(IntCond::Eq, off, rep1);
        b.br(Flags::NONE, c1, (b_eq1, &[]), (b_ne1, &[]));

        b.begin(b_eq1);
        let k1 = b.iconst(Ty::I64, 11);
        let v1 = b.binary(Op::Add, Flags::NONE, rep1, k1);
        b.jump((join, &[v1, rep1, rep2, rep3]));

        b.begin(b_ne1);
        let c2 = b.icmp(IntCond::Eq, off, rep2);
        b.br(Flags::NONE, c2, (b_eq2, &[]), (b_ne2, &[]));

        b.begin(b_eq2);
        let k2 = b.iconst(Ty::I64, 21);
        b.jump((join, &[k2, rep2, rep1, rep3]));

        b.begin(b_ne2);
        let c3 = b.icmp(IntCond::Eq, off, rep3);
        b.br(Flags::NONE, c3, (b_eq3, &[]), (b_else, &[]));

        b.begin(b_eq3);
        let k3 = b.iconst(Ty::I64, 31);
        b.jump((join, &[k3, rep3, rep1, rep2]));

        b.begin(b_else);
        let k_off = b.iconst(Ty::I64, 3);
        let lit = b.binary(Op::Add, Flags::NONE, off, k_off);
        let k4 = b.iconst(Ty::I64, 41);
        b.jump((join, &[k4, lit, rep1, rep2]));

        b.begin(join);
        let out_v = b.param(Ty::I64);
        let nr1 = b.param(Ty::I64);
        let nr2 = b.param(Ty::I64);
        let nr3 = b.param(Ty::I64);
        let k7 = b.iconst(Ty::I64, 7);
        let k13 = b.iconst(Ty::I64, 13);
        let k5 = b.iconst(Ty::I64, 5);
        let m0 = b.binary(Op::Mul, Flags::NONE, out_v, k7);
        let m1 = b.binary(Op::Mul, Flags::NONE, nr1, k13);
        let m2 = b.binary(Op::Mul, Flags::NONE, nr2, k5);
        let x0 = b.binary(Op::Xor, Flags::NONE, m0, m1);
        let x1 = b.binary(Op::Add, Flags::NONE, x0, m2);
        let res = b.binary(Op::Add, Flags::NONE, x1, nr3);
        b.ret(&[res]);

        let func = b.finish_checked().unwrap();
        let out = if_convert(func.view(), 16, TestHeap).unwrap();
        let v = out.func.view();
        assert!(verify(v).is_ok());
        assert_eq!(out.stats.branches_converted, 3, "expected all 3 ladder branches converted");
        let brs = (0..v.len()).filter(|&i| v.op(i) == Op::Br).count();
        assert_eq!(brs, 0, "expected 0 Op::Br remaining after constant-excluding if-conversion");
        assert_eq!(v.blocks(), 1, "expected entire ladder collapsed into 1 straight-line block");
        let selects = (0..v.len()).filter(|&i| v.op(i) == Op::Select).count();
        assert!(selects >= 3, "expected branchless Op::Selects, got {selects}");

        for &off_v in &[10u128, 20, 30, 99, 0] {
            let args = [Val::Int(off_v), Val::Int(10), Val::Int(20), Val::Int(30)];
            assert_eq!(
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                eval::run_in(v, &args, 10_000, TestHeap).unwrap(),
                "repcode ladder mismatch at off={off_v}",
            );
        }
    }

    #[test]
    fn if_converts_three_tier_repcode_ten_op_outer_else_to_zero_br() {
        // 3-tier Zstd repcode dispatch (`if off == rep1 { ... } else if off == rep2 { ... }
        // else if off == rep3 { ... } else { ... }`) where:
        // - `t_arm` (`b_eq1`) has 0 non-const ops + 2 `Op::Iconst`s,
        // - after inner if-conversion of the `rep2`/`rep3` tiers, the outer `else` arm (`b_ne1`)
        //   has 10 non-const ops + 6 `Op::Iconst`s (16 raw ops).
        // With `MAX_ARM_INSTS = 12`, `MAX_ARM_RAW_INSTS = 22`, and `MAX_DIAMOND_INSTS = 14`,
        // all 3 tiers convert into straight-line `Op::Select`s with 0 `Op::Br` remaining.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, b_eq1, b_ne1, b_eq2, b_ne2, b_eq3, b_else, join] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let off = b.param(Ty::I64);
        let rep1 = b.param(Ty::I64);
        let rep2 = b.param(Ty::I64);
        let rep3 = b.param(Ty::I64);
        let c1 = b.icmp(IntCond::Eq, off, rep1);
        b.br(Flags::NONE, c1, (b_eq1, &[]), (b_ne1, &[]));

        b.begin(b_eq1);
        let k1 = b.iconst(Ty::I64, 1);
        let k1_tag = b.iconst(Ty::I64, 10);
        b.jump((join, &[k1, rep1, rep2, rep3, k1_tag]));

        b.begin(b_ne1);
        let c2 = b.icmp(IntCond::Eq, off, rep2);
        b.br(Flags::NONE, c2, (b_eq2, &[]), (b_ne2, &[]));

        b.begin(b_eq2);
        let k2 = b.iconst(Ty::I64, 2);
        let k2_tag = b.iconst(Ty::I64, 20);
        b.jump((join, &[k2, rep2, rep1, rep3, k2_tag]));

        b.begin(b_ne2);
        let c3 = b.icmp(IntCond::Eq, off, rep3);
        b.br(Flags::NONE, c3, (b_eq3, &[]), (b_else, &[]));

        b.begin(b_eq3);
        let k3 = b.iconst(Ty::I64, 3);
        let k3_tag = b.iconst(Ty::I64, 30);
        b.jump((join, &[k3, rep3, rep1, rep2, k3_tag]));

        b.begin(b_else);
        let k_off = b.iconst(Ty::I64, 3);
        let lit = b.binary(Op::Add, Flags::NONE, off, k_off);
        let k4_tag = b.iconst(Ty::I64, 40);
        b.jump((join, &[lit, off, rep1, rep2, k4_tag]));

        b.begin(join);
        let out_v = b.param(Ty::I64);
        let nr1 = b.param(Ty::I64);
        let nr2 = b.param(Ty::I64);
        let nr3 = b.param(Ty::I64);
        let tag = b.param(Ty::I64);
        let k7 = b.iconst(Ty::I64, 7);
        let k13 = b.iconst(Ty::I64, 13);
        let k5 = b.iconst(Ty::I64, 5);
        let k17 = b.iconst(Ty::I64, 17);
        let m0 = b.binary(Op::Mul, Flags::NONE, out_v, k7);
        let m1 = b.binary(Op::Mul, Flags::NONE, nr1, k13);
        let m2 = b.binary(Op::Mul, Flags::NONE, nr2, k5);
        let m3 = b.binary(Op::Mul, Flags::NONE, tag, k17);
        let x0 = b.binary(Op::Xor, Flags::NONE, m0, m1);
        let x1 = b.binary(Op::Add, Flags::NONE, x0, m2);
        let x2 = b.binary(Op::Xor, Flags::NONE, x1, m3);
        let res = b.binary(Op::Add, Flags::NONE, x2, nr3);
        b.ret(&[res]);

        let func = b.finish_checked().unwrap();

        // After collapsing the inner 2 tiers, verify `t_arm` has 0 non-const ops + 2 consts
        // and the outer `else` arm has 10 non-const ops + 6 consts (16 raw ops).
        let mid = if_convert(func.view(), 2, TestHeap).unwrap();
        let mv = mid.func.view();
        assert!(verify(mv).is_ok());
        assert_eq!(mid.stats.branches_converted, 2);
        assert_eq!(mv.blocks(), 4);
        let count_arm = |blk: u32| -> (usize, usize) {
            let range = mv.block(Block(blk));
            let params = mv.params(Block(blk));
            let term = range.end - 1;
            let mut non_const = 0usize;
            let mut consts = 0usize;
            for inst in params.end..term {
                if is_zero_cost_const(mv.op(inst)) {
                    consts += 1;
                } else {
                    non_const += 1;
                }
            }
            (non_const, consts)
        };
        assert_eq!(count_arm(1), (0, 2), "t_arm should have 0 non-const ops + 2 consts");
        assert_eq!(count_arm(2), (10, 6), "outer else arm should have 10 non-const ops + 6 consts");

        // Full if-conversion collapses all 3 tiers into 1 block with 0 `Op::Br`.
        let out = if_convert(func.view(), 16, TestHeap).unwrap();
        let v = out.func.view();
        assert!(verify(v).is_ok());
        assert_eq!(out.stats.branches_converted, 3, "expected all 3 repcode branches converted");
        let brs = (0..v.len()).filter(|&i| v.op(i) == Op::Br).count();
        assert_eq!(brs, 0, "expected 0 Op::Br remaining after 3-tier repcode if-conversion");
        assert_eq!(v.blocks(), 1, "expected 3-tier repcode dispatch collapsed into 1 block");
        let selects = (0..v.len()).filter(|&i| v.op(i) == Op::Select).count();
        assert_eq!(selects, 12, "expected 12 Op::Selects across all 3 tiers");

        for &(off_v, r1_v, r2_v, r3_v) in &[
            (10u128, 10u128, 20u128, 30u128),
            (20, 10, 20, 30),
            (30, 10, 20, 30),
            (99, 10, 20, 30),
            (0, 10, 20, 30),
            (10, 10, 10, 30),
            (20, 10, 20, 20),
        ] {
            let args = [Val::Int(off_v), Val::Int(r1_v), Val::Int(r2_v), Val::Int(r3_v)];
            assert_eq!(
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                eval::run_in(v, &args, 10_000, TestHeap).unwrap(),
                "3-tier repcode mismatch at off={off_v}, reps=({r1_v}, {r2_v}, {r3_v})",
            );
        }
    }

    fn build_six_op_diamond() -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, t_blk, e_blk, join] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let z = b.param(Ty::I64);
        let k3 = b.iconst(Ty::I64, 3);
        let k5 = b.iconst(Ty::I64, 5);
        let k7 = b.iconst(Ty::I64, 7);
        let k11 = b.iconst(Ty::I64, 11);
        let cond = b.icmp(IntCond::Ult, x, y);
        b.br(Flags::NONE, cond, (t_blk, &[]), (e_blk, &[]));

        // 3 non-const ops in `t_blk`
        b.begin(t_blk);
        let t0 = b.binary(Op::Add, Flags::NONE, x, z);
        let t1 = b.binary(Op::Mul, Flags::NONE, t0, k3);
        let t2 = b.binary(Op::Xor, Flags::NONE, t1, k5);
        b.jump((join, &[t2]));

        // 3 non-const ops in `e_blk` (6 non-const ops total across the diamond)
        b.begin(e_blk);
        let e0 = b.binary(Op::Sub, Flags::NONE, y, z);
        let e1 = b.binary(Op::Mul, Flags::NONE, e0, k7);
        let e2 = b.binary(Op::Add, Flags::NONE, e1, k11);
        b.jump((join, &[e2]));

        b.begin(join);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn pgo_predictable_guard_stays_branch() {
        let func = build_six_op_diamond();

        // 98/2 predictable guard on Apple M5 stays a branch so TAGE predicts it at 0 speculative cycles.
        let taken_hi = [980u16, u16::MAX, u16::MAX, u16::MAX];
        let out_hi = if_convert_with_opts(
            func.view(),
            IfConvOpts {
                max_rounds: 8,
                target: IfConvTarget::APPLE_M5,
                profile: BranchProfile { taken_permille_by_block: &taken_hi },
            },
            TestHeap,
        )
        .unwrap();
        let v_hi = out_hi.func.view();
        assert!(verify(v_hi).is_ok());
        assert_eq!(out_hi.stats.branches_converted, 0);
        assert_eq!(out_hi.stats.predictable_branches_preserved, 1);
        let brs_hi = (0..v_hi.len()).filter(|&i| v_hi.op(i) == Op::Br).count();
        assert_eq!(brs_hi, 1, "predictable 98/2 guard must remain a branch");

        // Symmetric 2/98 predictable guard on Graviton 5 also stays a branch.
        let taken_lo = [20u16, u16::MAX, u16::MAX, u16::MAX];
        let out_lo = if_convert_with_opts(
            func.view(),
            IfConvOpts {
                max_rounds: 8,
                target: IfConvTarget::GRAVITON5,
                profile: BranchProfile { taken_permille_by_block: &taken_lo },
            },
            TestHeap,
        )
        .unwrap();
        let v_lo = out_lo.func.view();
        assert!(verify(v_lo).is_ok());
        assert_eq!(out_lo.stats.branches_converted, 0);
        assert_eq!(out_lo.stats.predictable_branches_preserved, 1);
        let brs_lo = (0..v_lo.len()).filter(|&i| v_lo.op(i) == Op::Br).count();
        assert_eq!(brs_lo, 1, "predictable 2/98 guard must remain a branch");

        for &(xv, yv, zv) in &[(1u128, 5u128, 3u128), (10, 2, 4), (7, 7, 9)] {
            let args = [Val::Int(xv), Val::Int(yv), Val::Int(zv)];
            let orig = eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap();
            assert_eq!(orig, eval::run_in(v_hi, &args, 10_000, TestHeap).unwrap());
            assert_eq!(orig, eval::run_in(v_lo, &args, 10_000, TestHeap).unwrap());
        }
    }

    #[test]
    fn pgo_unpredictable_diamond_converts_branchless() {
        let func = build_six_op_diamond();
        let taken = [500u16, u16::MAX, u16::MAX, u16::MAX];
        let out = if_convert_with_opts(
            func.view(),
            IfConvOpts {
                max_rounds: 8,
                target: IfConvTarget::APPLE_M5,
                profile: BranchProfile { taken_permille_by_block: &taken },
            },
            TestHeap,
        )
        .unwrap();
        let v = out.func.view();
        assert!(verify(v).is_ok());
        assert_eq!(out.stats.branches_converted, 1);
        assert_eq!(out.stats.predictable_branches_preserved, 0);
        let brs = (0..v.len()).filter(|&i| v.op(i) == Op::Br).count();
        assert_eq!(brs, 0, "unpredictable 50/50 diamond must convert to branchless Select");
        let selects = (0..v.len()).filter(|&i| v.op(i) == Op::Select).count();
        assert_eq!(selects, 1);
        assert_eq!(v.blocks(), 1);

        for &(xv, yv, zv) in &[(1u128, 5u128, 3u128), (10, 2, 4), (7, 7, 9), (0, 1, 99), (100, 50, 17)] {
            let args = [Val::Int(xv), Val::Int(yv), Val::Int(zv)];
            assert_eq!(
                eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                eval::run_in(v, &args, 10_000, TestHeap).unwrap(),
                "pgo unpredictable diamond mismatch at ({xv}, {yv}, {zv})",
            );
        }
    }

    #[test]
    fn pgo_high_entropy_widens_budget_and_remaps_across_rounds() {
        // 1. A 16-op diamond (8 + 8) exceeds the static 14-op diamond budget, but converts under
        //    near-50/50 high-entropy PGO (`250..=750` permille grants +4 diamond budget -> 18).
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [entry, t_blk, e_blk, join] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let z = b.param(Ty::I64);
            let k1 = b.iconst(Ty::I64, 1);
            let k3 = b.iconst(Ty::I64, 3);
            let k5 = b.iconst(Ty::I64, 5);
            let k7 = b.iconst(Ty::I64, 7);
            let cond = b.icmp(IntCond::Ult, x, y);
            b.br(Flags::NONE, cond, (t_blk, &[]), (e_blk, &[]));

            b.begin(t_blk);
            let t0 = b.binary(Op::Add, Flags::NONE, x, z);
            let t1 = b.binary(Op::Mul, Flags::NONE, t0, k3);
            let t2 = b.binary(Op::Xor, Flags::NONE, t1, k5);
            let t3 = b.binary(Op::Add, Flags::NONE, t2, y);
            let t4 = b.binary(Op::Mul, Flags::NONE, t3, k7);
            let t5 = b.binary(Op::Sub, Flags::NONE, t4, k1);
            let t6 = b.binary(Op::Xor, Flags::NONE, t5, x);
            let t7 = b.binary(Op::Add, Flags::NONE, t6, z);
            b.jump((join, &[t7]));

            b.begin(e_blk);
            let e0 = b.binary(Op::Sub, Flags::NONE, y, z);
            let e1 = b.binary(Op::Mul, Flags::NONE, e0, k5);
            let e2 = b.binary(Op::Add, Flags::NONE, e1, k7);
            let e3 = b.binary(Op::Xor, Flags::NONE, e2, x);
            let e4 = b.binary(Op::Mul, Flags::NONE, e3, k3);
            let e5 = b.binary(Op::Add, Flags::NONE, e4, k1);
            let e6 = b.binary(Op::Sub, Flags::NONE, e5, y);
            let e7 = b.binary(Op::Xor, Flags::NONE, e6, z);
            b.jump((join, &[e7]));

            b.begin(join);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let static_out = if_convert(func.view(), 8, TestHeap).unwrap();
            assert_eq!(static_out.stats.branches_converted, 0);

            let taken = [500u16, u16::MAX, u16::MAX, u16::MAX];
            let pgo_out = if_convert_with_opts(
                func.view(),
                IfConvOpts {
                    max_rounds: 8,
                    target: IfConvTarget::APPLE_M5,
                    profile: BranchProfile { taken_permille_by_block: &taken },
                },
                TestHeap,
            )
            .unwrap();
            let v = pgo_out.func.view();
            assert!(verify(v).is_ok());
            assert_eq!(pgo_out.stats.branches_converted, 1);
            let brs = (0..v.len()).filter(|&i| v.op(i) == Op::Br).count();
            assert_eq!(brs, 0);

            for &(xv, yv, zv) in &[(1u128, 5u128, 3u128), (10, 2, 4), (7, 7, 9)] {
                let args = [Val::Int(xv), Val::Int(yv), Val::Int(zv)];
                assert_eq!(
                    eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                    eval::run_in(v, &args, 10_000, TestHeap).unwrap(),
                );
            }
        }

        // 2. Two sequential 6-op diamonds: block 0 is 50/50 unpredictable, block 3 is 98/2 predictable.
        //    Converting block 0 coalesces block 3 into block 0; remapped profile still preserves block 3.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [entry, t1, e1, mid, t2, e2, exit] = [(); 7].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let z = b.param(Ty::I64);
            let k3 = b.iconst(Ty::I64, 3);
            let k5 = b.iconst(Ty::I64, 5);
            let k7 = b.iconst(Ty::I64, 7);
            let k11 = b.iconst(Ty::I64, 11);
            let c1 = b.icmp(IntCond::Ult, x, y);
            b.br(Flags::NONE, c1, (t1, &[]), (e1, &[]));

            b.begin(t1);
            let a0 = b.binary(Op::Add, Flags::NONE, x, z);
            let a1 = b.binary(Op::Mul, Flags::NONE, a0, k3);
            let a2 = b.binary(Op::Xor, Flags::NONE, a1, k5);
            b.jump((mid, &[a2]));

            b.begin(e1);
            let b0 = b.binary(Op::Sub, Flags::NONE, y, z);
            let b1 = b.binary(Op::Mul, Flags::NONE, b0, k7);
            let b2 = b.binary(Op::Add, Flags::NONE, b1, k11);
            b.jump((mid, &[b2]));

            b.begin(mid);
            let m = b.param(Ty::I64);
            let c2 = b.icmp(IntCond::Ugt, m, z);
            b.br(Flags::NONE, c2, (t2, &[]), (e2, &[]));

            b.begin(t2);
            let p0 = b.binary(Op::Add, Flags::NONE, m, k3);
            let p1 = b.binary(Op::Mul, Flags::NONE, p0, k5);
            let p2 = b.binary(Op::Xor, Flags::NONE, p1, k7);
            b.jump((exit, &[p2]));

            b.begin(e2);
            let q0 = b.binary(Op::Sub, Flags::NONE, m, k11);
            let q1 = b.binary(Op::Mul, Flags::NONE, q0, k3);
            let q2 = b.binary(Op::Add, Flags::NONE, q1, k5);
            b.jump((exit, &[q2]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let taken = [500u16, u16::MAX, u16::MAX, 980, u16::MAX, u16::MAX, u16::MAX];
            let out = if_convert_with_opts(
                func.view(),
                IfConvOpts {
                    max_rounds: 8,
                    target: IfConvTarget::APPLE_M5,
                    profile: BranchProfile { taken_permille_by_block: &taken },
                },
                TestHeap,
            )
            .unwrap();
            let v = out.func.view();
            assert!(verify(v).is_ok());
            assert_eq!(out.stats.branches_converted, 1);
            assert_eq!(out.stats.predictable_branches_preserved, 1);
            let brs = (0..v.len()).filter(|&i| v.op(i) == Op::Br).count();
            assert_eq!(brs, 1, "second predictable diamond must remain a branch after first coalesces");

            for &(xv, yv, zv) in &[(1u128, 5u128, 3u128), (10, 2, 4), (7, 7, 1000)] {
                let args = [Val::Int(xv), Val::Int(yv), Val::Int(zv)];
                assert_eq!(
                    eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap(),
                    eval::run_in(v, &args, 10_000, TestHeap).unwrap(),
                );
            }
        }
    }
}
