//! Compiler-Synthesized AMAC (Asynchronous Memory Access Chaining) — K-Way Interleaved
//! Lockstep Coroutine Prefetching (`interleave_amac_loops`).
//!
//! Transforms a 2-level nested loop executing `Q` independent pointer-chasing / data-dependent
//! queries (such as batched binary search, B-tree traversal, hash-chain lookup, or fixed-step
//! branchless search) into a K-way lockstep interleaved loop (`K = 4` default on ARM64 / Apple
//! Silicon, or `K = 8` for wide DRAM/L3 MSHR saturation) followed by the untouched scalar tail
//! loop for `Q % K` remaining queries:
//!
//! - **Lockstep load-wave scheduling**: groups all `K` lanes' probe address computations first,
//!   issues all `K` parallel `"blaze.prefetch.r1"` (`PRFM PLDL1KEEP`) hints back-to-back, issues
//!   all `K` independent `Op::Load`s back-to-back so the out-of-order engine overlaps all `K`
//!   cache misses across L1/L2 MSHRs, and groups post-load ALU/`Icmp`/`Select` per lane so AArch64
//!   instruction selection fuses `cmp + csel + csinc` with zero NZCV flag clobbering.
//! - **Memory-safe variable-trip masking**: for data-dependent inner loops where lanes finish on
//!   different iterations (e.g. non-power-of-2 binary search where `lo == hi == N`), derives a
//!   guaranteed-valid, aligned, in-bounds fallback address from the active lanes via a branchless
//!   `Op::Select` chain (`safe_addr[k] = select(active[k], addr[k], safe_fallback)`), freezes
//!   finished lanes' state via `select(active[k], next_state[k], cur_state[k])`, and strips
//!   promise flags (`nsw`/`nuw`/`inbounds`) on speculated lane arithmetic so inactive lanes NEVER
//!   dereference out of bounds or trigger UB.
//! - **Zero-overhead uniform / fixed-trip fast path**: detects lane-uniform inner loop conditions
//!   (such as Shar's branchless binary search `while len > 1 { half = len >> 1; ... len -= half; }`
//!   or fixed-depth tree walks) via a bounded dataflow fixpoint, sharing uniform state in a single
//!   register and emitting ZERO `active` masking or fallback selects.
//! - **Full CFG coverage**: supports both `HeaderBody` and post-`rotate_loops` `Rotated` outer
//!   loops (with `Jump` or post-`unswitch_loops` `Br` preheaders), both `HeaderBody` and
//!   post-`if_convert` `SingleBlock` inner loops, arbitrary combinations of associative integer
//!   reductions (`Add`/`Xor`/`Or`/`And`) and provably non-aliasing unit-stride output stores
//!   (`out[q] = res`).
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, `#![no_std]`, deterministic (Rule 25), bounded loops
//! (Rule 18), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
    prefetch::PREFETCH_READ_L1_INTRINSIC,
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    func::{Conv, Ext, IntrinsicId, IntrinsicRec, ParamFactsRec, ParamKind, ParamRec, SigId, SigRec},
    op::{Field, Layout},
    verify,
};

/// Upper bound on group interleaving width `K`.
const MAX_GROUP_WIDTH: usize = 8;
/// Maximum number of outer loop nests transformed in a single pass (bounded progress, Rule 18).
const MAX_NESTS_PER_FUNC: usize = 4;
/// Maximum non-param instructions allowed in `outer_body`.
const MAX_OUTER_BODY_INSTS: usize = 32;
/// Maximum non-param instructions allowed in the inner loop (`inner_hdr` + `inner_body`).
const MAX_INNER_INSTS: usize = 64;
/// Maximum non-param instructions allowed in `outer_latch`.
const MAX_OUTER_LATCH_INSTS: usize = 32;
/// Maximum pure speculable instructions allowed per arm of a branching inner loop.
const MAX_INNER_BRANCH_ARM_INSTS: usize = 12;
/// Maximum constant byte stride supported for pointer-IV outer loops.
const MAX_PTR_STRIDE: u64 = 4096;
/// Maximum associative outer reductions supported in a single AMAC nest.
const MAX_OUTER_REDUCTIONS: usize = 8;

/// Options for [`interleave_amac_loops`] (Rule 83).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AmacOpts {
    /// Target number of independent queries interleaved in lockstep (`K`, clamped to `2..=8`).
    pub group_width: usize,
    /// Whether to emit `"blaze.prefetch.r1"` (`PRFM PLDL1KEEP`) hints before each inner load wave.
    pub emit_prefetch: bool,
    /// Register budget for live inner-loop state (`uniform_params + K * per_lane_live_regs`).
    /// If `group_width` exceeds this budget, `K` steps down cleanly to the largest fitting width `>= 2`.
    pub max_state_regs: usize,
    /// Minimum outer-invariant inner-loop initial bound (e.g. `hi = n` for binary search) required
    /// at runtime to enter variable-trip masked AMAC (`0` = always enter). Fixed-trip / uniform-guard
    /// inner loops have zero active-masking overhead and enter regardless of this threshold.
    pub min_var_trip_bound: u64,
}

impl AmacOpts {
    /// ARM64 / Apple Silicon preset: 4-way lockstep AMAC with L1 read prefetching and a 20-reg
    /// inner-state cap so 4-way binary search (`4 lo + 4 hi + 4 key = 12` regs) stays 100%
    /// frameless in ARM64's 28 GPRs with zero stack spills.
    pub const fn arm64() -> Self {
        Self {
            group_width: 4,
            emit_prefetch: true,
            max_state_regs: 20,
            min_var_trip_bound: 0,
        }
    }

    /// Apple Silicon M-series (128 KB L1d / 16 MB L2) SOTA preset: up to 8-way lockstep AMAC with
    /// L1 read prefetching and `max_state_regs: 18` (leaving 8 GPRs free for shared base pointers,
    /// bounds, `p`, `end`, `acc`, and active mask so total live GPRs stay <= 26 on AArch64 with
    /// zero stack spills: `K = 8` for 2-reg pointer chasing, `K = 6` for 3-reg `bsearch_batch`,
    /// `K = 4` for 4-reg state), entering variable-trip masked AMAC once the searched table exceeds
    /// L1d (`>= 65_536` elements / 512 KB).
    pub const fn apple_silicon() -> Self {
        Self {
            group_width: 8,
            emit_prefetch: true,
            max_state_regs: 18,
            min_var_trip_bound: 65_536,
        }
    }

    /// Wide 8-way lockstep AMAC preset for large L3/DRAM pointer-chasing workloads with deep MSHRs.
    pub const fn wide_8() -> Self {
        Self {
            group_width: 8,
            emit_prefetch: true,
            max_state_regs: 28,
            min_var_trip_bound: 0,
        }
    }
}

/// Telemetry returned by [`interleave_amac_loops`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct AmacStats {
    pub nests_interleaved: u32,
    pub group_width_used: u32,
    pub prefetches_inserted: u32,
}

/// Output of [`interleave_amac_loops`].
#[derive(Debug)]
pub struct AmacOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: AmacStats,
}

// ---------------------------------------------------------------------------
// Ratchet-clean low-level column & encoding helpers
// ---------------------------------------------------------------------------

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
fn at_u32(s: &[u32], i: usize) -> u32 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u16(s: &[u16], i: usize) -> u16 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u8(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_bool(s: &[bool], i: usize) -> bool {
    s.get(i).copied().unwrap_or(false)
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

#[inline]
const fn block_kind_byte(kind: BlockKind) -> u8 {
    match kind {
        BlockKind::Normal => 0,
        BlockKind::Cold => 1,
        BlockKind::Cleanup => 2,
        BlockKind::Catch => 3,
        BlockKind::Terminate => 4,
    }
}

#[inline]
const fn int_cond_word(c: IntCond) -> u32 {
    match c {
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

#[inline]
fn conv_byte(c: Conv) -> u8 {
    Conv::ALL.iter().position(|&x| x == c).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn ext_byte(e: Ext) -> u8 {
    Ext::ALL.iter().position(|&x| x == e).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn param_kind_byte(k: ParamKind) -> u8 {
    ParamKind::ALL.iter().position(|&x| x == k).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = Ty(at_u16(view.tys, inst));
    if !ty.is_scalar() || !ty.is_int() || ty.bits(64) > 64 {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(at_u32(view.data, inst), width),
        Op::IconstWide => {
            let at = to_idx(at_u32(view.data, inst));
            let end = at.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(at..end)?, width)
        }
        _ => return None,
    };
    u64::try_from(v).ok()
}

fn push_inst<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, data: u32, attr: u32) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(flags.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(0);
    idx
}

fn push_param<A: Allocator>(func: &mut Func<A>, ty: Ty) -> u32 {
    push_inst(func, Op::Param, Flags::NONE, ty, 0, 0)
}

fn push_iconst<A: Allocator>(func: &mut Func<A>, ty: Ty, val: u64) -> u32 {
    let bits = ty.bits(64);
    if bits < 32 {
        let masked = trunc_u32(val & ((1u64 << bits) - 1));
        push_inst(func, Op::Iconst, Flags::NONE, ty, masked, 0)
    } else if bits == 32 {
        push_inst(func, Op::Iconst, Flags::NONE, ty, trunc_u32(val), 0)
    } else if let Ok(v32) = i32::try_from(val.cast_signed()) {
        push_inst(func, Op::Iconst, Flags::NONE, ty, v32.cast_unsigned(), 0)
    } else {
        let at = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        if bits == 128 {
            func.extra.push(0);
            func.extra.push(0);
        }
        push_inst(func, Op::IconstWide, Flags::NONE, ty, at, 0)
    }
}

fn push_dummy_of_ty<A: Allocator>(func: &mut Func<A>, ty: Ty) -> Option<u32> {
    if !ty.is_scalar() {
        return None;
    }
    if ty.is_int() {
        Some(push_iconst(func, ty, 0))
    } else if ty == Ty::PTR {
        Some(push_inst(func, Op::Null, Flags::NONE, Ty::PTR, 0, 0))
    } else if ty == Ty::F16 || ty == Ty::F32 {
        Some(push_inst(func, Op::Fconst, Flags::NONE, ty, 0, 0))
    } else if ty == Ty::F64 {
        let at = to_u32(func.extra.len());
        func.extra.push(0);
        func.extra.push(0);
        Some(push_inst(func, Op::FconstWide, Flags::NONE, Ty::F64, at, 0))
    } else {
        None
    }
}

#[inline]
fn is_latchable_scalar_ty(ty: Ty) -> bool {
    ty.is_scalar() && (ty.is_int() || ty == Ty::PTR || ty == Ty::F16 || ty == Ty::F32 || ty == Ty::F64)
}

fn push_unary<A: Allocator>(func: &mut Func<A>, op: Op, ty: Ty, a: u32) -> u32 {
    push_inst(func, op, Flags::NONE, ty, a, 0)
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, at, 0)
}

fn push_icmp<A: Allocator>(func: &mut Func<A>, cond: IntCond, a: u32, b: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Icmp, Flags::NONE, Ty::I1, at, 0)
}

fn push_select<A: Allocator>(func: &mut Func<A>, ty: Ty, cond: u32, a: u32, b: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(cond);
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Select, Flags::NONE, ty, at, 0)
}

fn push_load<A: Allocator>(func: &mut Func<A>, ty: Ty, ptr: u32, off: i32, attr: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(ptr);
    func.extra.push(off.cast_unsigned());
    push_inst(func, Op::Load, Flags::NONE, ty, at, attr)
}

fn push_prefetch<A: Allocator>(func: &mut Func<A>, intrinsic: IntrinsicId, ptr: u32) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(intrinsic.0);
    func.extra.push(1);
    func.extra.push(ptr);
    push_inst(func, Op::TargetIntrinsic, Flags::NONE, Ty::NONE, at, 0)
}

fn push_jump<A: Allocator>(func: &mut Func<A>, target: u32, args: &[u32]) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len()));
    for &a in args {
        func.extra.push(a);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, at, 0)
}

fn push_br<A: Allocator>(
    func: &mut Func<A>,
    cond: u32,
    t_blk: u32,
    t_args: &[u32],
    e_blk: u32,
    e_args: &[u32],
) -> u32 {
    let at = to_u32(func.extra.len());
    func.extra.push(cond);
    func.extra.push(t_blk);
    func.extra.push(to_u32(t_args.len()));
    for &a in t_args {
        func.extra.push(a);
    }
    func.extra.push(e_blk);
    func.extra.push(to_u32(e_args.len()));
    for &a in e_args {
        func.extra.push(a);
    }
    push_inst(func, Op::Br, Flags::NONE, Ty::NONE, at, 0)
}

fn begin_block<A: Allocator>(func: &mut Func<A>, kind: BlockKind) -> u32 {
    let id = to_u32(func.block_starts.len());
    func.block_starts.push(to_u32(func.ops.len()));
    func.block_kinds.push(block_kind_byte(kind));
    id
}

fn clear_code_columns<A: Allocator>(func: &mut Func<A>) {
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();
}

/// Ensures `"blaze.prefetch.r1"` with signature `(ptr) -> ()` exists in `func.intrinsics`,
/// preserving canonical CSR tiling of `sigs`/`sig_params` and `intrinsics`/`names`.
fn ensure_prefetch_intrinsic<A: Allocator>(func: &mut Func<A>) -> IntrinsicId {
    let want = PREFETCH_READ_L1_INTRINSIC.as_bytes();
    for (row, rec) in func.intrinsics.iter().enumerate() {
        let start = to_idx(rec.name_first);
        let Some(end) = start.checked_add(to_idx(rec.name_len)) else {
            continue;
        };
        if func.names.get(start..end) == Some(want)
            && let Some(&sig) = func.sigs.get(to_idx(rec.sig))
            && sig.params == 1
            && sig.rets == 0
            && sig.bits == 0
            && func.sig_params.get(to_idx(sig.first)).map(|p| p.ty) == Some(Ty::PTR.0)
        {
            return IntrinsicId(to_u32(row));
        }
    }

    let c_conv = conv_byte(Conv::C);
    let no_ext = ext_byte(Ext::None);
    let val_kind = param_kind_byte(ParamKind::Value);

    let existing_sig = func.sigs.iter().enumerate().find_map(|(r, s)| {
        (s.conv == c_conv
            && s.bits == 0
            && s.params == 1
            && s.rets == 0
            && func.sig_params.get(to_idx(s.first)).copied()
                == Some(ParamRec {
                    facts: 0,
                    ty: Ty::PTR.0,
                    ext: no_ext,
                    kind: val_kind,
                }))
        .then_some(to_u32(r))
    });
    let sig_row = match existing_sig {
        Some(row) => row,
        None => {
            let first = to_u32(func.sig_params.len());
            func.sig_params.push(ParamRec {
                facts: 0,
                ty: Ty::PTR.0,
                ext: no_ext,
                kind: val_kind,
            });
            let row = to_u32(func.sigs.len());
            func.sigs.push(SigRec {
                first,
                params: 1,
                rets: 0,
                conv: c_conv,
                bits: 0,
                pad: [0; 2],
            });
            row
        }
    };

    let name_first = to_u32(func.names.len());
    func.names.extend_from_slice(want);
    let id = to_u32(func.intrinsics.len());
    func.intrinsics.push(IntrinsicRec {
        name_first,
        name_len: to_u32(want.len()),
        sig: sig_row,
    });
    IntrinsicId(id)
}

#[inline]
fn is_pure_speculable_op(view: &View<'_>, inst: usize) -> bool {
    let op = view.op(inst);
    let flags = Flags(at_u8(view.flags, inst));
    if op.info().flags.meanings_only(flags).0 != 0 {
        return false;
    }
    let ty = Ty(at_u16(view.tys, inst));
    if ty.is_vector() || ty.compound_row().is_some() {
        return false;
    }
    match op {
        Op::Iconst
        | Op::IconstWide
        | Op::Fconst
        | Op::FconstWide
        | Op::Null
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
        | Op::FptoUi
        | Op::FptoSi
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
        | Op::FromExposed => true,
        // Unsigned division/remainder by a compile-time non-zero constant is 100% trap-free.
        Op::Udiv | Op::Urem => {
            let at = to_idx(at_u32(view.data, inst));
            let rhs = at_u32(view.extra, at + 1);
            const_u64(view, to_idx(rhs)).is_some_and(|c| c != 0)
        }
        _ => false,
    }
}

#[inline]
fn is_nonvolatile_scalar_load(view: &View<'_>, inst: usize) -> bool {
    if view.op(inst) != Op::Load {
        return false;
    }
    let flags = Flags(at_u8(view.flags, inst));
    if Op::Load.info().flags.meanings_only(flags).0 != 0 || flags.contains(Flags::VOLATILE) {
        return false;
    }
    let ty = Ty(at_u16(view.tys, inst));
    ty.is_scalar() && !ty.is_none()
}

#[inline]
fn is_nonvolatile_scalar_store(view: &View<'_>, inst: usize) -> bool {
    if view.op(inst) != Op::Store {
        return false;
    }
    let flags = Flags(at_u8(view.flags, inst));
    if Op::Store.info().flags.meanings_only(flags).0 != 0 || flags.contains(Flags::VOLATILE) {
        return false;
    }
    let at = to_idx(at_u32(view.data, inst));
    let val = at_u32(view.extra, at);
    let ty = Ty(at_u16(view.tys, to_idx(val)));
    ty.is_scalar() && !ty.is_none()
}

#[inline]
fn is_assoc_int_reduction_op(op: Op, ty: Ty) -> bool {
    ty.is_scalar() && ty.is_int() && matches!(op, Op::Add | Op::Xor | Op::Or | Op::And)
}

fn for_each_value_use(view: &View<'_>, inst: usize, mut f: impl FnMut(u32)) {
    each(view, inst, |item| match item {
        Item::Value(v) => f(v),
        Item::Edge(edge) => {
            if let Some((start, count)) = edge.args {
                for k in 0..count {
                    f(at_u32(view.extra, start + k));
                }
            }
        }
    });
}

/// Strips up to 8 nested `Op::PtrAdd` operations to recover a pointer's root object.
fn ptr_root(view: &View<'_>, mut ptr: u32) -> u32 {
    for _ in 0..8 {
        let pi = to_idx(ptr);
        if view.op(pi) == Op::PtrAdd {
            ptr = at_u32(view.extra, to_idx(at_u32(view.data, pi)));
        } else {
            break;
        }
    }
    ptr
}

/// True iff pointer roots `a` and `b` are provably non-aliasing:
/// - distinct local stack slots (`Op::SlotAddr`),
/// - one local stack slot (`Op::SlotAddr`) and one entry-block parameter (`Op::Param`), or
/// - distinct entry-block parameters where at least one carries `ParamFactsRec::NOALIAS`.
fn provably_noalias_bases(view: &View<'_>, a: u32, b: u32) -> bool {
    let ra = ptr_root(view, a);
    let rb = ptr_root(view, b);
    if ra == rb {
        return false;
    }
    let ai = to_idx(ra);
    let bi = to_idx(rb);
    let a_slot = view.op(ai) == Op::SlotAddr;
    let b_slot = view.op(bi) == Op::SlotAddr;
    if a_slot && b_slot {
        return at_u32(view.data, ai) != at_u32(view.data, bi);
    }
    let entry_params = view.params(Block(0));
    let a_param = view.op(ai) == Op::Param && ai >= entry_params.start && ai < entry_params.end;
    let b_param = view.op(bi) == Op::Param && bi >= entry_params.start && bi < entry_params.end;
    if (a_slot && b_param) || (b_slot && a_param) {
        return true;
    }
    if a_param && b_param {
        let sig = view.sig(SigId(view.head.sig));
        let pa = ai - entry_params.start;
        let pb = bi - entry_params.start;
        let has_noalias = |p_idx: usize| -> bool {
            sig.params
                .get(p_idx)
                .and_then(|p| view.facts(p))
                .is_some_and(|f| (f.bits & ParamFactsRec::NOALIAS) != 0)
        };
        return has_noalias(pa) || has_noalias(pb);
    }
    false
}

/// Checks whether `off_v` is `iv`, `iv << k`, or `iv * c` (for constant `c >= 1`), proving distinct
/// lanes `q + 0..K` write to distinct non-overlapping elements of `out_base`.
fn is_unit_scaled_iv(view: &View<'_>, off_v: u32, iv: u32) -> bool {
    if off_v == iv {
        return true;
    }
    let vi = to_idx(off_v);
    match view.op(vi) {
        Op::Shl => {
            let at = to_idx(at_u32(view.data, vi));
            let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
            lhs == iv && const_u64(view, to_idx(rhs)).is_some_and(|k| k < 64)
        }
        Op::Mul => {
            let at = to_idx(at_u32(view.data, vi));
            let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
            (lhs == iv && const_u64(view, to_idx(rhs)).is_some_and(|c| c >= 1))
                || (rhs == iv && const_u64(view, to_idx(lhs)).is_some_and(|c| c >= 1))
        }
        _ => false,
    }
}

/// Computes immediate dominators (`idom[b]`, `NONE` for unreachable blocks) via Cooper-Harvey-Kennedy.
fn compute_idom<A: Allocator + Copy>(
    view: &View<'_>,
    succs: &[Vec<u32, A>],
    preds: &[Vec<u32, A>],
    alloc: &A,
) -> Vec<u32, A> {
    let blocks = view.blocks();
    let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
    let mut seen = filled(blocks, false, alloc);
    let mut stack: Vec<(u32, usize), A> = Vec::with_capacity_in(blocks, *alloc);
    set_at(&mut seen, 0, true);
    stack.push((0, 0));
    while let Some(top) = stack.last_mut() {
        let (b, next) = *top;
        if let Some(&s) = succs.get(to_idx(b)).and_then(|v| v.get(next)) {
            top.1 += 1;
            let si = to_idx(s);
            if !at_bool(&seen, si) {
                set_at(&mut seen, si, true);
                stack.push((s, 0));
            }
        } else {
            post.push(b);
            stack.pop();
        }
    }

    let mut order = filled(blocks, NONE, alloc);
    let mut rpo: Vec<u32, A> = Vec::with_capacity_in(post.len(), *alloc);
    for (idx, &b) in post.iter().rev().enumerate() {
        set_at(&mut order, to_idx(b), to_u32(idx));
        rpo.push(b);
    }

    let mut idom = filled(blocks, NONE, alloc);
    set_at(&mut idom, 0, 0);
    for _ in 0..2 * blocks + 2 {
        let mut changed = false;
        for &b in rpo.iter().skip(1) {
            let bi = to_idx(b);
            let mut new_idom = NONE;
            if let Some(b_preds) = preds.get(bi) {
                for &p in b_preds.iter() {
                    if at_u32(&idom, to_idx(p)) == NONE {
                        continue;
                    }
                    new_idom = if new_idom == NONE {
                        p
                    } else {
                        let mut a = p;
                        let mut c = new_idom;
                        for _ in 0..2 * blocks + 2 {
                            if a == c {
                                break;
                            }
                            while at_u32(&order, to_idx(a)) > at_u32(&order, to_idx(c)) {
                                a = at_u32(&idom, to_idx(a));
                            }
                            while at_u32(&order, to_idx(c)) > at_u32(&order, to_idx(a)) {
                                c = at_u32(&idom, to_idx(c));
                            }
                        }
                        a
                    };
                }
            }
            if new_idom != at_u32(&idom, bi) {
                set_at(&mut idom, bi, new_idom);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    idom
}

/// Whether reachable block `a` dominates reachable block `b`.
fn dominates(idom: &[u32], a: u32, mut b: u32) -> bool {
    if at_u32(idom, to_idx(a)) == NONE || at_u32(idom, to_idx(b)) == NONE {
        return false;
    }
    for _ in 0..idom.len() {
        if a == b {
            return true;
        }
        if b == 0 {
            return false;
        }
        b = at_u32(idom, to_idx(b));
    }
    false
}

/// Unwraps up to 4 boolean negation/zext wrappers (`Not`, `Xor(_, 1)`, `Icmp(Eq|Ne, i1|zext(i1), 0|1)`).
fn unwrap_bool_cond(view: &View<'_>, mut cond_val: u32) -> (u32, bool) {
    let mut flipped = false;
    for _ in 0..4 {
        let ci = to_idx(cond_val);
        if ci >= view.len() {
            break;
        }
        match view.op(ci) {
            Op::Not => {
                cond_val = at_u32(view.data, ci);
                flipped = !flipped;
            }
            Op::Xor => {
                let pos = to_idx(at_u32(view.data, ci));
                let (l, r) = (at_u32(view.extra, pos), at_u32(view.extra, pos + 1));
                if const_u64(view, to_idx(r)) == Some(1) {
                    cond_val = l;
                    flipped = !flipped;
                } else if const_u64(view, to_idx(l)) == Some(1) {
                    cond_val = r;
                    flipped = !flipped;
                } else {
                    break;
                }
            }
            Op::Icmp => {
                let cat = to_idx(at_u32(view.data, ci));
                let Some(ic) = IntCond::from_word(at_u32(view.extra, cat)) else {
                    break;
                };
                if ic != IntCond::Eq && ic != IntCond::Ne {
                    break;
                }
                let (lhs, rhs) = (at_u32(view.extra, cat + 1), at_u32(view.extra, cat + 2));
                let (val, c) = if let Some(k) = const_u64(view, to_idx(rhs)).filter(|&k| k <= 1) {
                    (lhs, k)
                } else if let Some(k) = const_u64(view, to_idx(lhs)).filter(|&k| k <= 1) {
                    (rhs, k)
                } else {
                    break;
                };
                let vi = to_idx(val);
                let inner_i1 = if Ty(at_u16(view.tys, vi)) == Ty::I1 {
                    val
                } else if view.op(vi) == Op::Zext {
                    let src = at_u32(view.data, vi);
                    if Ty(at_u16(view.tys, to_idx(src))) == Ty::I1 {
                        src
                    } else {
                        break;
                    }
                } else {
                    break;
                };
                let is_inv = (c == 0 && ic == IntCond::Eq) || (c == 1 && ic == IntCond::Ne);
                cond_val = inner_i1;
                if is_inv {
                    flipped = !flipped;
                }
            }
            _ => break,
        }
    }
    (cond_val, flipped)
}

#[inline]
fn unwrap_ptr_addr(view: &View<'_>, v: u32) -> u32 {
    let vi = to_idx(v);
    if view.op(vi) == Op::PtrAddr {
        at_u32(view.data, vi)
    } else {
        v
    }
}

#[inline]
fn is_pure_outer_hdr_helper(op: Op) -> bool {
    matches!(
        op,
        Op::Icmp | Op::PtrAddr | Op::Not | Op::Xor | Op::Zext | Op::Iconst | Op::IconstWide | Op::Assume
    )
}

/// Matches a counted integer loop continuation condition `iv < bound` (unsigned/signed) or
/// `iv != bound` across all comparison/polarity encodings. Returns `(bound, is_signed)`.
fn match_int_iv_guard(
    view: &View<'_>,
    cond_inst: usize,
    iv_val: u32,
    continue_on_then: bool,
) -> Option<(u32, bool)> {
    if view.op(cond_inst) != Op::Icmp {
        return None;
    }
    let iat = to_idx(at_u32(view.data, cond_inst));
    let icond = IntCond::from_word(at_u32(view.extra, iat))?;
    let lhs = at_u32(view.extra, iat + 1);
    let rhs = at_u32(view.extra, iat + 2);
    if continue_on_then {
        match icond {
            IntCond::Ult | IntCond::Ne if lhs == iv_val => Some((rhs, false)),
            IntCond::Slt if lhs == iv_val => Some((rhs, true)),
            IntCond::Ugt | IntCond::Ne if rhs == iv_val => Some((lhs, false)),
            IntCond::Sgt if rhs == iv_val => Some((lhs, true)),
            _ => None,
        }
    } else {
        match icond {
            IntCond::Uge | IntCond::Eq if lhs == iv_val => Some((rhs, false)),
            IntCond::Sge if lhs == iv_val => Some((rhs, true)),
            IntCond::Ule | IntCond::Eq if rhs == iv_val => Some((lhs, false)),
            IntCond::Sle if rhs == iv_val => Some((lhs, true)),
            _ => None,
        }
    }
}

/// Matches a pointer-IV loop continuation condition `p != end` / `p < end` (comparing either raw
/// `Ty::PTR` values or `Op::PtrAddr` integers). Returns `end_ptr`.
fn match_ptr_iv_guard(
    view: &View<'_>,
    cond_inst: usize,
    ptr_val: u32,
    continue_on_then: bool,
) -> Option<u32> {
    if view.op(cond_inst) != Op::Icmp {
        return None;
    }
    let iat = to_idx(at_u32(view.data, cond_inst));
    let icond = IntCond::from_word(at_u32(view.extra, iat))?;
    let ua = unwrap_ptr_addr(view, at_u32(view.extra, iat + 1));
    let ub = unwrap_ptr_addr(view, at_u32(view.extra, iat + 2));
    if continue_on_then {
        match icond {
            IntCond::Ult | IntCond::Ne if ua == ptr_val => Some(ub),
            IntCond::Ugt | IntCond::Ne if ub == ptr_val => Some(ua),
            _ => None,
        }
    } else {
        match icond {
            IntCond::Uge | IntCond::Eq if ua == ptr_val => Some(ub),
            IntCond::Ule | IntCond::Eq if ub == ptr_val => Some(ua),
            _ => None,
        }
    }
}

/// Matches `val == base + 1` or `val == 1 + base`.
fn is_add_one(view: &View<'_>, val: u32, base: u32) -> bool {
    let vi = to_idx(val);
    if view.op(vi) != Op::Add {
        return false;
    }
    let aat = to_idx(at_u32(view.data, vi));
    let (al, ar) = (at_u32(view.extra, aat), at_u32(view.extra, aat + 1));
    (al == base && const_u64(view, to_idx(ar)) == Some(1))
        || (ar == base && const_u64(view, to_idx(al)) == Some(1))
}

/// Matches `val == ptr_add(base, stride)` for a constant byte stride `1..=MAX_PTR_STRIDE`.
fn match_ptr_step(view: &View<'_>, val: u32, base: u32) -> Option<u64> {
    let vi = to_idx(val);
    if view.op(vi) != Op::PtrAdd {
        return None;
    }
    let pat = to_idx(at_u32(view.data, vi));
    let ptr = at_u32(view.extra, pat);
    let off = at_u32(view.extra, pat + 1);
    if ptr != base {
        return None;
    }
    let stride = const_u64(view, to_idx(off))?;
    (1..=MAX_PTR_STRIDE).contains(&stride).then_some(stride)
}

/// Recognizes `bound = PtrAdd[INBOUNDS](iv_init, byte_len)` (the standard Rust `slice::Iter`
/// end-pointer form) with an `i64` byte offset, returning `Some(byte_len)`.
fn inbounds_ptr_byte_offset(view: &View<'_>, iv_init: u32, bound: u32) -> Option<u32> {
    let bi = to_idx(bound);
    if bi >= view.len()
        || view.op(bi) != Op::PtrAdd
        || !Flags(at_u8(view.flags, bi)).contains(Flags::INBOUNDS)
    {
        return None;
    }
    let pat = to_idx(at_u32(view.data, bi));
    let base = at_u32(view.extra, pat);
    let off = at_u32(view.extra, pat + 1);
    if base == iv_init && Ty(at_u16(view.tys, to_idx(off))) == Ty::I64 {
        Some(off)
    } else {
        None
    }
}

/// Picks the largest group width `K` in `2..=min(group_width, MAX_GROUP_WIDTH)` whose inner-loop
/// register footprint (`n_uniform_params + K * per_lane_regs`) fits inside `max_state_regs`,
/// stepping down through even widths (`8 -> 6 -> 4 -> 2`) so contiguous loads pair into `ldp`.
#[inline]
fn select_group_width(
    group_width: usize,
    n_uniform_params: usize,
    per_lane_regs: usize,
    max_state_regs: usize,
) -> usize {
    let max_k = group_width.min(MAX_GROUP_WIDTH);
    if max_k < 2 || per_lane_regs == 0 || n_uniform_params >= max_state_regs {
        return 0;
    }
    let mut cand = max_k;
    while cand >= 2 {
        if n_uniform_params.saturating_add(cand.saturating_mul(per_lane_regs)) <= max_state_regs {
            return cand;
        }
        cand = match cand {
            8 => 6,
            6 => 4,
            4 => 2,
            _ => cand - 1,
        };
    }
    0
}

// ---------------------------------------------------------------------------
// AMAC nest plan types & detection (`detect_amac_nest`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreheaderEdge {
    Jump,
    BrThen,
    BrElse,
}

enum OuterShape<A: Allocator> {
    /// Traditional `outer_hdr(q, acc...) -> outer_body -> inner -> outer_latch -> outer_hdr`.
    HeaderBody {
        outer_hdr: u32,
        outer_body: u32,
        outer_exit: u32,
    },
    /// Post-`rotate_loops` `outer_body(q, acc...) -> inner -> outer_latch -> br(q2 < Q, outer_body, outer_exit)`.
    Rotated {
        outer_hdr: u32,
        outer_exit: u32,
        outer_exit_args: Vec<u32, A>,
    },
}

impl<A: Allocator> OuterShape<A> {
    #[inline]
    fn outer_hdr(&self) -> u32 {
        match *self {
            OuterShape::HeaderBody { outer_hdr, .. } | OuterShape::Rotated { outer_hdr, .. } => outer_hdr,
        }
    }

    #[inline]
    fn outer_body(&self) -> u32 {
        match *self {
            OuterShape::HeaderBody { outer_body, .. } => outer_body,
            OuterShape::Rotated { outer_hdr, .. } => outer_hdr,
        }
    }

    #[inline]
    fn outer_exit(&self) -> u32 {
        match *self {
            OuterShape::HeaderBody { outer_exit, .. } | OuterShape::Rotated { outer_exit, .. } => outer_exit,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OuterIvKind {
    /// Counted integer IV (`Ty::I64` or `Ty::I32`, unit step `+1`).
    Counted { ty: Ty, signed: bool },
    /// Running pointer IV (`Ty::PTR`, constant positive byte stride `1..=MAX_PTR_STRIDE`).
    Pointer { stride: u64 },
}

struct InnerBranch<A: Allocator> {
    inner_hdr: u32,
    inner_body: u32,
    inner_cond: u32,
    inner_cond_inverted: bool,
    branch_cond: u32,
    then_arm: Option<u32>,
    else_arm: Option<u32>,
    merge_blk: Option<u32>,
    then_join_args: Vec<u32, A>,
    else_join_args: Vec<u32, A>,
}

enum InnerShape<A: Allocator> {
    /// Post-`if_convert` / post-`rotate_loops` 1-block do-while inner loop.
    SingleBlock {
        inner_hdr: u32,
        inner_cond: u32,
        inner_cond_inverted: bool,
    },
    /// 2-block `inner_hdr -> inner_body -> inner_hdr` while inner loop.
    HeaderBody {
        inner_hdr: u32,
        inner_body: u32,
        inner_cond: u32,
        inner_cond_inverted: bool,
    },
    /// Branching inner loop: `inner_hdr -> inner_body -> (then_arm / else_arm) -> [merge_blk ->] inner_hdr`.
    HeaderBranch(InnerBranch<A>),
}

impl<A: Allocator> InnerShape<A> {
    #[inline]
    fn inner_hdr(&self) -> u32 {
        match self {
            InnerShape::SingleBlock { inner_hdr, .. }
            | InnerShape::HeaderBody { inner_hdr, .. }
            | InnerShape::HeaderBranch(InnerBranch { inner_hdr, .. }) => *inner_hdr,
        }
    }

    #[inline]
    fn inner_body(&self) -> u32 {
        match self {
            InnerShape::SingleBlock { inner_hdr, .. } => *inner_hdr,
            InnerShape::HeaderBody { inner_body, .. }
            | InnerShape::HeaderBranch(InnerBranch { inner_body, .. }) => *inner_body,
        }
    }

    #[inline]
    fn inner_cond(&self) -> u32 {
        match self {
            InnerShape::SingleBlock { inner_cond, .. }
            | InnerShape::HeaderBody { inner_cond, .. }
            | InnerShape::HeaderBranch(InnerBranch { inner_cond, .. }) => *inner_cond,
        }
    }

    #[inline]
    fn contains_block(&self, b: u32) -> bool {
        match self {
            InnerShape::SingleBlock { inner_hdr, .. } => b == *inner_hdr,
            InnerShape::HeaderBody { inner_hdr, inner_body, .. } => b == *inner_hdr || b == *inner_body,
            InnerShape::HeaderBranch(br) => {
                b == br.inner_hdr
                    || b == br.inner_body
                    || br.then_arm == Some(b)
                    || br.else_arm == Some(b)
                    || br.merge_blk == Some(b)
            }
        }
    }
}

#[derive(Clone, Copy)]
struct OuterReduction {
    param_pos: usize,
    param_inst: u32,
    op: Op,
    ty: Ty,
    update_inst: u32,
    contrib_val: u32,
}

struct AmacPlan<A: Allocator> {
    preheader: u32,
    pre_edge: PreheaderEdge,
    outer_shape: OuterShape<A>,
    outer_latch: u32,
    iv_pos: usize,
    iv_init: u32,
    iv_inst: u32,
    iv_next_inst: u32,
    iv_kind: OuterIvKind,
    bound: u32,
    skip_iv_next_in_ob: bool,
    outer_reductions: Vec<OuterReduction, A>,
    outer_body_insts: Vec<u32, A>,
    outer_body_inv: Vec<bool, A>,
    /// Optional outer-loop-invariant entry guard `(guard_cond, enter_is_then)` from `outer_body`'s
    /// `Op::Br` (one arm enters `inner_hdr`, the other skips directly to `outer_latch`). Evaluated
    /// once in `b_amac_entry` so `b_amac_outer_body` always jumps straight into `b_amac_inner_hdr`.
    inner_entry_guard: Option<(u32, bool)>,
    inner_shape: InnerShape<A>,
    inner_init_args: Vec<u32, A>,
    inner_next_args: Vec<u32, A>,
    inner_exit_args: Vec<u32, A>,
    inner_param_uniform: Vec<bool, A>,
    inner_inst_uniform: Vec<bool, A>,
    inner_is_uniform: bool,
    /// Values defined inside `inner_hdr` directly referenced by `outer_latch` (not via `outer_latch` params).
    inner_direct_uses: Vec<u32, A>,
    /// For non-uniform `SingleBlock` inner loops, exit values that need a latched register across lanes.
    extra_latched: Vec<u32, A>,
    outer_latch_insts: Vec<u32, A>,
    group_width: usize,
}

/// Detects one eligible 2-level pointer-chasing loop nest suitable for K-way AMAC interleaving.
fn detect_amac_nest<A: Allocator + Copy>(
    view: &View<'_>,
    opts: AmacOpts,
    alloc: A,
) -> Option<AmacPlan<A>> {
    let blocks = view.blocks();
    let len = view.len();
    if opts.group_width < 2 || opts.max_state_regs < 2 || view.head.ptr_bits != 64 || blocks < 3 || len < 8 {
        return None;
    }

    let mut inst_block = filled(len, 0u32, &alloc);
    let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        succs.push(Vec::new_in(alloc));
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
                if let Some(sb) = succs.get_mut(b) {
                    sb.push(edge.block);
                }
                if let Some(pb) = preds.get_mut(to_idx(edge.block))
                    && pb.scan_for(&b32).is_none()
                {
                    pb.push(b32);
                }
            }
        });
    }
    let idom = compute_idom(view, &succs, &preds, &alloc);

    for oh in 1..blocks {
        let outer_hdr = to_u32(oh);
        if view.block_kind(Block(outer_hdr)) != BlockKind::Normal || at_u32(&idom, oh) == NONE {
            continue;
        }
        // Require exactly 2 predecessors: one preheader `< outer_hdr` and one latch `> outer_hdr`.
        // (This also guarantees idempotence: our scalar tail loop has 3 predecessors!)
        let (p0, p1) = match preds.get(oh).map(Vec::as_slice) {
            Some(&[p0, p1]) => (p0, p1),
            _ => continue,
        };
        let (preheader, outer_latch) = if p0 < outer_hdr && p1 > outer_hdr {
            (p0, p1)
        } else if p1 < outer_hdr && p0 > outer_hdr {
            (p1, p0)
        } else {
            continue;
        };
        if !dominates(&idom, outer_hdr, outer_latch) || view.block_kind(Block(outer_latch)) != BlockKind::Normal {
            continue;
        }

        // Validate preheader edge into `outer_hdr` (`Jump` or single-arm `Br`).
        let pre_term = view.terminator(Block(preheader));
        let oh_params = view.params(Block(outer_hdr));
        if oh_params.is_empty() || oh_params.len() > MAX_OUTER_REDUCTIONS + 1 {
            continue;
        }
        let (pre_edge, pre_args_at) = match view.op(pre_term) {
            Op::Jump => {
                let jat = to_idx(at_u32(view.data, pre_term));
                if at_u32(view.extra, jat) != outer_hdr
                    || to_idx(at_u32(view.extra, jat + 1)) != oh_params.len()
                {
                    continue;
                }
                (PreheaderEdge::Jump, jat + 2)
            }
            Op::Br => {
                let bat = to_idx(at_u32(view.data, pre_term));
                let t_blk = at_u32(view.extra, bat + 1);
                let t_argc = to_idx(at_u32(view.extra, bat + 2));
                let e_at = bat + 3 + t_argc;
                let e_blk = at_u32(view.extra, e_at);
                let e_argc = to_idx(at_u32(view.extra, e_at + 1));
                if t_blk == outer_hdr && e_blk != outer_hdr && t_argc == oh_params.len() {
                    (PreheaderEdge::BrThen, bat + 3)
                } else if e_blk == outer_hdr && t_blk != outer_hdr && e_argc == oh_params.len() {
                    (PreheaderEdge::BrElse, e_at + 2)
                } else {
                    continue;
                }
            }
            _ => continue,
        };

        // Determine whether outer loop is `HeaderBody` or `Rotated`.
        let ol_term = view.terminator(Block(outer_latch));
        let (outer_shape, outer_back_args_at, outer_cond_inst, outer_continue_on_then) =
            if view.op(ol_term) == Op::Jump {
                // `HeaderBody` outer loop: `outer_hdr` branches to `(outer_body, outer_exit)`,
                // `outer_latch` jumps back to `outer_hdr`.
                let ol_at = to_idx(at_u32(view.data, ol_term));
                if at_u32(view.extra, ol_at) != outer_hdr
                    || to_idx(at_u32(view.extra, ol_at + 1)) != oh_params.len()
                {
                    continue;
                }
                let oh_term = view.terminator(Block(outer_hdr));
                if view.op(oh_term) != Op::Br {
                    continue;
                }
                let bat = to_idx(at_u32(view.data, oh_term));
                let cond_v = to_idx(at_u32(view.extra, bat));
                let t_blk = at_u32(view.extra, bat + 1);
                let t_argc = to_idx(at_u32(view.extra, bat + 2));
                let e_at = bat + 3 + t_argc;
                let e_blk = at_u32(view.extra, e_at);
                let e_argc = to_idx(at_u32(view.extra, e_at + 1));

                // `outer_hdr` may contain only pure guard helpers between its params and terminator.
                if (oh_params.end..oh_term).any(|i| !is_pure_outer_hdr_helper(view.op(i))) {
                    continue;
                }

                let t_is_body = t_blk != outer_hdr
                    && t_blk != outer_latch
                    && t_argc == 0
                    && preds.get(to_idx(t_blk)).map(Vec::as_slice) == Some(&[outer_hdr])
                    && view.params(Block(t_blk)).is_empty()
                    && dominates(&idom, t_blk, outer_latch);
                let e_is_body = e_blk != outer_hdr
                    && e_blk != outer_latch
                    && e_argc == 0
                    && preds.get(to_idx(e_blk)).map(Vec::as_slice) == Some(&[outer_hdr])
                    && view.params(Block(e_blk)).is_empty()
                    && dominates(&idom, e_blk, outer_latch);

                let (outer_body, outer_exit, cont_on_then) = if t_is_body && !e_is_body {
                    (t_blk, e_blk, true)
                } else if e_is_body && !t_is_body {
                    (e_blk, t_blk, false)
                } else {
                    continue;
                };
                if outer_exit == outer_hdr || outer_exit == outer_body || outer_exit == outer_latch {
                    continue;
                }
                if view.block_kind(Block(outer_body)) != BlockKind::Normal {
                    continue;
                }
                (
                    OuterShape::HeaderBody { outer_hdr, outer_body, outer_exit },
                    ol_at + 2,
                    cond_v,
                    cont_on_then,
                )
            } else if view.op(ol_term) == Op::Br {
                // `Rotated` outer loop: `outer_hdr` IS `outer_body`, `outer_latch` ends in
                // `Br(cond, outer_hdr(back_args), outer_exit(exit_args))`.
                let bat = to_idx(at_u32(view.data, ol_term));
                let cond_v = to_idx(at_u32(view.extra, bat));
                let t_blk = at_u32(view.extra, bat + 1);
                let t_argc = to_idx(at_u32(view.extra, bat + 2));
                let e_at = bat + 3 + t_argc;
                let e_blk = at_u32(view.extra, e_at);
                let e_argc = to_idx(at_u32(view.extra, e_at + 1));
                let (back_at, outer_exit, ex_at, ex_argc, cont_on_then) =
                    if t_blk == outer_hdr && e_blk != outer_hdr && t_argc == oh_params.len() {
                        (bat + 3, e_blk, e_at + 2, e_argc, true)
                    } else if e_blk == outer_hdr && t_blk != outer_hdr && e_argc == oh_params.len() {
                        (e_at + 2, t_blk, bat + 3, t_argc, false)
                    } else {
                        continue;
                    };
                if outer_exit == outer_latch || at_u32(&inst_block, cond_v) != outer_latch {
                    continue;
                }
                let mut ex_args: Vec<u32, A> = Vec::with_capacity_in(ex_argc, alloc);
                for k in 0..ex_argc {
                    ex_args.push(at_u32(view.extra, ex_at + k));
                }
                (
                    OuterShape::Rotated { outer_hdr, outer_exit, outer_exit_args: ex_args },
                    back_at,
                    cond_v,
                    cont_on_then,
                )
            } else {
                continue;
            };

        let outer_body = outer_shape.outer_body();

        // Match outer IV: either unit-step counted integer (`q_next = q + 1`, `q < bound` / `q != bound`)
        // or constant-stride pointer IV (`p_next = ptr_add(p, stride)`, `p != end` / `p < end`).
        let (outer_cmp_val, outer_cond_flipped) = unwrap_bool_cond(view, to_u32(outer_cond_inst));
        let outer_cmp_idx = to_idx(outer_cmp_val);
        let eff_outer_cont = outer_continue_on_then ^ outer_cond_flipped;
        let mut found_iv = None;
        for pos in 0..oh_params.len() {
            let p_idx = oh_params.start + pos;
            let p_inst = to_u32(p_idx);
            let p_ty = Ty(at_u16(view.tys, p_idx));
            let back_v = at_u32(view.extra, outer_back_args_at + pos);
            let back_b = at_u32(&inst_block, to_idx(back_v));
            if back_b != outer_latch && back_b != outer_body {
                continue;
            }
            let cmp_val = match outer_shape {
                OuterShape::HeaderBody { .. } => p_inst,
                OuterShape::Rotated { .. } => back_v,
            };
            if (p_ty == Ty::I64 || p_ty == Ty::I32) && is_add_one(view, back_v, p_inst) {
                if let Some((bound, signed_iv)) =
                    match_int_iv_guard(view, outer_cmp_idx, cmp_val, eff_outer_cont)
                {
                    let bound_b = at_u32(&inst_block, to_idx(bound));
                    if Ty(at_u16(view.tys, to_idx(bound))) == p_ty
                        && bound_b < outer_hdr
                        && dominates(&idom, bound_b, outer_hdr)
                    {
                        found_iv = Some((
                            pos,
                            p_inst,
                            back_v,
                            OuterIvKind::Counted { ty: p_ty, signed: signed_iv },
                            bound,
                        ));
                        break;
                    }
                }
            } else if p_ty == Ty::PTR
                && let Some(stride) = match_ptr_step(view, back_v, p_inst)
                && let Some(end_ptr) = match_ptr_iv_guard(view, outer_cmp_idx, cmp_val, eff_outer_cont)
            {
                let end_b = at_u32(&inst_block, to_idx(end_ptr));
                if Ty(at_u16(view.tys, to_idx(end_ptr))) == Ty::PTR
                    && end_b < outer_hdr
                    && dominates(&idom, end_b, outer_hdr)
                {
                    found_iv = Some((
                        pos,
                        p_inst,
                        back_v,
                        OuterIvKind::Pointer { stride },
                        end_ptr,
                    ));
                    break;
                }
            }
        }
        let Some((iv_pos, iv_inst, iv_next_inst, iv_kind, bound)) = found_iv else {
            continue;
        };
        let iv_init = at_u32(view.extra, pre_args_at + iv_pos);

        // Helper: whether a value is strictly outer-loop-invariant and already emitted before `outer_hdr`.
        let is_pre_outer_inv = |v: u32| -> bool {
            let vb = at_u32(&inst_block, to_idx(v));
            vb < outer_hdr && dominates(&idom, vb, outer_hdr)
        };

        // Parse `outer_body`'s transition into the inner loop (`Jump` or outer-invariant-guarded
        // `Br` where one arm enters `inner_hdr` and the other skips directly to `outer_latch`).
        let ob_term = view.terminator(Block(outer_body));
        let ol_params = view.params(Block(outer_latch));
        let (inner_hdr, inner_init_args, inner_entry_guard) = match view.op(ob_term) {
            Op::Jump => {
                let jat = to_idx(at_u32(view.data, ob_term));
                let ih = at_u32(view.extra, jat);
                let argc = to_idx(at_u32(view.extra, jat + 1));
                let mut init: Vec<u32, A> = Vec::with_capacity_in(argc, alloc);
                for k in 0..argc {
                    init.push(at_u32(view.extra, jat + 2 + k));
                }
                (ih, init, None)
            }
            Op::Br => {
                let bat = to_idx(at_u32(view.data, ob_term));
                let gv = at_u32(view.extra, bat);
                let t_blk = at_u32(view.extra, bat + 1);
                let t_argc = to_idx(at_u32(view.extra, bat + 2));
                let e_at = bat + 3 + t_argc;
                let e_blk = at_u32(view.extra, e_at);
                let e_argc = to_idx(at_u32(view.extra, e_at + 1));
                let (ih, i_at, i_argc, s_argc, enter_is_then) =
                    if e_blk == outer_latch && t_blk != outer_latch {
                        (t_blk, bat + 3, t_argc, e_argc, true)
                    } else if t_blk == outer_latch && e_blk != outer_latch {
                        (e_blk, e_at + 2, e_argc, t_argc, false)
                    } else {
                        continue;
                    };
                if s_argc != ol_params.len() {
                    continue;
                }
                let mut init: Vec<u32, A> = Vec::with_capacity_in(i_argc, alloc);
                for k in 0..i_argc {
                    init.push(at_u32(view.extra, i_at + k));
                }
                (ih, init, Some((gv, enter_is_then)))
            }
            _ => continue,
        };

        if inner_hdr == outer_hdr
            || inner_hdr == outer_body
            || inner_hdr == outer_latch
            || view.block_kind(Block(inner_hdr)) != BlockKind::Normal
            || !dominates(&idom, outer_body, inner_hdr)
        {
            continue;
        }
        let ih_params = view.params(Block(inner_hdr));
        if ih_params.is_empty() || inner_init_args.len() != ih_params.len() {
            continue;
        }
        // `inner_hdr` must have 2 predecessors (`[outer_body, inner_latch]`) or 3 predecessors
        // (`[outer_body, back0, back1]` for a branching inner loop jumping back to `inner_hdr`).
        let (inner_back0, inner_back1) = match preds.get(to_idx(inner_hdr)).map(Vec::as_slice) {
            Some(&[a, b]) if a == outer_body => (b, NONE),
            Some(&[a, b]) if b == outer_body => (a, NONE),
            Some(&[a, b, c]) if a == outer_body => (b, c),
            Some(&[a, b, c]) if b == outer_body => (a, c),
            Some(&[a, b, c]) if c == outer_body => (a, b),
            _ => continue,
        };

        // Match `SingleBlock`, `HeaderBody`, or `HeaderBranch` inner loop shape.
        let (inner_shape, inner_next_args, inner_exit_args) = if inner_back1 == NONE && inner_back0 == inner_hdr {
            let ih_term = view.terminator(Block(inner_hdr));
            if view.op(ih_term) != Op::Br {
                continue;
            }
            let bat = to_idx(at_u32(view.data, ih_term));
            let ic = at_u32(view.extra, bat);
            let t_blk = at_u32(view.extra, bat + 1);
            let t_argc = to_idx(at_u32(view.extra, bat + 2));
            let e_at = bat + 3 + t_argc;
            let e_blk = at_u32(view.extra, e_at);
            let e_argc = to_idx(at_u32(view.extra, e_at + 1));
            let (n_at, x_at, inverted) = if t_blk == inner_hdr
                && e_blk == outer_latch
                && t_argc == ih_params.len()
                && e_argc == ol_params.len()
            {
                (bat + 3, e_at + 2, false)
            } else if e_blk == inner_hdr
                && t_blk == outer_latch
                && e_argc == ih_params.len()
                && t_argc == ol_params.len()
            {
                (e_at + 2, bat + 3, true)
            } else {
                continue;
            };
            let mut next_a: Vec<u32, A> = Vec::with_capacity_in(ih_params.len(), alloc);
            for k in 0..ih_params.len() {
                next_a.push(at_u32(view.extra, n_at + k));
            }
            let mut exit_a: Vec<u32, A> = Vec::with_capacity_in(ol_params.len(), alloc);
            for k in 0..ol_params.len() {
                exit_a.push(at_u32(view.extra, x_at + k));
            }
            (
                InnerShape::SingleBlock { inner_hdr, inner_cond: ic, inner_cond_inverted: inverted },
                next_a,
                exit_a,
            )
        } else {
            let ih_term = view.terminator(Block(inner_hdr));
            if view.op(ih_term) != Op::Br {
                continue;
            }
            let bat = to_idx(at_u32(view.data, ih_term));
            let ic = at_u32(view.extra, bat);
            let t_blk = at_u32(view.extra, bat + 1);
            let t_argc = to_idx(at_u32(view.extra, bat + 2));
            let e_at = bat + 3 + t_argc;
            let e_blk = at_u32(view.extra, e_at);
            let e_argc = to_idx(at_u32(view.extra, e_at + 1));
            let (inner_body, x_at, inverted) = if e_blk == outer_latch
                && t_blk != outer_latch
                && t_argc == 0
                && e_argc == ol_params.len()
            {
                (t_blk, e_at + 2, false)
            } else if t_blk == outer_latch
                && e_blk != outer_latch
                && e_argc == 0
                && t_argc == ol_params.len()
            {
                (e_blk, bat + 3, true)
            } else {
                continue;
            };
            if inner_body == outer_hdr
                || inner_body == outer_body
                || inner_body == outer_latch
                || inner_body == inner_hdr
                || view.block_kind(Block(inner_body)) != BlockKind::Normal
                || preds.get(to_idx(inner_body)).map(Vec::as_slice) != Some(&[inner_hdr])
                || !view.params(Block(inner_body)).is_empty()
            {
                continue;
            }
            let mut exit_a: Vec<u32, A> = Vec::with_capacity_in(ol_params.len(), alloc);
            for k in 0..ol_params.len() {
                exit_a.push(at_u32(view.extra, x_at + k));
            }

            let ib_term = view.terminator(Block(inner_body));
            if inner_back1 == NONE && inner_back0 == inner_body && view.op(ib_term) == Op::Jump {
                let jat = to_idx(at_u32(view.data, ib_term));
                if at_u32(view.extra, jat) != inner_hdr
                    || to_idx(at_u32(view.extra, jat + 1)) != ih_params.len()
                {
                    continue;
                }
                let mut next_a: Vec<u32, A> = Vec::with_capacity_in(ih_params.len(), alloc);
                for k in 0..ih_params.len() {
                    next_a.push(at_u32(view.extra, jat + 2 + k));
                }
                (
                    InnerShape::HeaderBody { inner_hdr, inner_body, inner_cond: ic, inner_cond_inverted: inverted },
                    next_a,
                    exit_a,
                )
            } else if view.op(ib_term) == Op::Br {
                // Branching inner loop: `inner_body` branches on `branch_cond` to two pure arms
                // (or directly) joining at either `merge_blk` or `inner_hdr`.
                let (merge_blk, next_a) = if inner_back1 == NONE && inner_back0 != inner_body {
                    let mb = inner_back0;
                    if mb == outer_hdr
                        || mb == outer_body
                        || mb == outer_latch
                        || mb == inner_hdr
                        || mb == inner_body
                        || view.block_kind(Block(mb)) != BlockKind::Normal
                    {
                        continue;
                    }
                    let mb_term = view.terminator(Block(mb));
                    if view.op(mb_term) != Op::Jump {
                        continue;
                    }
                    let mjat = to_idx(at_u32(view.data, mb_term));
                    if at_u32(view.extra, mjat) != inner_hdr
                        || to_idx(at_u32(view.extra, mjat + 1)) != ih_params.len()
                    {
                        continue;
                    }
                    let mut na: Vec<u32, A> = Vec::with_capacity_in(ih_params.len(), alloc);
                    for k in 0..ih_params.len() {
                        na.push(at_u32(view.extra, mjat + 2 + k));
                    }
                    (Some(mb), na)
                } else {
                    (None, Vec::new_in(alloc))
                };

                let join_blk = merge_blk.unwrap_or(inner_hdr);
                let join_argc = view.params(Block(join_blk)).len();

                let resolve_side = |target_blk: u32,
                                    target_argc: usize,
                                    target_args_at: usize|
                 -> Option<(Option<u32>, u32, Vec<u32, A>)> {
                    if target_blk == join_blk {
                        if target_argc != join_argc {
                            return None;
                        }
                        let mut args: Vec<u32, A> = Vec::with_capacity_in(join_argc, alloc);
                        for k in 0..join_argc {
                            args.push(at_u32(view.extra, target_args_at + k));
                        }
                        Some((None, inner_body, args))
                    } else {
                        let arm = target_blk;
                        if target_argc != 0
                            || arm == outer_hdr
                            || arm == outer_body
                            || arm == outer_latch
                            || arm == inner_hdr
                            || arm == inner_body
                            || arm == join_blk
                            || view.block_kind(Block(arm)) != BlockKind::Normal
                            || preds.get(to_idx(arm)).map(Vec::as_slice) != Some(&[inner_body])
                            || !view.params(Block(arm)).is_empty()
                        {
                            return None;
                        }
                        let a_term = view.terminator(Block(arm));
                        if view.op(a_term) != Op::Jump {
                            return None;
                        }
                        let ajat = to_idx(at_u32(view.data, a_term));
                        if at_u32(view.extra, ajat) != join_blk
                            || to_idx(at_u32(view.extra, ajat + 1)) != join_argc
                        {
                            return None;
                        }
                        let mut args: Vec<u32, A> = Vec::with_capacity_in(join_argc, alloc);
                        for k in 0..join_argc {
                            args.push(at_u32(view.extra, ajat + 2 + k));
                        }
                        Some((Some(arm), arm, args))
                    }
                };

                let b_at = to_idx(at_u32(view.data, ib_term));
                let branch_cond = at_u32(view.extra, b_at);
                let bt_blk = at_u32(view.extra, b_at + 1);
                let bt_argc = to_idx(at_u32(view.extra, b_at + 2));
                let be_at = b_at + 3 + bt_argc;
                let be_blk = at_u32(view.extra, be_at);
                let be_argc = to_idx(at_u32(view.extra, be_at + 1));

                let Some((then_arm, t_pred, then_join_args)) = resolve_side(bt_blk, bt_argc, b_at + 3) else {
                    continue;
                };
                let Some((else_arm, e_pred, else_join_args)) = resolve_side(be_blk, be_argc, be_at + 2) else {
                    continue;
                };
                if then_arm.is_some() && then_arm == else_arm {
                    continue;
                }

                let back_preds_ok = if let Some(mb) = merge_blk {
                    if t_pred == e_pred {
                        preds.get(to_idx(mb)).map(Vec::as_slice) == Some(&[t_pred])
                    } else {
                        match preds.get(to_idx(mb)).map(Vec::as_slice) {
                            Some(&[a, b]) => (a == t_pred && b == e_pred) || (a == e_pred && b == t_pred),
                            _ => false,
                        }
                    }
                } else if inner_back1 == NONE {
                    t_pred == inner_body && e_pred == inner_body
                } else {
                    (t_pred == inner_back0 && e_pred == inner_back1)
                        || (t_pred == inner_back1 && e_pred == inner_back0)
                };
                if !back_preds_ok {
                    continue;
                }

                (
                    InnerShape::HeaderBranch(InnerBranch {
                        inner_hdr,
                        inner_body,
                        inner_cond: ic,
                        inner_cond_inverted: inverted,
                        branch_cond,
                        then_arm,
                        else_arm,
                        merge_blk,
                        then_join_args,
                        else_join_args,
                    }),
                    next_a,
                    exit_a,
                )
            } else {
                continue;
            }
        };

        // Verify `outer_latch` predecessors are exclusively `{inner_hdr}` (or `{inner_hdr, outer_body}` with guard).
        let ol_preds_ok = match inner_entry_guard {
            None => preds.get(to_idx(outer_latch)).map(Vec::as_slice) == Some(&[inner_hdr]),
            Some(_) => match preds.get(to_idx(outer_latch)).map(Vec::as_slice) {
                Some(&[a, b]) => (a == inner_hdr && b == outer_body) || (a == outer_body && b == inner_hdr),
                _ => false,
            },
        };
        if !ol_preds_ok {
            continue;
        }

        // Mark exact block membership of the outer loop nest.
        let inner_body_blk = inner_shape.inner_body();
        let is_inner_blk = |b: u32| inner_shape.contains_block(b);
        let is_outer_blk = |b: u32| b == outer_hdr || b == outer_body || is_inner_blk(b) || b == outer_latch;

        // Validate non-IV outer header parameters are pure associative integer reductions.
        let mut outer_reductions: Vec<OuterReduction, A> = Vec::new_in(alloc);
        let mut outer_params_ok = true;
        for pos in 0..oh_params.len() {
            if pos == iv_pos {
                continue;
            }
            let p_idx = oh_params.start + pos;
            let p_inst = to_u32(p_idx);
            let p_ty = Ty(at_u16(view.tys, p_idx));
            let back_v = at_u32(view.extra, outer_back_args_at + pos);
            let back_i = to_idx(back_v);
            if at_u32(&inst_block, back_i) != outer_latch {
                outer_params_ok = false;
                break;
            }
            let op = view.op(back_i);
            if !is_assoc_int_reduction_op(op, p_ty) {
                outer_params_ok = false;
                break;
            }
            let aat = to_idx(at_u32(view.data, back_i));
            let (al, ar) = (at_u32(view.extra, aat), at_u32(view.extra, aat + 1));
            let contrib_val = if al == p_inst && ar != p_inst {
                ar
            } else if ar == p_inst && al != p_inst {
                al
            } else {
                outer_params_ok = false;
                break;
            };
            outer_reductions.push(OuterReduction {
                param_pos: pos,
                param_inst: p_inst,
                op,
                ty: p_ty,
                update_inst: back_v,
                contrib_val,
            });
        }
        if !outer_params_ok {
            continue;
        }

        // For `Rotated` outer loops, verify every `outer_exit_arg` is outer-invariant, `iv_next`, or a reduction update.
        if let OuterShape::Rotated { ref outer_exit_args, .. } = outer_shape {
            let exit_args_ok = outer_exit_args.iter().all(|&v| {
                is_pre_outer_inv(v)
                    || v == iv_next_inst
                    || outer_reductions.iter().any(|r| r.update_inst == v)
            });
            if !exit_args_ok {
                continue;
            }
        }

        // Verify no values defined inside the outer loop escape directly outside the outer loop
        // (except `outer_hdr` params in `HeaderBody`, because `orig_outer_hdr` still dominates `outer_exit`!).
        let mut escapes_ok = true;
        for b in 0..blocks {
            let b32 = to_u32(b);
            if is_outer_blk(b32) {
                continue;
            }
            for inst in view.block(Block(b32)) {
                for_each_value_use(view, inst, |v| {
                    let vb = at_u32(&inst_block, to_idx(v));
                    if is_outer_blk(vb) {
                        let allowed_hdr_param = matches!(outer_shape, OuterShape::HeaderBody { .. })
                            && vb == outer_hdr
                            && to_idx(v) >= oh_params.start
                            && to_idx(v) < oh_params.end;
                        if !allowed_hdr_param {
                            escapes_ok = false;
                        }
                    }
                });
                if !escapes_ok {
                    break;
                }
            }
            if !escapes_ok {
                break;
            }
        }
        if !escapes_ok {
            continue;
        }

        // Collect & validate `outer_body` instructions (pure + non-volatile scalar loads, no acc reads).
        let ob_start = view.params(Block(outer_body)).end;
        if ob_term < ob_start || ob_term - ob_start > MAX_OUTER_BODY_INSTS {
            continue;
        }
        let mut outer_body_insts: Vec<u32, A> = Vec::with_capacity_in(ob_term - ob_start, alloc);
        let mut outer_body_inv: Vec<bool, A> = Vec::with_capacity_in(ob_term - ob_start, alloc);
        let mut is_ob_inv_val = filled(len, false, &alloc);
        let mut all_load_ptrs: Vec<u32, A> = Vec::new_in(alloc);
        let mut ob_ok = true;
        for inst in ob_start..ob_term {
            if view.op(inst) == Op::Assume {
                continue;
            }
            let is_load = is_nonvolatile_scalar_load(view, inst);
            if !is_load && !is_pure_speculable_op(view, inst) {
                ob_ok = false;
                break;
            }
            if is_load {
                let lat = to_idx(at_u32(view.data, inst));
                all_load_ptrs.push(at_u32(view.extra, lat));
            }
            let mut all_ops_inv = !is_load;
            for_each_value_use(view, inst, |v| {
                if outer_reductions.iter().any(|r| r.param_inst == v) {
                    ob_ok = false;
                }
                let vb = at_u32(&inst_block, to_idx(v));
                if !is_pre_outer_inv(v) && vb != outer_body && !(vb == outer_hdr && v == iv_inst) {
                    ob_ok = false;
                }
                if !(is_pre_outer_inv(v) || at_bool(&is_ob_inv_val, to_idx(v))) {
                    all_ops_inv = false;
                }
            });
            if !ob_ok {
                break;
            }
            set_at(&mut is_ob_inv_val, inst, all_ops_inv);
            outer_body_insts.push(to_u32(inst));
            outer_body_inv.push(all_ops_inv);
        }
        if !ob_ok {
            continue;
        }

        // If `inner_entry_guard` is present, its condition MUST be outer-loop-invariant (either
        // defined strictly before `outer_hdr` or computed inside `outer_body` purely from
        // outer-invariant operands) so it can be hoisted and checked once in `b_amac_entry`.
        if let Some((guard_cond, _)) = inner_entry_guard
            && !(is_pre_outer_inv(guard_cond) || at_bool(&is_ob_inv_val, to_idx(guard_cond)))
        {
            continue;
        }

        // Validate `inner_hdr`, `inner_body`, and (for `HeaderBranch`) pure branch arms & merge block.
        let ih_term = view.terminator(Block(inner_hdr));
        let ih_body_start = ih_params.end;
        let ib_range = if inner_body_blk != inner_hdr {
            let r = view.block(Block(inner_body_blk));
            r.start..(r.end - 1)
        } else {
            0..0
        };
        let mut extra_branch_insts = 0usize;
        if let InnerShape::HeaderBranch(ref br) = inner_shape {
            let ta_len = br.then_arm.map_or(0, |b| view.block(Block(b)).len().saturating_sub(1));
            let ea_len = br.else_arm.map_or(0, |b| view.block(Block(b)).len().saturating_sub(1));
            let mb_len = br.merge_blk.map_or(0, |b| {
                view.terminator(Block(b)).saturating_sub(view.params(Block(b)).end)
            });
            if ta_len > MAX_INNER_BRANCH_ARM_INSTS || ea_len > MAX_INNER_BRANCH_ARM_INSTS {
                continue;
            }
            extra_branch_insts = ta_len + ea_len + mb_len;
        }
        let total_inner_insts = (ih_term - ih_body_start) + (ib_range.end - ib_range.start) + extra_branch_insts;
        if total_inner_insts == 0 || total_inner_insts > MAX_INNER_INSTS {
            continue;
        }

        let mut inner_ok = true;
        let mut dep_on_inner_param = filled(len, false, &alloc);
        for p in ih_params.start..ih_params.end {
            let p_ty = Ty(at_u16(view.tys, p));
            if !is_latchable_scalar_ty(p_ty) {
                inner_ok = false;
                break;
            }
            set_at(&mut dep_on_inner_param, p, true);
        }
        if !inner_ok {
            continue;
        }

        let mut inner_dependent_loads = 0u32;
        let check_inner_inst = |inst: usize,
                                allow_load: bool,
                                dep: &mut [bool],
                                load_ptrs: &mut Vec<u32, A>,
                                dep_loads: &mut u32|
         -> bool {
            if view.op(inst) == Op::Assume {
                return true;
            }
            let is_load = is_nonvolatile_scalar_load(view, inst);
            if is_load {
                if !allow_load {
                    return false;
                }
                let lat = to_idx(at_u32(view.data, inst));
                let ptr = at_u32(view.extra, lat);
                load_ptrs.push(ptr);
                if at_bool(dep, to_idx(ptr)) {
                    *dep_loads += 1;
                }
            } else if !is_pure_speculable_op(view, inst) {
                return false;
            }
            let mut uses_dep = false;
            let mut ok = true;
            for_each_value_use(view, inst, |v| {
                if outer_reductions.iter().any(|r| r.param_inst == v) {
                    ok = false;
                }
                let vb = at_u32(&inst_block, to_idx(v));
                if !is_pre_outer_inv(v) && vb != outer_body && !is_inner_blk(vb) && v != iv_inst {
                    ok = false;
                }
                if at_bool(dep, to_idx(v)) {
                    uses_dep = true;
                }
            });
            if uses_dep {
                set_at(dep, inst, true);
            }
            ok
        };

        // For `HeaderBody`/`HeaderBranch`, `inner_hdr` MUST be 100% pure (zero loads); `inner_body` has the loads.
        // For `SingleBlock`, `inner_hdr` IS the body and allows loads.
        let hdr_allows_load = matches!(inner_shape, InnerShape::SingleBlock { .. });
        for inst in ih_body_start..ih_term {
            if !check_inner_inst(
                inst,
                hdr_allows_load,
                &mut dep_on_inner_param,
                &mut all_load_ptrs,
                &mut inner_dependent_loads,
            ) {
                inner_ok = false;
                break;
            }
        }
        if inner_ok && inner_body_blk != inner_hdr {
            for inst in ib_range.start..ib_range.end {
                if !check_inner_inst(
                    inst,
                    true,
                    &mut dep_on_inner_param,
                    &mut all_load_ptrs,
                    &mut inner_dependent_loads,
                ) {
                    inner_ok = false;
                    break;
                }
            }
        }
        if inner_ok
            && let InnerShape::HeaderBranch(ref br) = inner_shape
        {
            let check_arm_val = |v: u32, own_blk: Option<u32>| -> bool {
                if outer_reductions.iter().any(|r| r.param_inst == v) {
                    return false;
                }
                let vb = at_u32(&inst_block, to_idx(v));
                is_pre_outer_inv(v)
                    || vb == outer_body
                    || vb == inner_hdr
                    || vb == inner_body_blk
                    || own_blk == Some(vb)
                    || v == iv_inst
            };
            if !check_arm_val(br.branch_cond, None) {
                inner_ok = false;
            }
            let check_pure_blk = |blk: u32| -> bool {
                let start = view.params(Block(blk)).end;
                let end = view.terminator(Block(blk));
                for inst in start..end {
                    if view.op(inst) == Op::Assume {
                        continue;
                    }
                    if !is_pure_speculable_op(view, inst) {
                        return false;
                    }
                    let mut ok = true;
                    for_each_value_use(view, inst, |v| {
                        if !check_arm_val(v, Some(blk)) {
                            ok = false;
                        }
                    });
                    if !ok {
                        return false;
                    }
                }
                true
            };
            if inner_ok && let Some(ta) = br.then_arm && !check_pure_blk(ta) {
                inner_ok = false;
            }
            if inner_ok && let Some(ea) = br.else_arm && !check_pure_blk(ea) {
                inner_ok = false;
            }
            if inner_ok
                && (!br.then_join_args.iter().all(|&v| check_arm_val(v, br.then_arm))
                    || !br.else_join_args.iter().all(|&v| check_arm_val(v, br.else_arm)))
            {
                inner_ok = false;
            }
            if inner_ok {
                let join_params = view.params(Block(br.merge_blk.unwrap_or(inner_hdr)));
                for k in 0..join_params.len() {
                    if at_u32(&br.then_join_args, k) != at_u32(&br.else_join_args, k)
                        && !is_latchable_scalar_ty(Ty(at_u16(view.tys, join_params.start + k)))
                    {
                        inner_ok = false;
                        break;
                    }
                }
            }
            if inner_ok
                && let Some(mb) = br.merge_blk
                && (!check_pure_blk(mb) || !inner_next_args.iter().all(|&v| check_arm_val(v, Some(mb))))
            {
                inner_ok = false;
            }
        }
        // Require at least one inner-loop-dependent `Op::Load` (pointer-chasing / indexed lookup!).
        if !inner_ok || inner_dependent_loads == 0 {
            continue;
        }

        // Validate `outer_latch`: pure post-processing + non-volatile loads + provably non-aliasing
        // unit-stride output stores + associative reduction updates + IV increment.
        let ol_start = ol_params.end;
        if ol_term < ol_start || ol_term - ol_start > MAX_OUTER_LATCH_INSTS {
            continue;
        }
        let mut outer_latch_insts: Vec<u32, A> = Vec::with_capacity_in(ol_term - ol_start, alloc);
        let mut inner_direct_uses: Vec<u32, A> = Vec::new_in(alloc);
        let mut store_bases: Vec<u32, A> = Vec::new_in(alloc);
        let mut ol_ok = true;
        for inst in ol_start..ol_term {
            let inst32 = to_u32(inst);
            if view.op(inst) == Op::Assume || inst32 == iv_next_inst {
                continue;
            }
            if matches!(outer_shape, OuterShape::Rotated { .. }) && inst == outer_cond_inst {
                continue;
            }
            if outer_reductions.iter().any(|r| r.update_inst == inst32) {
                continue;
            }

            let is_load = is_nonvolatile_scalar_load(view, inst);
            let is_store = is_nonvolatile_scalar_store(view, inst);
            if !is_load && !is_store && !is_pure_speculable_op(view, inst) {
                ol_ok = false;
                break;
            }
            if is_load {
                let lat = to_idx(at_u32(view.data, inst));
                all_load_ptrs.push(at_u32(view.extra, lat));
            }
            if is_store {
                // Require unit-stride store `out[q]` with outer-invariant `out_base`.
                let sat = to_idx(at_u32(view.data, inst));
                let ptr_v = to_idx(at_u32(view.extra, sat + 1));
                if view.op(ptr_v) != Op::PtrAdd {
                    ol_ok = false;
                    break;
                }
                let pat = to_idx(at_u32(view.data, ptr_v));
                let (st_base, st_off) = (at_u32(view.extra, pat), at_u32(view.extra, pat + 1));
                if !is_pre_outer_inv(st_base) || !is_unit_scaled_iv(view, st_off, iv_inst) {
                    ol_ok = false;
                    break;
                }
                store_bases.push(st_base);
            }

            for_each_value_use(view, inst, |v| {
                // Non-reduction instructions in `outer_latch` may NOT read any reduction accumulator,
                // reduction update, or skipped `Rotated` exit condition.
                if outer_reductions.iter().any(|r| r.param_inst == v || r.update_inst == v)
                    || (matches!(outer_shape, OuterShape::Rotated { .. }) && to_idx(v) == outer_cond_inst)
                {
                    ol_ok = false;
                }
                let vb = at_u32(&inst_block, to_idx(v));
                if is_inner_blk(vb) {
                    // Direct use of an inner-loop value (allowed only when `inner_hdr` dominates `outer_latch`).
                    if inner_entry_guard.is_some() || vb != inner_hdr {
                        ol_ok = false;
                    } else if inner_direct_uses.scan_for(&v).is_none() {
                        inner_direct_uses.push(v);
                    }
                } else if !is_pre_outer_inv(v) && vb != outer_body && vb != outer_latch && v != iv_inst {
                    ol_ok = false;
                }
            });
            if !ol_ok {
                break;
            }
            outer_latch_insts.push(inst32);
        }
        if !ol_ok {
            continue;
        }

        // Also check reduction `contrib_val` references (in case a reduction directly consumes an `inner_hdr` value).
        for red in &outer_reductions {
            let cv = red.contrib_val;
            if outer_reductions.iter().any(|r| r.param_inst == cv || r.update_inst == cv)
                || (matches!(outer_shape, OuterShape::Rotated { .. }) && to_idx(cv) == outer_cond_inst)
            {
                ol_ok = false;
                break;
            }
            let vb = at_u32(&inst_block, to_idx(cv));
            if is_inner_blk(vb) {
                if inner_entry_guard.is_some() || vb != inner_hdr {
                    ol_ok = false;
                    break;
                }
                if inner_direct_uses.scan_for(&cv).is_none() {
                    inner_direct_uses.push(cv);
                }
            } else if !is_pre_outer_inv(cv) && vb != outer_body && vb != outer_latch && cv != iv_inst {
                ol_ok = false;
                break;
            }
        }
        if !ol_ok {
            continue;
        }

        // Every unit-stride output store in `outer_latch` MUST provably not alias ANY load in the nest.
        let alias_ok = store_bases.iter().all(|&sb| {
            all_load_ptrs.iter().all(|&lp| provably_noalias_bases(view, sb, lp))
        });
        if !alias_ok {
            continue;
        }

        // Require at least one observable outer effect (associative reduction or unit-stride store).
        if outer_reductions.is_empty() && store_bases.is_empty() {
            continue;
        }

        // Compute lane-uniformity fixpoint for inner loop parameters & instructions!
        // A value is lane-uniform iff it is guaranteed identical across all `K` lanes on every iteration.
        let n_ip = ih_params.len();
        let mut inner_param_uniform = filled(n_ip, true, &alloc);
        let mut inner_inst_uniform = filled(len, false, &alloc);
        for i in 0..len {
            if is_pre_outer_inv(to_u32(i)) || at_bool(&is_ob_inv_val, i) {
                set_at(&mut inner_inst_uniform, i, true);
            }
        }
        let propagate_uniform = |param_u: &[bool], inst_u: &mut [bool]| {
            for p in 0..n_ip {
                set_at(inst_u, ih_params.start + p, at_bool(param_u, p));
            }
            let prop_range = |start: usize, end: usize, iu: &mut [bool]| {
                for inst in start..end {
                    if view.op(inst) == Op::Assume {
                        continue;
                    }
                    let mut u = view.op(inst) != Op::Load;
                    for_each_value_use(view, inst, |v| {
                        if !at_bool(iu, to_idx(v)) {
                            u = false;
                        }
                    });
                    set_at(iu, inst, u);
                }
            };
            prop_range(ih_body_start, ih_term, inst_u);
            if inner_body_blk != inner_hdr {
                prop_range(ib_range.start, ib_range.end, inst_u);
            }
            if let InnerShape::HeaderBranch(ref br) = inner_shape {
                if let Some(ta) = br.then_arm {
                    let r = view.block(Block(ta));
                    prop_range(r.start, r.end.saturating_sub(1), inst_u);
                }
                if let Some(ea) = br.else_arm {
                    let r = view.block(Block(ea));
                    prop_range(r.start, r.end.saturating_sub(1), inst_u);
                }
                if let Some(mb) = br.merge_blk {
                    let mb_params = view.params(Block(mb));
                    let bc_u = at_bool(inst_u, to_idx(br.branch_cond));
                    for k in 0..mb_params.len() {
                        let ta_v = at_u32(&br.then_join_args, k);
                        let ea_v = at_u32(&br.else_join_args, k);
                        let u = if ta_v == ea_v {
                            at_bool(inst_u, to_idx(ta_v))
                        } else {
                            bc_u && at_bool(inst_u, to_idx(ta_v)) && at_bool(inst_u, to_idx(ea_v))
                        };
                        set_at(inst_u, mb_params.start + k, u);
                    }
                    prop_range(mb_params.end, view.terminator(Block(mb)), inst_u);
                }
            }
        };
        for _ in 0..(n_ip + 2) {
            propagate_uniform(&inner_param_uniform, &mut inner_inst_uniform);
            let mut changed = false;
            for p in 0..n_ip {
                if !at_bool(&inner_param_uniform, p) {
                    continue;
                }
                let init_u = at_bool(&inner_inst_uniform, to_idx(at_u32(&inner_init_args, p)));
                let next_u = match &inner_shape {
                    InnerShape::HeaderBranch(br) if br.merge_blk.is_none() => {
                        let ta_v = at_u32(&br.then_join_args, p);
                        let ea_v = at_u32(&br.else_join_args, p);
                        if ta_v == ea_v {
                            at_bool(&inner_inst_uniform, to_idx(ta_v))
                        } else {
                            at_bool(&inner_inst_uniform, to_idx(br.branch_cond))
                                && at_bool(&inner_inst_uniform, to_idx(ta_v))
                                && at_bool(&inner_inst_uniform, to_idx(ea_v))
                        }
                    }
                    _ => at_bool(&inner_inst_uniform, to_idx(at_u32(&inner_next_args, p))),
                };
                if !(init_u && next_u) {
                    set_at(&mut inner_param_uniform, p, false);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // Final propagation pass so `inner_inst_uniform` matches final `inner_param_uniform`.
        propagate_uniform(&inner_param_uniform, &mut inner_inst_uniform);

        let inner_cond = inner_shape.inner_cond();
        let inner_is_uniform = at_bool(&inner_inst_uniform, to_idx(inner_cond));

        // For non-uniform `SingleBlock` inner loops, collect any exit values that need latching.
        let mut extra_latched: Vec<u32, A> = Vec::new_in(alloc);
        let mut latch_types_ok = true;
        if matches!(inner_shape, InnerShape::SingleBlock { .. }) && !inner_is_uniform {
            let mut consider = |v: u32| {
                let vb = at_u32(&inst_block, to_idx(v));
                if !is_inner_blk(vb) {
                    return;
                }
                let matches_non_uniform_next = (0..n_ip)
                    .any(|p| !at_bool(&inner_param_uniform, p) && at_u32(&inner_next_args, p) == v);
                if matches_non_uniform_next {
                    return;
                }
                if extra_latched.scan_for(&v).is_none() {
                    let ty = Ty(at_u16(view.tys, to_idx(v)));
                    if !is_latchable_scalar_ty(ty) {
                        latch_types_ok = false;
                    } else {
                        extra_latched.push(v);
                    }
                }
            };
            for &v in &inner_exit_args {
                consider(v);
            }
            for &v in &inner_direct_uses {
                consider(v);
            }
        }
        if !latch_types_ok {
            continue;
        }

        // Count per-lane live registers inside the inner loop to enforce `opts.max_state_regs`:
        // `uniform_params + K * (non_uniform_params + extra_latched + per_query_live_in_inner)`.
        let mut per_query_live: Vec<u32, A> = Vec::new_in(alloc);
        let mut note_live = |v: u32| {
            let vb = at_u32(&inst_block, to_idx(v));
            if (vb == outer_body && !at_bool(&is_ob_inv_val, to_idx(v))) || v == iv_inst {
                if per_query_live.scan_for(&v).is_none() {
                    per_query_live.push(v);
                }
            }
        };
        for inst in ih_body_start..ih_term {
            if view.op(inst) != Op::Assume {
                for_each_value_use(view, inst, &mut note_live);
            }
        }
        if inner_body_blk != inner_hdr {
            for inst in ib_range.start..ib_range.end {
                if view.op(inst) != Op::Assume {
                    for_each_value_use(view, inst, &mut note_live);
                }
            }
        }
        if let InnerShape::HeaderBranch(ref br) = inner_shape {
            note_live(br.branch_cond);
            for opt_b in [br.then_arm, br.else_arm, br.merge_blk] {
                if let Some(blk) = opt_b {
                    let start = view.params(Block(blk)).end;
                    let end = view.terminator(Block(blk));
                    for inst in start..end {
                        if view.op(inst) != Op::Assume {
                            for_each_value_use(view, inst, &mut note_live);
                        }
                    }
                }
            }
            for &v in &br.then_join_args {
                note_live(v);
            }
            for &v in &br.else_join_args {
                note_live(v);
            }
            for &v in &inner_next_args {
                note_live(v);
            }
        }
        let n_uniform_params = (0..n_ip).filter(|&p| at_bool(&inner_param_uniform, p)).count();
        let n_non_uniform_params = n_ip - n_uniform_params;
        let per_lane_regs = n_non_uniform_params + extra_latched.len() + per_query_live.len();
        if per_lane_regs == 0 {
            continue;
        }

        // Pick largest group width `K <= min(opts.group_width, MAX_GROUP_WIDTH)` fitting `max_state_regs`.
        let chosen_k = select_group_width(opts.group_width, n_uniform_params, per_lane_regs, opts.max_state_regs);
        if chosen_k < 2 {
            continue;
        }

        // If `iv_next_inst` lives in `outer_body` and is unused before `outer_latch`, skip emitting
        // K redundant step instructions in `b_amac_outer_body` (it is mapped in `b_amac_outer_latch`).
        let mut skip_iv_next_in_ob = at_u32(&inst_block, to_idx(iv_next_inst)) == outer_body;
        if skip_iv_next_in_ob {
            for &oi in &outer_body_insts {
                if oi == iv_next_inst {
                    continue;
                }
                for_each_value_use(view, to_idx(oi), |v| {
                    if v == iv_next_inst {
                        skip_iv_next_in_ob = false;
                    }
                });
            }
            for_each_value_use(view, ob_term, |v| {
                if v == iv_next_inst {
                    skip_iv_next_in_ob = false;
                }
            });
            for inst in ih_body_start..=ih_term {
                if view.op(inst) == Op::Assume {
                    continue;
                }
                for_each_value_use(view, inst, |v| {
                    if v == iv_next_inst {
                        skip_iv_next_in_ob = false;
                    }
                });
            }
            if inner_body_blk != inner_hdr {
                for inst in view.block(Block(inner_body_blk)) {
                    if view.op(inst) == Op::Assume {
                        continue;
                    }
                    for_each_value_use(view, inst, |v| {
                        if v == iv_next_inst {
                            skip_iv_next_in_ob = false;
                        }
                    });
                }
            }
            if let InnerShape::HeaderBranch(ref br) = inner_shape {
                for opt_b in [br.then_arm, br.else_arm, br.merge_blk] {
                    if let Some(blk) = opt_b {
                        for inst in view.block(Block(blk)) {
                            if view.op(inst) == Op::Assume {
                                continue;
                            }
                            for_each_value_use(view, inst, |v| {
                                if v == iv_next_inst {
                                    skip_iv_next_in_ob = false;
                                }
                            });
                        }
                    }
                }
            }
        }

        return Some(AmacPlan {
            preheader,
            pre_edge,
            outer_shape,
            outer_latch,
            iv_pos,
            iv_init,
            iv_inst,
            iv_next_inst,
            iv_kind,
            bound,
            skip_iv_next_in_ob,
            outer_reductions,
            outer_body_insts,
            outer_body_inv,
            inner_entry_guard,
            inner_shape,
            inner_init_args,
            inner_next_args,
            inner_exit_args,
            inner_param_uniform,
            inner_inst_uniform,
            inner_is_uniform,
            inner_direct_uses,
            extra_latched,
            outer_latch_insts,
            group_width: chosen_k,
        });
    }

    None
}

// ---------------------------------------------------------------------------
// Instruction cloning & lockstep load-wave emitter
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ValFixup {
    Data { inst: usize, old_val: u32 },
    Extra { pos: usize, old_val: u32 },
}

fn copy_orig_inst<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    redirect_edge: Option<(u32, u32)>,
    func: &mut Func<A>,
    fixups: &mut Vec<ValFixup, A>,
) -> u32 {
    let op = view.op(inst);
    let op_raw = at_u8(view.ops, inst);
    let flags = at_u8(view.flags, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let inst_data = at_u32(view.data, inst);
    let new_inst = func.ops.len();

    let map_blk = |old_b: u32, ordinal: u32| -> u32 {
        if let Some((want_ord, target_b)) = redirect_edge
            && ordinal == want_ord
        {
            target_b
        } else {
            at_u32(blk_map, to_idx(old_b))
        }
    };

    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            let mapped = at_u32(val_map, to_idx(inst_data));
            if mapped == NONE {
                fixups.push(ValFixup::Data { inst: new_inst, old_val: inst_data });
                0
            } else {
                mapped
            }
        }
        Layout::Data(Field::Block) => at_u32(blk_map, to_idx(inst_data)),
        Layout::Data(_) => inst_data,
        Layout::Extra(fields) => {
            let src = to_idx(inst_data);
            let dst = to_u32(func.extra.len());
            let mut p = src;
            let mut target_ord = 0u32;
            for &f in fields {
                match f {
                    Field::Value => {
                        let old = at_u32(view.extra, p);
                        let mapped = at_u32(val_map, to_idx(old));
                        if mapped == NONE {
                            fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                            func.extra.push(0);
                        } else {
                            func.extra.push(mapped);
                        }
                        p += 1;
                    }
                    Field::Block => {
                        func.extra.push(at_u32(blk_map, to_idx(at_u32(view.extra, p))));
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            let old = at_u32(view.extra, p + k);
                            let mapped = at_u32(val_map, to_idx(old));
                            if mapped == NONE {
                                fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                                func.extra.push(0);
                            } else {
                                func.extra.push(mapped);
                            }
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(blk_map, to_idx(at_u32(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Target => {
                        let old_b = at_u32(view.extra, p);
                        func.extra.push(map_blk(old_b, target_ord));
                        target_ord += 1;
                        let n_u32 = at_u32(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            let old = at_u32(view.extra, p + k);
                            let mapped = at_u32(val_map, to_idx(old));
                            if mapped == NONE {
                                fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                                func.extra.push(0);
                            } else {
                                func.extra.push(mapped);
                            }
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
                            let old_b = at_u32(view.extra, p);
                            func.extra.push(map_blk(old_b, target_ord));
                            target_ord += 1;
                            let an_u32 = at_u32(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                let old = at_u32(view.extra, p + k);
                                let mapped = at_u32(val_map, to_idx(old));
                                if mapped == NONE {
                                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                                    func.extra.push(0);
                                } else {
                                    func.extra.push(mapped);
                                }
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
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
    to_u32(new_inst)
}

#[inline]
fn resolve_lane_val(orig: u32, lane_map: &[u32], val_map: &[u32]) -> u32 {
    let idx = to_idx(orig);
    let m = at_u32(lane_map, idx);
    if m != NONE {
        m
    } else {
        at_u32(val_map, idx)
    }
}

/// Emits a cloned non-terminator scalar instruction (pure ALU, `Load`, or `Store`) for a lane,
/// stripping promise flags (`Flags::NONE`) for speculative / masked safety while preserving `MemAttrs`.
fn emit_lane_inst<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    lane_map: &[u32],
    val_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    let op = view.op(inst);
    let op_raw = at_u8(view.ops, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let inst_data = at_u32(view.data, inst);
    let new_inst = to_u32(func.ops.len());

    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => resolve_lane_val(inst_data, lane_map, val_map),
        Layout::Data(_) => inst_data,
        Layout::Extra(fields) => {
            let src = to_idx(inst_data);
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        let old = at_u32(view.extra, p);
                        func.extra.push(resolve_lane_val(old, lane_map, val_map));
                        p += 1;
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
    func.flags.push(Flags::NONE.0);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    new_inst
}

/// Emits the inner body instructions across `K` lanes in **lockstep load-wave order**:
/// 1. For each load wave `w = 0..=max_load_wave`:
///    - Emit pure address-generation instructions that transitively feed wave `w` loads (uniform once,
///      non-uniform across `0..K`).
///    - For each `Op::Load` in wave `w`:
///      - Compute `safe_addr[0..K]` (if `!is_uniform`: branchless active-lane `safe_fallback` select
///        chain so finished lanes NEVER load out of bounds!).
///      - Emit `K` parallel `"blaze.prefetch.r1"` hints back-to-back (if `prefetch_id.is_some()`).
///      - Emit `K` parallel `Op::Load` instructions back-to-back for full MSHR MLP.
/// 2. Emit remaining post-load ALU (`Icmp` / `Select` / updates) grouped per lane `k = 0..K` so
///    AArch64 `isel` fuses `cmp + csel + csinc` with zero NZCV clobbering.
fn emit_lockstep_inner_waves<A: Allocator + Copy>(
    view: &View<'_>,
    inst_start: usize,
    inst_end: usize,
    k_width: usize,
    is_uniform_loop: bool,
    inst_uniform: &[bool],
    active_lanes: &[u32],
    prefetch_id: Option<IntrinsicId>,
    val_map: &[u32],
    lane_maps: &mut [Vec<u32, A>],
    func: &mut Func<A>,
    alloc: A,
    mut on_lane_post: impl FnMut(usize, &mut Vec<u32, A>, &mut Func<A>),
) -> u32 {
    let count = inst_end.saturating_sub(inst_start);
    let mut wave_of = filled(count, 0u32, &alloc);
    let mut max_load_wave = 0u32;
    let mut has_load = false;

    for idx in 0..count {
        let inst = inst_start + idx;
        if view.op(inst) == Op::Assume {
            continue;
        }
        let mut w = 0u32;
        for_each_value_use(view, inst, |v| {
            let vi = to_idx(v);
            if vi >= inst_start && vi < inst_end {
                let dep_idx = vi - inst_start;
                let dep_w = if view.op(vi) == Op::Load {
                    at_u32(&wave_of, dep_idx) + 1
                } else {
                    at_u32(&wave_of, dep_idx)
                };
                if dep_w > w {
                    w = dep_w;
                }
            }
        });
        set_at(&mut wave_of, idx, w);
        if view.op(inst) == Op::Load {
            has_load = true;
            if w > max_load_wave {
                max_load_wave = w;
            }
        }
    }

    // Back-propagate earliest load wave fed by each non-load instruction (`NONE` = post-load).
    let mut feeds_wave = filled(count, NONE, &alloc);
    for rev in 0..count {
        let idx = count - 1 - rev;
        let inst = inst_start + idx;
        if view.op(inst) == Op::Assume {
            continue;
        }
        let my_w = if view.op(inst) == Op::Load {
            let lw = at_u32(&wave_of, idx);
            set_at(&mut feeds_wave, idx, lw);
            lw
        } else {
            at_u32(&feeds_wave, idx)
        };
        if my_w != NONE {
            for_each_value_use(view, inst, |v| {
                let vi = to_idx(v);
                if vi >= inst_start && vi < inst_end && view.op(vi) != Op::Load {
                    let dep_idx = vi - inst_start;
                    let cur = at_u32(&feeds_wave, dep_idx);
                    if cur == NONE || my_w < cur {
                        set_at(&mut feeds_wave, dep_idx, my_w);
                    }
                }
            });
        }
    }

    let mut prefetches_emitted = 0u32;
    let mut emitted = filled(count, false, &alloc);
    for idx in 0..count {
        if view.op(inst_start + idx) == Op::Assume {
            set_at(&mut emitted, idx, true);
        }
    }

    if has_load {
        for w in 0..=max_load_wave {
            // 1. Emit pure address-generation instructions for wave `w` across all `K` lanes.
            for idx in 0..count {
                let inst = inst_start + idx;
                if at_bool(&emitted, idx) || view.op(inst) == Op::Load || at_u32(&feeds_wave, idx) != w {
                    continue;
                }
                if at_bool(inst_uniform, inst) {
                    let mapped = match lane_maps.first() {
                        Some(lm) => emit_lane_inst(view, inst, lm, val_map, func),
                        None => 0,
                    };
                    for lm in lane_maps.iter_mut().take(k_width) {
                        set_at(lm, inst, mapped);
                    }
                } else {
                    for lm in lane_maps.iter_mut().take(k_width) {
                        let mapped = emit_lane_inst(view, inst, lm, val_map, func);
                        set_at(lm, inst, mapped);
                    }
                }
                set_at(&mut emitted, idx, true);
            }

            // Collect loads belonging to wave `w`.
            let mut wave_loads: Vec<usize, A> = Vec::new_in(alloc);
            for idx in 0..count {
                let inst = inst_start + idx;
                if !at_bool(&emitted, idx) && view.op(inst) == Op::Load && at_u32(&wave_of, idx) == w {
                    wave_loads.push(inst);
                    set_at(&mut emitted, idx, true);
                }
            }

            // 2. For each load in wave `w`, compute `safe_addr[0..K]`, issue `K` prefetches, then `K` loads.
            let mut safe_ptrs_per_load: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(wave_loads.len(), alloc);
            for &ld_inst in &wave_loads {
                let lat = to_idx(at_u32(view.data, ld_inst));
                let orig_ptr = at_u32(view.extra, lat);
                let mut raw_ptrs: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
                for lm in lane_maps.iter().take(k_width) {
                    raw_ptrs.push(resolve_lane_val(orig_ptr, lm, val_map));
                }
                let mut safe_ptrs: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
                if is_uniform_loop {
                    for &rp in &raw_ptrs {
                        safe_ptrs.push(rp);
                    }
                } else {
                    // At least one lane is active whenever the inner body runs; select chain yields
                    // an active lane's valid, aligned, in-bounds pointer for every finished lane!
                    let mut fallback = at_u32(&raw_ptrs, 0);
                    for lane in 1..k_width {
                        let act = at_u32(active_lanes, lane);
                        let rp = at_u32(&raw_ptrs, lane);
                        fallback = push_select(func, Ty::PTR, act, rp, fallback);
                    }
                    for lane in 0..k_width {
                        let act = at_u32(active_lanes, lane);
                        let rp = at_u32(&raw_ptrs, lane);
                        let sp = push_select(func, Ty::PTR, act, rp, fallback);
                        safe_ptrs.push(sp);
                    }
                }
                safe_ptrs_per_load.push(safe_ptrs);
            }

            // Issue all `K` prefetches for wave `w` BEFORE any `Op::Load` in wave `w`!
            if let Some(pf_id) = prefetch_id {
                for (li, &ld_inst) in wave_loads.iter().enumerate() {
                    let lat = to_idx(at_u32(view.data, ld_inst));
                    let load_off = at_u32(view.extra, lat + 1).cast_signed();
                    if let Some(safe_ptrs) = safe_ptrs_per_load.get(li) {
                        for &sp in safe_ptrs.iter() {
                            let pf_ptr = if load_off != 0 {
                                let off_c = push_iconst(func, Ty::I64, i64::from(load_off).cast_unsigned());
                                push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, sp, off_c)
                            } else {
                                sp
                            };
                            push_prefetch(func, pf_id, pf_ptr);
                            prefetches_emitted += 1;
                        }
                    }
                }
            }

            // Issue all `K` parallel loads for wave `w` back-to-back!
            for (li, &ld_inst) in wave_loads.iter().enumerate() {
                let lat = to_idx(at_u32(view.data, ld_inst));
                let load_off = at_u32(view.extra, lat + 1).cast_signed();
                let ty = Ty(at_u16(view.tys, ld_inst));
                let attr = at_u32(view.attrs, ld_inst);
                if let Some(safe_ptrs) = safe_ptrs_per_load.get(li) {
                    for (lane, &sp) in safe_ptrs.iter().enumerate() {
                        let ld = push_load(func, ty, sp, load_off, attr);
                        if let Some(lm) = lane_maps.get_mut(lane) {
                            set_at(lm, ld_inst, ld);
                        }
                    }
                }
            }
        }
    }

    // 3. Emit uniform post-load instructions once, then per-lane post-load instructions grouped
    //    by lane so ARM64 `isel` fuses `cmp + csel + csinc` with zero flag clobbering.
    for idx in 0..count {
        let inst = inst_start + idx;
        if at_bool(&emitted, idx) || !at_bool(inst_uniform, inst) {
            continue;
        }
        let mapped = match lane_maps.first() {
            Some(lm) => emit_lane_inst(view, inst, lm, val_map, func),
            None => 0,
        };
        for lm in lane_maps.iter_mut().take(k_width) {
            set_at(lm, inst, mapped);
        }
        set_at(&mut emitted, idx, true);
    }
    for lane in 0..k_width {
        if let Some(lm) = lane_maps.get_mut(lane) {
            for idx in 0..count {
                let inst = inst_start + idx;
                if at_bool(&emitted, idx) {
                    continue;
                }
                let mapped = emit_lane_inst(view, inst, lm, val_map, func);
                set_at(lm, inst, mapped);
            }
            on_lane_post(lane, lm, func);
        }
    }

    prefetches_emitted
}

/// Reduces `vals[0..k]` via a balanced binary tree of associative `op` (`Add`/`Xor`/`Or`/`And`),
/// then combines into `acc`.
fn emit_assoc_tree_reduction<A: Allocator + Copy>(
    func: &mut Func<A>,
    op: Op,
    ty: Ty,
    acc: u32,
    vals: &[u32],
    alloc: A,
) -> u32 {
    if vals.is_empty() {
        return acc;
    }
    let mut cur: Vec<u32, A> = Vec::with_capacity_in(vals.len(), alloc);
    for &v in vals {
        cur.push(v);
    }
    for _ in 0..8 {
        if cur.len() <= 1 {
            break;
        }
        let mut next: Vec<u32, A> = Vec::with_capacity_in(cur.len().div_ceil(2), alloc);
        let mut i = 0usize;
        while i + 1 < cur.len() {
            let combined = push_bin(func, op, Flags::NONE, ty, at_u32(&cur, i), at_u32(&cur, i + 1));
            next.push(combined);
            i += 2;
        }
        if i < cur.len() {
            next.push(at_u32(&cur, i));
        }
        cur = next;
    }
    push_bin(func, op, Flags::NONE, ty, acc, at_u32(&cur, 0))
}

// ---------------------------------------------------------------------------
// CFG & SSA Rewrite (`rewrite_amac_nest`)
// ---------------------------------------------------------------------------

fn rewrite_amac_nest<A: Allocator + Copy>(
    view: &View<'_>,
    plan: &AmacPlan<A>,
    opts: AmacOpts,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    let k_width = plan.group_width;
    let outer_hdr = plan.outer_shape.outer_hdr();
    let outer_hdr_idx = to_idx(outer_hdr);
    let inner_hdr = plan.inner_shape.inner_hdr();
    let ih_params = view.params(Block(inner_hdr));
    let n_ip = ih_params.len();
    let oh_params = view.params(Block(outer_hdr));
    let n_op = oh_params.len();
    let ol_params = view.params(Block(plan.outer_latch));
    let n_olp = ol_params.len();

    let has_inner_body_blk =
        matches!(plan.inner_shape, InnerShape::HeaderBody { .. } | InnerShape::HeaderBranch(_));
    let has_tail_check_blk = matches!(plan.outer_shape, OuterShape::Rotated { .. });
    let num_new_blocks: u32 = 4 + u32::from(has_inner_body_blk) + u32::from(has_tail_check_blk);

    let b_amac_entry = outer_hdr;
    let b_amac_outer_body = b_amac_entry + 1;
    let b_amac_inner_hdr = b_amac_outer_body + 1;
    let b_amac_inner_body = b_amac_inner_hdr + 1;
    let b_amac_outer_latch = if has_inner_body_blk {
        b_amac_inner_body + 1
    } else {
        b_amac_inner_hdr + 1
    };
    let b_amac_tail_check = b_amac_outer_latch + 1;

    // Precompute block remapping table: `0..outer_hdr` unchanged; `outer_hdr..blocks` shifted by `+num_new_blocks`.
    let mut blk_map = filled(blocks, 0u32, &alloc);
    for b in 0..blocks {
        let b32 = to_u32(b);
        set_at(&mut blk_map, b, if b < outer_hdr_idx { b32 } else { b32 + num_new_blocks });
    }
    let orig_outer_hdr_new = at_u32(&blk_map, outer_hdr_idx);
    let orig_outer_exit_new = at_u32(&blk_map, to_idx(plan.outer_shape.outer_exit()));

    let mut func = view.to_func_in(alloc);
    let prefetch_id = if opts.emit_prefetch {
        Some(ensure_prefetch_intrinsic(&mut func))
    } else {
        None
    };
    clear_code_columns(&mut func);

    let mut val_map = filled(len, NONE, &alloc);
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);

    // 1. Copy original blocks `0..outer_hdr`, redirecting `preheader`'s edge to `b_amac_entry`.
    for b in 0..outer_hdr_idx {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));
        let term = view.terminator(Block(b32));
        for inst in range {
            let redirect = if b32 == plan.preheader && inst == term {
                match plan.pre_edge {
                    PreheaderEdge::Jump | PreheaderEdge::BrThen => Some((0, b_amac_entry)),
                    PreheaderEdge::BrElse => Some((1, b_amac_entry)),
                }
            } else {
                None
            };
            let mapped = copy_orig_inst(view, inst, &val_map, &blk_map, redirect, &mut func, &mut fixups);
            set_at(&mut val_map, inst, mapped);
        }
    }

    let bound_new = at_u32(&val_map, to_idx(plan.bound));

    // 2. Emit `b_amac_entry`: overflow-free check `q0 < bound && (bound - q0) >= K`.
    let _ = begin_block(&mut func, BlockKind::Normal);
    let mut entry_params_new: Vec<u32, A> = Vec::with_capacity_in(n_op, alloc);
    for p in 0..n_op {
        let ty = Ty(at_u16(view.tys, oh_params.start + p));
        entry_params_new.push(push_param(&mut func, ty));
    }
    let q0 = at_u32(&entry_params_new, plan.iv_pos);
    let mut ptr_end_minus_k = NONE;
    let mut has_k = match plan.iv_kind {
        OuterIvKind::Counted { ty, signed } => {
            let lt_cond = if signed { IntCond::Slt } else { IntCond::Ult };
            let is_lt = push_icmp(&mut func, lt_cond, q0, bound_new);
            let diff = push_bin(&mut func, Op::Sub, Flags::NONE, ty, bound_new, q0);
            let kc = push_iconst(&mut func, ty, u64::from(to_u32(k_width)));
            let ge_k = push_icmp(&mut func, IntCond::Uge, diff, kc);
            push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, is_lt, ge_k)
        }
        OuterIvKind::Pointer { stride } => {
            let k_bytes = u64::from(to_u32(k_width)).saturating_mul(stride);
            let neg_k_bytes = push_iconst(&mut func, Ty::I64, (0u64).wrapping_sub(k_bytes));
            ptr_end_minus_k = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, bound_new, neg_k_bytes);
            if let Some(byte_len_new) = inbounds_ptr_byte_offset(view, plan.iv_init, plan.bound)
                .map(|bl| at_u32(&val_map, to_idx(bl)))
                .filter(|&v| v != NONE)
            {
                // Fast 2-check path for `bound = PtrAdd[INBOUNDS](iv_init, byte_len)` (all Rust
                // `slice::Iter` loops): `INBOUNDS` guarantees `q0` and `bound_new` share one
                // allocation without address wrap, so `q0 < bound_new && byte_len >= k_bytes` is
                // exact, provenance-safe, and `eval`-safe with zero `Op::PtrAddr` or selects.
                let is_lt = push_icmp(&mut func, IntCond::Ult, q0, bound_new);
                let k_bytes_c = push_iconst(&mut func, Ty::I64, k_bytes);
                let ge_k = push_icmp(&mut func, IntCond::Uge, byte_len_new, k_bytes_c);
                push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, is_lt, ge_k)
            } else {
                // Opaque-pointer fallback: check `q0 + k_width * stride <= bound_new` without
                // `Op::PtrAddr` and without ever comparing an out-of-bounds pointer by stepping
                // at most `k_width` times while `p < bound_new`.
                let step_c = push_iconst(&mut func, Ty::I64, stride);
                let mut p = q0;
                let mut ok = push_iconst(&mut func, Ty::I1, 1);
                for _ in 0..k_width {
                    let lt = push_icmp(&mut func, IntCond::Ult, p, bound_new);
                    ok = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, ok, lt);
                    let p_step = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, p, step_c);
                    p = push_select(&mut func, Ty::PTR, ok, p_step, p);
                }
                ok
            }
        }
    };
    // Hoist all pure outer-loop-invariant `outer_body` instructions once into `b_amac_entry` so
    // `inner_entry_guard` and `min_var_trip_bound` can read them directly and `b_amac_outer_body`
    // avoids re-evaluating them per K-query batch.
    let mut ob_inv_map = filled(len, NONE, &alloc);
    for (oi, &inst32) in plan.outer_body_insts.iter().enumerate() {
        if !at_bool(&plan.outer_body_inv, oi) {
            continue;
        }
        let inst = to_idx(inst32);
        let mapped = emit_lane_inst(view, inst, &ob_inv_map, &val_map, &mut func);
        set_at(&mut ob_inv_map, inst, mapped);
    }
    // Check outer-loop-invariant `inner_entry_guard` once in `b_amac_entry`: when false (e.g.
    // `table_len == 0`), jump straight to `orig_outer_hdr_new` (exact original scalar loop); when
    // true, every lane in `b_amac_outer_body` enters `b_amac_inner_hdr` unconditionally!
    if let Some((guard_cond, enter_is_then)) = plan.inner_entry_guard {
        let raw_guard = resolve_lane_val(guard_cond, &ob_inv_map, &val_map);
        let guard_ok = if enter_is_then {
            raw_guard
        } else {
            push_unary(&mut func, Op::Not, Ty::I1, raw_guard)
        };
        has_k = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_k, guard_ok);
    }
    // For variable-trip masked AMAC (e.g. `while lo < hi` binary search), skip AMAC overhead when
    // the searched table fits inside L1 cache (guided by `opts.min_var_trip_bound`).
    if !plan.inner_is_uniform && opts.min_var_trip_bound > 0 {
        for &orig_init in &plan.inner_init_args {
            let oi = to_idx(orig_init);
            let mapped = resolve_lane_val(orig_init, &ob_inv_map, &val_map);
            if mapped == NONE {
                continue;
            }
            let ity = Ty(at_u16(view.tys, oi));
            if matches!(ity, Ty::I64 | Ty::I32) && const_u64(view, oi).is_none() {
                let min_c = push_iconst(&mut func, ity, opts.min_var_trip_bound);
                let big_enough = push_icmp(&mut func, IntCond::Uge, mapped, min_c);
                has_k = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_k, big_enough);
                break;
            }
        }
    }
    push_br(
        &mut func,
        has_k,
        b_amac_outer_body,
        &entry_params_new,
        orig_outer_hdr_new,
        &entry_params_new,
    );

    // 3. Emit `b_amac_outer_body`: compute `q + 0..K`, run `K` copies of `outer_body`, enter K-way inner loop.
    let _ = begin_block(&mut func, BlockKind::Normal);
    let mut ob_params_new: Vec<u32, A> = Vec::with_capacity_in(n_op, alloc);
    for p in 0..n_op {
        let ty = Ty(at_u16(view.tys, oh_params.start + p));
        ob_params_new.push(push_param(&mut func, ty));
    }
    let q_cur = at_u32(&ob_params_new, plan.iv_pos);
    let mut q_lanes: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
    q_lanes.push(q_cur);
    for lane in 1..k_width {
        match plan.iv_kind {
            OuterIvKind::Counted { ty, .. } => {
                let off_c = push_iconst(&mut func, ty, u64::from(to_u32(lane)));
                q_lanes.push(push_bin(&mut func, Op::Add, Flags::NONE, ty, q_cur, off_c));
            }
            OuterIvKind::Pointer { stride } => {
                let off_bytes = u64::from(to_u32(lane)).saturating_mul(stride);
                let off_c = push_iconst(&mut func, Ty::I64, off_bytes);
                q_lanes.push(push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, q_cur, off_c));
            }
        }
    }

    let mut lane_maps: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(k_width, alloc);
    for lane in 0..k_width {
        let mut lm = filled(len, NONE, &alloc);
        set_at(&mut lm, to_idx(plan.iv_inst), at_u32(&q_lanes, lane));
        lane_maps.push(lm);
    }

    for (oi, &inst32) in plan.outer_body_insts.iter().enumerate() {
        if plan.skip_iv_next_in_ob && inst32 == plan.iv_next_inst {
            continue;
        }
        let inst = to_idx(inst32);
        if at_bool(&plan.outer_body_inv, oi) {
            // Already hoisted once into `b_amac_entry`; bind every lane to the hoisted SSA value.
            let hoisted = at_u32(&ob_inv_map, inst);
            for lm in lane_maps.iter_mut() {
                set_at(lm, inst, hoisted);
            }
        } else {
            // For pointer-IV outer loops loading directly from `[p + off]`, emit contiguous
            // `[q_cur + off + lane * stride]` loads so ARM64 `isel` pairs them into `ldp`.
            let mut emitted_direct_ptr_loads = false;
            if let OuterIvKind::Pointer { stride } = plan.iv_kind
                && view.op(inst) == Op::Load
            {
                let lat = to_idx(at_u32(view.data, inst));
                let orig_ptr = at_u32(view.extra, lat);
                let orig_off = at_u32(view.extra, lat + 1).cast_signed();
                if orig_ptr == plan.iv_inst {
                    let all_fit = (0..k_width).all(|lane| {
                        let off_bytes = u64::from(to_u32(lane)).saturating_mul(stride);
                        i32::try_from(off_bytes).ok().and_then(|d| orig_off.checked_add(d)).is_some()
                    });
                    if all_fit {
                        let ty = Ty(at_u16(view.tys, inst));
                        let attr = at_u32(view.attrs, inst);
                        for (lane, lm) in lane_maps.iter_mut().enumerate() {
                            let off_bytes = u64::from(to_u32(lane)).saturating_mul(stride);
                            let delta = i32::try_from(off_bytes).unwrap_or(0);
                            let total_off = orig_off.wrapping_add(delta);
                            let ld = push_load(&mut func, ty, q_cur, total_off, attr);
                            set_at(lm, inst, ld);
                        }
                        emitted_direct_ptr_loads = true;
                    }
                }
            }
            if !emitted_direct_ptr_loads {
                for lm in lane_maps.iter_mut() {
                    let mapped = emit_lane_inst(view, inst, lm, &val_map, &mut func);
                    set_at(lm, inst, mapped);
                }
            }
        }
    }

    // Pack initial inner-loop arguments: uniform params (1 each), non-uniform params (K each),
    // and for non-uniform `SingleBlock`: `active[0..K] = true` + `extra_latched[e][0..K] = dummy`.
    let mut inner_init_packed: Vec<u32, A> = Vec::new_in(alloc);
    for p in 0..n_ip {
        let orig_init = at_u32(&plan.inner_init_args, p);
        if at_bool(&plan.inner_param_uniform, p) {
            let v = match lane_maps.first() {
                Some(lm) => resolve_lane_val(orig_init, lm, &val_map),
                None => 0,
            };
            inner_init_packed.push(v);
        } else {
            for lm in lane_maps.iter() {
                inner_init_packed.push(resolve_lane_val(orig_init, lm, &val_map));
            }
        }
    }
    if matches!(plan.inner_shape, InnerShape::SingleBlock { .. }) && !plan.inner_is_uniform {
        let true_c = push_iconst(&mut func, Ty::I1, 1);
        for _ in 0..k_width {
            inner_init_packed.push(true_c);
        }
        for &ev in &plan.extra_latched {
            let ty = Ty(at_u16(view.tys, to_idx(ev)));
            let Some(dummy) = push_dummy_of_ty(&mut func, ty) else {
                return Err(SimplifyError::Refused { inst: to_idx(ev), what: "unsupported AMAC latched scalar type" });
            };
            for _ in 0..k_width {
                inner_init_packed.push(dummy);
            }
        }
    }

    // Any `inner_entry_guard` was already proven true in `b_amac_entry`, so all K lanes enter
    // `b_amac_inner_hdr` unconditionally with ZERO skip branch inside the AMAC loop.
    push_jump(&mut func, b_amac_inner_hdr, &inner_init_packed);

    // 4. Emit K-way inner loop (`HeaderBody`, `HeaderBranch`, or `SingleBlock`).
    let mut prefetches_inserted = 0u32;
    let ih_term = view.terminator(Block(inner_hdr));

    match &plan.inner_shape {
        InnerShape::HeaderBody { inner_body, inner_cond, inner_cond_inverted, .. }
        | InnerShape::HeaderBranch(InnerBranch { inner_body, inner_cond, inner_cond_inverted, .. }) => {
            let inner_body = *inner_body;
            let inner_cond = *inner_cond;
            let inner_cond_inverted = *inner_cond_inverted;
            let branch_opt = match &plan.inner_shape {
                InnerShape::HeaderBranch(br) => Some(br),
                _ => None,
            };
            // `b_amac_inner_hdr`: ZERO `active` block params!
            let _ = begin_block(&mut func, BlockKind::Normal);
            let mut inner_nu_params: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(n_ip, alloc);
            for p in 0..n_ip {
                let p_inst = ih_params.start + p;
                let ty = Ty(at_u16(view.tys, p_inst));
                let mut per_lane: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
                if at_bool(&plan.inner_param_uniform, p) {
                    let pv = push_param(&mut func, ty);
                    for lm in lane_maps.iter_mut() {
                        set_at(lm, p_inst, pv);
                    }
                    per_lane.push(pv);
                } else {
                    for lm in lane_maps.iter_mut() {
                        let pv = push_param(&mut func, ty);
                        set_at(lm, p_inst, pv);
                        per_lane.push(pv);
                    }
                }
                inner_nu_params.push(per_lane);
            }

            // Evaluate pure `inner_hdr` instructions (uniform once, non-uniform across `K` lanes).
            for inst in ih_params.end..ih_term {
                if view.op(inst) == Op::Assume {
                    continue;
                }
                if at_bool(&plan.inner_inst_uniform, inst) {
                    let mapped = match lane_maps.first() {
                        Some(lm) => emit_lane_inst(view, inst, lm, &val_map, &mut func),
                        None => 0,
                    };
                    for lm in lane_maps.iter_mut() {
                        set_at(lm, inst, mapped);
                    }
                } else {
                    for lm in lane_maps.iter_mut() {
                        let mapped = emit_lane_inst(view, inst, lm, &val_map, &mut func);
                        set_at(lm, inst, mapped);
                    }
                }
            }

            // Compute `keep_lanes[0..K]` and `any_active`.
            let mut keep_lanes: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
            let any_active = if plan.inner_is_uniform {
                let raw = match lane_maps.first() {
                    Some(lm) => resolve_lane_val(inner_cond, lm, &val_map),
                    None => 0,
                };
                let k0 = if inner_cond_inverted {
                    push_unary(&mut func, Op::Not, Ty::I1, raw)
                } else {
                    raw
                };
                for _ in 0..k_width {
                    keep_lanes.push(k0);
                }
                k0
            } else {
                for lm in lane_maps.iter() {
                    let raw = resolve_lane_val(inner_cond, lm, &val_map);
                    let kv = if inner_cond_inverted {
                        push_unary(&mut func, Op::Not, Ty::I1, raw)
                    } else {
                        raw
                    };
                    keep_lanes.push(kv);
                }
                let mut acc_any = at_u32(&keep_lanes, 0);
                for lane in 1..k_width {
                    acc_any = push_bin(&mut func, Op::Or, Flags::NONE, Ty::I1, acc_any, at_u32(&keep_lanes, lane));
                }
                acc_any
            };

            // Pack exit args for `b_amac_outer_latch`: `inner_exit_args` + `inner_direct_uses`.
            let mut latch_exit_packed: Vec<u32, A> = Vec::new_in(alloc);
            for &ea in &plan.inner_exit_args {
                for lm in lane_maps.iter() {
                    latch_exit_packed.push(resolve_lane_val(ea, lm, &val_map));
                }
            }
            for &du in &plan.inner_direct_uses {
                for lm in lane_maps.iter() {
                    latch_exit_packed.push(resolve_lane_val(du, lm, &val_map));
                }
            }
            push_br(&mut func, any_active, b_amac_inner_body, &[], b_amac_outer_latch, &latch_exit_packed);

            // `b_amac_inner_body`: lockstep load-wave execution + branch-to-select + masked state freeze!
            let _ = begin_block(&mut func, BlockKind::Normal);
            let ib_range = view.block(Block(inner_body));
            let direct_branch_to_hdr = branch_opt.is_some_and(|br| br.merge_blk.is_none());
            let mut branch_next: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(if direct_branch_to_hdr { n_ip } else { 0 }, alloc);
            if direct_branch_to_hdr {
                for _ in 0..n_ip {
                    branch_next.push(filled(k_width, 0u32, &alloc));
                }
            }
            prefetches_inserted += emit_lockstep_inner_waves(
                view,
                ib_range.start,
                ib_range.end - 1,
                k_width,
                plan.inner_is_uniform,
                &plan.inner_inst_uniform,
                &keep_lanes,
                prefetch_id,
                &val_map,
                &mut lane_maps,
                &mut func,
                alloc,
                |lane, lm, f| {
                    let Some(br) = branch_opt else {
                        return;
                    };
                    for opt_arm in [br.then_arm, br.else_arm] {
                        if let Some(arm) = opt_arm {
                            let r = view.block(Block(arm));
                            for inst in r.start..r.end.saturating_sub(1) {
                                if view.op(inst) == Op::Assume {
                                    continue;
                                }
                                let mapped = emit_lane_inst(view, inst, lm, &val_map, f);
                                set_at(lm, inst, mapped);
                            }
                        }
                    }
                    let bcond = resolve_lane_val(br.branch_cond, lm, &val_map);
                    if let Some(mb) = br.merge_blk {
                        let mb_params = view.params(Block(mb));
                        for k in 0..mb_params.len() {
                            let tv = resolve_lane_val(at_u32(&br.then_join_args, k), lm, &val_map);
                            let ev = resolve_lane_val(at_u32(&br.else_join_args, k), lm, &val_map);
                            let sel = if tv == ev {
                                tv
                            } else {
                                let ty = Ty(at_u16(view.tys, mb_params.start + k));
                                push_select(f, ty, bcond, tv, ev)
                            };
                            set_at(lm, mb_params.start + k, sel);
                        }
                        let mb_term = view.terminator(Block(mb));
                        for inst in mb_params.end..mb_term {
                            if view.op(inst) == Op::Assume {
                                continue;
                            }
                            let mapped = emit_lane_inst(view, inst, lm, &val_map, f);
                            set_at(lm, inst, mapped);
                        }
                    } else {
                        for p in 0..n_ip {
                            let tv = resolve_lane_val(at_u32(&br.then_join_args, p), lm, &val_map);
                            let ev = resolve_lane_val(at_u32(&br.else_join_args, p), lm, &val_map);
                            let nv = if tv == ev {
                                tv
                            } else {
                                let ty = Ty(at_u16(view.tys, ih_params.start + p));
                                push_select(f, ty, bcond, tv, ev)
                            };
                            if let Some(row) = branch_next.get_mut(p) {
                                set_at(row, lane, nv);
                            }
                        }
                    }
                },
            );

            // Pack back-edge state updates (freeze inactive lanes with `select(keep[k], next, cur)`).
            let resolve_next = |p: usize, lane: usize| -> u32 {
                if direct_branch_to_hdr {
                    branch_next.get(p).map_or(0, |row| at_u32(row, lane))
                } else {
                    let orig_next = at_u32(&plan.inner_next_args, p);
                    match lane_maps.get(lane) {
                        Some(lm) => resolve_lane_val(orig_next, lm, &val_map),
                        None => 0,
                    }
                }
            };
            let mut back_packed: Vec<u32, A> = Vec::new_in(alloc);
            for p in 0..n_ip {
                let ty = Ty(at_u16(view.tys, ih_params.start + p));
                if at_bool(&plan.inner_param_uniform, p) {
                    back_packed.push(resolve_next(p, 0));
                } else if plan.inner_is_uniform {
                    for lane in 0..k_width {
                        back_packed.push(resolve_next(p, lane));
                    }
                } else {
                    for lane in 0..k_width {
                        let nv = resolve_next(p, lane);
                        let cur_v = inner_nu_params.get(p).map_or(0, |v| at_u32(v, lane));
                        let act = at_u32(&keep_lanes, lane);
                        back_packed.push(push_select(&mut func, ty, act, nv, cur_v));
                    }
                }
            }
            push_jump(&mut func, b_amac_inner_hdr, &back_packed);
        }
        InnerShape::SingleBlock { inner_cond, inner_cond_inverted, .. } => {
            let inner_cond = *inner_cond;
            let inner_cond_inverted = *inner_cond_inverted;
            let _ = begin_block(&mut func, BlockKind::Normal);
            let mut inner_nu_params: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(n_ip, alloc);
            for p in 0..n_ip {
                let p_inst = ih_params.start + p;
                let ty = Ty(at_u16(view.tys, p_inst));
                let mut per_lane: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
                if at_bool(&plan.inner_param_uniform, p) {
                    let pv = push_param(&mut func, ty);
                    for lm in lane_maps.iter_mut() {
                        set_at(lm, p_inst, pv);
                    }
                    per_lane.push(pv);
                } else {
                    for lm in lane_maps.iter_mut() {
                        let pv = push_param(&mut func, ty);
                        set_at(lm, p_inst, pv);
                        per_lane.push(pv);
                    }
                }
                inner_nu_params.push(per_lane);
            }

            let mut active_params: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
            let mut latched_params: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(plan.extra_latched.len(), alloc);
            if !plan.inner_is_uniform {
                for _ in 0..k_width {
                    active_params.push(push_param(&mut func, Ty::I1));
                }
                for &ev in &plan.extra_latched {
                    let ty = Ty(at_u16(view.tys, to_idx(ev)));
                    let mut per_lane: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
                    for _ in 0..k_width {
                        per_lane.push(push_param(&mut func, ty));
                    }
                    latched_params.push(per_lane);
                }
            }

            prefetches_inserted += emit_lockstep_inner_waves(
                view,
                ih_params.end,
                ih_term,
                k_width,
                plan.inner_is_uniform,
                &plan.inner_inst_uniform,
                &active_params,
                prefetch_id,
                &val_map,
                &mut lane_maps,
                &mut func,
                alloc,
                |_, _, _| {},
            );

            if plan.inner_is_uniform {
                let raw = match lane_maps.first() {
                    Some(lm) => resolve_lane_val(inner_cond, lm, &val_map),
                    None => 0,
                };
                let any_active = if inner_cond_inverted {
                    push_unary(&mut func, Op::Not, Ty::I1, raw)
                } else {
                    raw
                };
                let mut back_packed: Vec<u32, A> = Vec::new_in(alloc);
                for p in 0..n_ip {
                    let orig_next = at_u32(&plan.inner_next_args, p);
                    if at_bool(&plan.inner_param_uniform, p) {
                        let nv = match lane_maps.first() {
                            Some(lm) => resolve_lane_val(orig_next, lm, &val_map),
                            None => 0,
                        };
                        back_packed.push(nv);
                    } else {
                        for lm in lane_maps.iter() {
                            back_packed.push(resolve_lane_val(orig_next, lm, &val_map));
                        }
                    }
                }
                let mut latch_exit_packed: Vec<u32, A> = Vec::new_in(alloc);
                for &ea in &plan.inner_exit_args {
                    for lm in lane_maps.iter() {
                        latch_exit_packed.push(resolve_lane_val(ea, lm, &val_map));
                    }
                }
                for &du in &plan.inner_direct_uses {
                    for lm in lane_maps.iter() {
                        latch_exit_packed.push(resolve_lane_val(du, lm, &val_map));
                    }
                }
                push_br(
                    &mut func,
                    any_active,
                    b_amac_inner_hdr,
                    &back_packed,
                    b_amac_outer_latch,
                    &latch_exit_packed,
                );
            } else {
                // Masked state updates + `next_active[k] = active[k] & keep[k]`.
                let mut next_nu_vals: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(n_ip, alloc);
                let mut back_packed: Vec<u32, A> = Vec::new_in(alloc);
                for p in 0..n_ip {
                    let orig_next = at_u32(&plan.inner_next_args, p);
                    let ty = Ty(at_u16(view.tys, ih_params.start + p));
                    let mut per_lane: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
                    if at_bool(&plan.inner_param_uniform, p) {
                        let nv = match lane_maps.first() {
                            Some(lm) => resolve_lane_val(orig_next, lm, &val_map),
                            None => 0,
                        };
                        back_packed.push(nv);
                        per_lane.push(nv);
                    } else {
                        for lane in 0..k_width {
                            let nv = match lane_maps.get(lane) {
                                Some(lm) => resolve_lane_val(orig_next, lm, &val_map),
                                None => 0,
                            };
                            let cur_v = inner_nu_params.get(p).map_or(0, |v| at_u32(v, lane));
                            let act = at_u32(&active_params, lane);
                            let g = push_select(&mut func, ty, act, nv, cur_v);
                            back_packed.push(g);
                            per_lane.push(g);
                        }
                    }
                    next_nu_vals.push(per_lane);
                }

                let mut next_active: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
                for lane in 0..k_width {
                    let raw = match lane_maps.get(lane) {
                        Some(lm) => resolve_lane_val(inner_cond, lm, &val_map),
                        None => 0,
                    };
                    let keep = if inner_cond_inverted {
                        push_unary(&mut func, Op::Not, Ty::I1, raw)
                    } else {
                        raw
                    };
                    let na = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, at_u32(&active_params, lane), keep);
                    next_active.push(na);
                    back_packed.push(na);
                }

                let mut next_latched: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(plan.extra_latched.len(), alloc);
                for (ei, &ev) in plan.extra_latched.iter().enumerate() {
                    let ty = Ty(at_u16(view.tys, to_idx(ev)));
                    let mut per_lane: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
                    for lane in 0..k_width {
                        let cur_ev = match lane_maps.get(lane) {
                            Some(lm) => resolve_lane_val(ev, lm, &val_map),
                            None => 0,
                        };
                        let prev_ev = latched_params.get(ei).map_or(0, |v| at_u32(v, lane));
                        let act = at_u32(&active_params, lane);
                        let lv = push_select(&mut func, ty, act, cur_ev, prev_ev);
                        per_lane.push(lv);
                        back_packed.push(lv);
                    }
                    next_latched.push(per_lane);
                }

                let mut any_active = at_u32(&next_active, 0);
                for lane in 1..k_width {
                    any_active =
                        push_bin(&mut func, Op::Or, Flags::NONE, Ty::I1, any_active, at_u32(&next_active, lane));
                }

                // Resolve every exit value from frozen `next_nu_vals`, `next_latched`, or outer-body/invariant map.
                let mut latch_exit_packed: Vec<u32, A> = Vec::new_in(alloc);
                let mut push_exit_val = |v: u32| {
                    if let Some(p) = (0..n_ip)
                        .find(|&p| !at_bool(&plan.inner_param_uniform, p) && at_u32(&plan.inner_next_args, p) == v)
                    {
                        for lane in 0..k_width {
                            latch_exit_packed.push(next_nu_vals.get(p).map_or(0, |row| at_u32(row, lane)));
                        }
                    } else if let Some(ei) = plan.extra_latched.scan_for(&v) {
                        for lane in 0..k_width {
                            latch_exit_packed.push(next_latched.get(ei).map_or(0, |row| at_u32(row, lane)));
                        }
                    } else {
                        for lm in lane_maps.iter() {
                            latch_exit_packed.push(resolve_lane_val(v, lm, &val_map));
                        }
                    }
                };
                for &ea in &plan.inner_exit_args {
                    push_exit_val(ea);
                }
                for &du in &plan.inner_direct_uses {
                    push_exit_val(du);
                }

                push_br(
                    &mut func,
                    any_active,
                    b_amac_inner_hdr,
                    &back_packed,
                    b_amac_outer_latch,
                    &latch_exit_packed,
                );
            }
        }
    }

    // 5. Emit `b_amac_outer_latch`: post-process K lanes, emit unit-stride stores, reduce K values, advance `q += K`.
    let _ = begin_block(&mut func, BlockKind::Normal);
    for j in 0..n_olp {
        let p_inst = ol_params.start + j;
        let ty = Ty(at_u16(view.tys, p_inst));
        for lm in lane_maps.iter_mut() {
            let pv = push_param(&mut func, ty);
            set_at(lm, p_inst, pv);
        }
    }
    for &du in &plan.inner_direct_uses {
        let ty = Ty(at_u16(view.tys, to_idx(du)));
        for lm in lane_maps.iter_mut() {
            let pv = push_param(&mut func, ty);
            set_at(lm, to_idx(du), pv);
        }
    }

    // Advance `q_next = q_cur + K` (or `ptr_add(p_cur, K * stride)`) up front so any `outer_latch`
    // reference to `iv_next_inst` maps cleanly.
    let (k_c_latch, q_next, diff_ty) = match plan.iv_kind {
        OuterIvKind::Counted { ty, .. } => {
            let kc = push_iconst(&mut func, ty, u64::from(to_u32(k_width)));
            let qn = push_bin(&mut func, Op::Add, Flags::NONE, ty, q_cur, kc);
            (kc, qn, ty)
        }
        OuterIvKind::Pointer { stride } => {
            let k_bytes = u64::from(to_u32(k_width)).saturating_mul(stride);
            let kc = push_iconst(&mut func, Ty::I64, k_bytes);
            let qn = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, q_cur, kc);
            (kc, qn, Ty::I64)
        }
    };
    for lane in 0..k_width {
        let next_lane_q = if lane + 1 < k_width {
            at_u32(&q_lanes, lane + 1)
        } else {
            q_next
        };
        if let Some(lm) = lane_maps.get_mut(lane) {
            set_at(lm, to_idx(plan.iv_next_inst), next_lane_q);
        }
    }

    // Execute `outer_latch_insts` sequentially across lanes `0..K` (exact sequential store order!).
    for lane in 0..k_width {
        if let Some(lm) = lane_maps.get_mut(lane) {
            for &inst32 in &plan.outer_latch_insts {
                let inst = to_idx(inst32);
                let mapped = emit_lane_inst(view, inst, lm, &val_map, &mut func);
                set_at(lm, inst, mapped);
            }
        }
    }

    // Build next outer header args: `q_next` + balanced-tree reductions across all `K` lanes.
    let mut next_outer_args = filled(n_op, 0u32, &alloc);
    set_at(&mut next_outer_args, plan.iv_pos, q_next);
    let mut red_next_vals: Vec<u32, A> = Vec::with_capacity_in(plan.outer_reductions.len(), alloc);
    for red in &plan.outer_reductions {
        let mut contribs: Vec<u32, A> = Vec::with_capacity_in(k_width, alloc);
        for lm in lane_maps.iter() {
            contribs.push(resolve_lane_val(red.contrib_val, lm, &val_map));
        }
        let acc_cur = at_u32(&ob_params_new, red.param_pos);
        let acc_next = emit_assoc_tree_reduction(&mut func, red.op, red.ty, acc_cur, &contribs, alloc);
        set_at(&mut next_outer_args, red.param_pos, acc_next);
        red_next_vals.push(acc_next);
    }

    let (rem_opt, more_k) = match plan.iv_kind {
        OuterIvKind::Counted { ty, .. } => {
            let rem = push_bin(&mut func, Op::Sub, Flags::NONE, ty, bound_new, q_next);
            let mk = push_icmp(&mut func, IntCond::Uge, rem, k_c_latch);
            (Some(rem), mk)
        }
        OuterIvKind::Pointer { .. } => {
            // Since `b_amac_outer_body` is only reached when `bound_new - q0 >= k_bytes`,
            // `ptr_end_minus_k` lies in `[q0, bound_new]` and `q_next <= bound_new`, so this
            // single comparison is both wrap-free and strictly within provenance bounds.
            let mk = push_icmp(&mut func, IntCond::Ule, q_next, ptr_end_minus_k);
            (None, mk)
        }
    };

    match plan.outer_shape {
        OuterShape::HeaderBody { .. } => {
            push_br(
                &mut func,
                more_k,
                b_amac_outer_body,
                &next_outer_args,
                orig_outer_hdr_new,
                &next_outer_args,
            );
        }
        OuterShape::Rotated { ref outer_exit_args, .. } => {
            push_br(&mut func, more_k, b_amac_outer_body, &next_outer_args, b_amac_tail_check, &[]);

            // 6. Emit `b_amac_tail_check`: skip scalar do-while tail completely when `rem == 0`.
            let _ = begin_block(&mut func, BlockKind::Normal);
            let has_tail = match rem_opt {
                Some(rem) => {
                    let zero_c = push_iconst(&mut func, diff_ty, 0);
                    push_icmp(&mut func, IntCond::Ne, rem, zero_c)
                }
                None => push_icmp(&mut func, IntCond::Ult, q_next, bound_new),
            };
            let mut mapped_exit_args: Vec<u32, A> = Vec::with_capacity_in(outer_exit_args.len(), alloc);
            for &ev in outer_exit_args {
                let mapped = if ev == plan.iv_next_inst {
                    q_next
                } else if let Some(ri) = plan.outer_reductions.iter().position(|r| r.update_inst == ev) {
                    at_u32(&red_next_vals, ri)
                } else {
                    at_u32(&val_map, to_idx(ev))
                };
                mapped_exit_args.push(mapped);
            }
            push_br(
                &mut func,
                has_tail,
                orig_outer_hdr_new,
                &next_outer_args,
                orig_outer_exit_new,
                &mapped_exit_args,
            );
        }
    }

    // 7. Copy untouched original blocks `outer_hdr..blocks` (scalar tail loop + exits + sibling arms).
    for b in outer_hdr_idx..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));
        for inst in range {
            let mapped = copy_orig_inst(view, inst, &val_map, &blk_map, None, &mut func, &mut fixups);
            set_at(&mut val_map, inst, mapped);
        }
    }

    // Resolve any deferred forward-layout value operands.
    for fixup in &fixups {
        match *fixup {
            ValFixup::Data { inst, old_val } => {
                set_at(&mut func.data, inst, at_u32(&val_map, to_idx(old_val)));
            }
            ValFixup::Extra { pos, old_val } => {
                set_at(&mut func.extra, pos, at_u32(&val_map, to_idx(old_val)));
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok((func, prefetches_inserted))
}

/// Compiler-synthesized AMAC pass: detects eligible 2-level nested pointer-chasing / data-dependent
/// lookup loops and rewrites them into K-way lockstep interleaved loops with parallel
/// `"blaze.prefetch.r1"` hints, memory-safe active-lane masking (zero masking on uniform fixed-trip
/// inner loops), and an exact scalar tail loop.
pub fn interleave_amac_loops<A: Allocator + Copy>(
    view: View<'_>,
    opts: AmacOpts,
    alloc: A,
) -> Result<AmacOutput<A>, SimplifyError> {
    if opts.group_width < 2 || opts.max_state_regs < 2 || view.head.ptr_bits != 64 {
        return Ok(AmacOutput { func: view.to_func_in(alloc), stats: AmacStats::default() });
    }

    let mut cur_func: Option<Func<A>> = None;
    let mut stats = AmacStats::default();

    for _ in 0..MAX_NESTS_PER_FUNC {
        let cur_view = match &cur_func {
            Some(f) => f.view(),
            None => view,
        };
        let Some(plan) = detect_amac_nest(&cur_view, opts, alloc) else {
            break;
        };
        let used_k = to_u32(plan.group_width);
        let (next_func, pfs) = rewrite_amac_nest(&cur_view, &plan, opts, alloc)?;
        stats.nests_interleaved += 1;
        stats.group_width_used = used_k;
        stats.prefetches_inserted += pfs;
        cur_func = Some(next_func);
    }

    let func = match cur_func {
        Some(f) => f,
        None => view.to_func_in(alloc),
    };
    Ok(AmacOutput { func, stats })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::SlotKind,
        simplify::{ifconv::if_convert, unroll::rotate_loops, unswitch::unswitch_loops},
    };

    const EVAL_FUEL: u64 = 500_000;

    /// Strips ONLY no-effect `TargetIntrinsic("blaze.prefetch.r1", _)` instructions from a verified
    /// function so the `emit_prefetch: true` output can also be executed directly in `eval::run_in`
    /// (whose `eval/target.rs` only whitelists `llvm.aarch64.{isb,dmb,dsb,hint}`).
    fn strip_prefetch_intrinsics(view: View<'_>) -> Func<TestHeap> {
        let mut func = view.to_func_in(TestHeap);
        clear_code_columns(&mut func);
        let len = view.len();
        let blocks = view.blocks();
        let mut val_map = filled(len, NONE, &TestHeap);
        let mut blk_map = filled(blocks, 0u32, &TestHeap);
        for b in 0..blocks {
            blk_map[b] = b as u32;
        }
        let mut fixups: Vec<ValFixup, TestHeap> = Vec::new_in(TestHeap);
        for b in 0..blocks {
            func.block_starts.push(func.ops.len() as u32);
            func.block_kinds.push(view.block_kinds[b]);
            for inst in view.block(Block(b as u32)) {
                let is_pf = view.op(inst) == Op::TargetIntrinsic
                    && view.intrinsic_name(IntrinsicId(view.extra[view.data[inst] as usize]))
                        == PREFETCH_READ_L1_INTRINSIC.as_bytes();
                if is_pf {
                    continue;
                }
                let mapped = copy_orig_inst(&view, inst, &val_map, &blk_map, None, &mut func, &mut fixups);
                val_map[inst] = mapped;
            }
        }
        for fixup in &fixups {
            match *fixup {
                ValFixup::Data { inst, old_val } => func.data[inst] = val_map[old_val as usize],
                ValFixup::Extra { pos, old_val } => func.extra[pos] = val_map[old_val as usize],
            }
        }
        verify(func.view()).expect("stripped AMAC function must verify");
        func
    }

    fn count_prefetches(view: View<'_>) -> usize {
        (0..view.len())
            .filter(|&k| {
                view.op(k) == Op::TargetIntrinsic
                    && view.intrinsic_name(IntrinsicId(view.extra[view.data[k] as usize]))
                        == PREFETCH_READ_L1_INTRINSIC.as_bytes()
            })
            .count()
    }

    /// Generates a sorted `i64` table of length `n` (with duplicates & gaps) and `q_max` queries
    /// hitting `< table[0]`, exact matches, duplicate runs, between-element misses, and `> table[n-1]`
    /// (which specifically drives `lo == hi == n` to stress out-of-bounds load safety!).
    fn make_table_and_queries(n: usize, q_max: usize) -> (Vec<u64, TestHeap>, Vec<u64, TestHeap>) {
        let mut table = Vec::with_capacity_in(n, TestHeap);
        for i in 0..n {
            // Intentional duplicate runs (e.g., i=3,4) and gaps.
            let v = ((i as u64) - ((i as u64) % 3 == 2) as u64) * 10 + 10;
            table.push(v);
        }
        let max_v = table.last().copied().unwrap_or(0);
        let mut queries = Vec::with_capacity_in(q_max, TestHeap);
        for q in 0..q_max {
            let key = match q % 7 {
                0 => 0,                         // strictly < table[0]
                1 => max_v.saturating_add(50),  // strictly > table[n-1] -> lo == hi == n!
                2 => table.get(q % n.max(1)).copied().unwrap_or(0), // exact match
                3 => table.get((q * 3) % n.max(1)).copied().unwrap_or(0).saturating_add(1), // miss between
                4 => max_v.saturating_add(999), // strictly > table[n-1] -> lo == hi == n!
                5 => table.first().copied().unwrap_or(0), // first element
                _ => table.last().copied().unwrap_or(0),  // last element
            };
            queries.push(key);
        }
        (table, queries)
    }

    /// Builds a real `bsearch_batch` function with exact-sized stack slots (`n * 8` bytes for `table`,
    /// `q_max * 8` bytes for `queries`) so ANY out-of-bounds read traps immediately in `eval::run_in`!
    fn build_bsearch_batch_func(table: &[u64], queries: &[u64]) -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let table_bytes = (table.len().max(1) * 8) as u32;
        let query_bytes = (queries.len().max(1) * 8) as u32;
        let arr_slot = b.slot(table_bytes, 3, SlotKind::Temp);
        let qry_slot = b.slot(query_bytes, 3, SlotKind::Temp);
        let [entry, h_q, b_q, h_bs, b_bs, end_bs, exit_all] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n_q = b.param(Ty::I64);
        let n_arr = b.param(Ty::I64);
        let arr_p = b.slot_addr(arr_slot);
        let qry_p = b.slot_addr(qry_slot);
        for (k, &v) in table.iter().enumerate() {
            let cv = b.iconst(Ty::I64, u128::from(v));
            b.store(Flags::NONE, cv, arr_p, (k * 8) as i32, MemAttrs::aligned(3));
        }
        for (k, &v) in queries.iter().enumerate() {
            let cv = b.iconst(Ty::I64, u128::from(v));
            b.store(Flags::NONE, cv, qry_p, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        b.jump((h_q, &[zero, zero]));

        b.begin(h_q);
        let q = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep_q = b.icmp(IntCond::Ult, q, n_q);
        b.br(Flags::NONE, keep_q, (b_q, &[]), (exit_all, &[acc]));

        b.begin(b_q);
        let q_off = b.binary(Op::Shl, Flags::NONE, q, three);
        let qp = b.ptr_add(Flags::NONE, qry_p, q_off);
        let key = b.load(Ty::I64, Flags::NONE, qp, 0, MemAttrs::aligned(3));
        b.jump((h_bs, &[zero, n_arr]));

        b.begin(h_bs);
        let lo = b.param(Ty::I64);
        let hi = b.param(Ty::I64);
        let keep_bs = b.icmp(IntCond::Ult, lo, hi);
        b.br(Flags::NONE, keep_bs, (b_bs, &[]), (end_bs, &[lo]));

        b.begin(b_bs);
        let sum_lh = b.binary(Op::Add, Flags::NONE, lo, hi);
        let mid = b.binary(Op::Lshr, Flags::NONE, sum_lh, one);
        let m_off = b.binary(Op::Shl, Flags::NONE, mid, three);
        let mp = b.ptr_add(Flags::NONE, arr_p, m_off);
        let mv = b.load(Ty::I64, Flags::NONE, mp, 0, MemAttrs::aligned(3));
        let lt = b.icmp(IntCond::Ult, mv, key);
        let mid1 = b.binary(Op::Add, Flags::NONE, mid, one);
        let lo2 = b.select(Flags::NONE, lt, mid1, lo);
        let hi2 = b.select(Flags::NONE, lt, hi, mid);
        b.jump((h_bs, &[lo2, hi2]));

        b.begin(end_bs);
        let res_lo = b.param(Ty::I64);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, res_lo);
        let q2 = b.binary(Op::Add, Flags::NONE, q, one);
        b.jump((h_q, &[q2, acc2]));

        b.begin(exit_all);
        let final_acc = b.param(Ty::I64);
        b.ret(&[final_acc]);
        b.finish_checked().unwrap()
    }

    fn eval_u64(view: View<'_>, args: &[Val]) -> u64 {
        match eval::run_in(view, args, EVAL_FUEL, TestHeap).expect("evaluator must succeed") {
            Outcome::Returned(rets) => match rets[..] {
                [Val::Int(v)] => v as u64,
                _ => panic!("unexpected return values: {rets:?}"),
            },
            other => panic!("unexpected outcome (possible OOB load or trap!): {other:?}"),
        }
    }

    #[test]
    fn amac_bsearch_header_body_arm64_wide8_and_eval_equivalence() {
        for &n in &[0usize, 1, 7, 13, 64] {
            let q_max = 32usize;
            let (table, queries) = make_table_and_queries(n, q_max);
            let func = build_bsearch_batch_func(&table, &queries);

            // 1. Default ARM64 4-way with prefetches enabled.
            let out_pf = interleave_amac_loops(func.view(), AmacOpts::arm64(), TestHeap).unwrap();
            verify(out_pf.func.view()).expect("AMAC arm64 output must verify");
            assert_eq!(out_pf.stats.nests_interleaved, 1);
            assert_eq!(out_pf.stats.group_width_used, 4);
            assert!(
                out_pf.stats.prefetches_inserted >= 4,
                "expected >= 4 prefetches, got {}",
                out_pf.stats.prefetches_inserted
            );
            assert_eq!(count_prefetches(out_pf.func.view()), out_pf.stats.prefetches_inserted as usize);

            // Idempotence: second AMAC pass must be a no-op!
            let again = interleave_amac_loops(out_pf.func.view(), AmacOpts::arm64(), TestHeap).unwrap();
            assert_eq!(again.stats.nests_interleaved, 0);

            // 2. 4-way without prefetches & 4-way with prefetches stripped & 8-way without prefetches.
            let out_4 = interleave_amac_loops(
                func.view(),
                AmacOpts { emit_prefetch: false, ..AmacOpts::arm64() },
                TestHeap,
            )
            .unwrap();
            verify(out_4.func.view()).expect("AMAC 4-way no-pf must verify");
            assert_eq!(out_4.stats.nests_interleaved, 1);
            assert_eq!(out_4.stats.group_width_used, 4);
            assert_eq!(out_4.stats.prefetches_inserted, 0);

            let out_4_stripped = strip_prefetch_intrinsics(out_pf.func.view());

            let out_8 = interleave_amac_loops(
                func.view(),
                AmacOpts { emit_prefetch: false, ..AmacOpts::wide_8() },
                TestHeap,
            )
            .unwrap();
            verify(out_8.func.view()).expect("AMAC 8-way no-pf must verify");
            assert_eq!(out_8.stats.nests_interleaved, 1);
            assert_eq!(out_8.stats.group_width_used, 8);

            // 3. Bit-exact equivalence & zero OOB loads across odd/even/boundary query counts!
            for &q in &[0usize, 1, 3, 4, 5, 11, 32] {
                let args = [Val::Int(q as u128), Val::Int(n as u128)];
                let expected = eval_u64(func.view(), &args);
                let got_4 = eval_u64(out_4.func.view(), &args);
                let got_4s = eval_u64(out_4_stripped.view(), &args);
                let got_8 = eval_u64(out_8.func.view(), &args);
                assert_eq!(expected, got_4, "4-way mismatch at N={n}, Q={q}");
                assert_eq!(expected, got_4s, "4-way pf-stripped mismatch at N={n}, Q={q}");
                assert_eq!(expected, got_8, "8-way mismatch at N={n}, Q={q}");
            }
        }
    }

    #[test]
    fn amac_bsearch_post_unswitch_and_post_rotate_single_block_eval_equivalence() {
        for &n in &[1usize, 7, 13, 64] {
            let q_max = 32usize;
            let (table, queries) = make_table_and_queries(n, q_max);
            let orig = build_bsearch_batch_func(&table, &queries);

            // Path A: post-`unswitch_loops` + `if_convert` (`HeaderBody` outer with `Br` preheader + `SingleBlock` inner).
            let unsw = unswitch_loops(orig.view(), 2, TestHeap).unwrap();
            let ifc = if_convert(unsw.func.view(), 8, TestHeap).unwrap();
            let amac_unsw_pf = interleave_amac_loops(ifc.func.view(), AmacOpts::arm64(), TestHeap).unwrap();
            verify(amac_unsw_pf.func.view()).expect("post-unswitch AMAC must verify");
            assert_eq!(amac_unsw_pf.stats.nests_interleaved, 1);
            assert!(amac_unsw_pf.stats.prefetches_inserted >= 4);
            let amac_unsw_eval = strip_prefetch_intrinsics(amac_unsw_pf.func.view());

            // Path B: post-`rotate_loops` (`Rotated` outer + `SingleBlock` inner with outer-invariant guard).
            let rot = rotate_loops(orig.view(), TestHeap).unwrap();
            let amac_rot_pf = interleave_amac_loops(rot.func.view(), AmacOpts::arm64(), TestHeap).unwrap();
            verify(amac_rot_pf.func.view()).expect("post-rotate AMAC must verify");
            assert_eq!(amac_rot_pf.stats.nests_interleaved, 1);
            assert!(amac_rot_pf.stats.prefetches_inserted >= 4);
            let amac_rot_eval = strip_prefetch_intrinsics(amac_rot_pf.func.view());

            // Verify idempotence on both shapes.
            assert_eq!(
                interleave_amac_loops(amac_unsw_pf.func.view(), AmacOpts::arm64(), TestHeap)
                    .unwrap()
                    .stats
                    .nests_interleaved,
                0
            );
            assert_eq!(
                interleave_amac_loops(amac_rot_pf.func.view(), AmacOpts::arm64(), TestHeap)
                    .unwrap()
                    .stats
                    .nests_interleaved,
                0
            );

            for &q in &[0usize, 1, 3, 4, 5, 11, 32] {
                let args = [Val::Int(q as u128), Val::Int(n as u128)];
                let expected = eval_u64(orig.view(), &args);
                let got_unsw = eval_u64(amac_unsw_eval.view(), &args);
                let got_rot = eval_u64(amac_rot_eval.view(), &args);
                assert_eq!(expected, got_unsw, "post-unswitch AMAC mismatch at N={n}, Q={q}");
                assert_eq!(expected, got_rot, "post-rotate AMAC mismatch at N={n}, Q={q}");
            }
        }
    }

    #[test]
    fn amac_fixed_trip_branchless_bsearch_has_zero_active_masking_and_matches_eval() {
        // Shar's branchless binary search: `len` is lane-uniform (`len = n; while len > 1 { half = len >> 1; ... len -= half; }`).
        for &n in &[1usize, 7, 13, 16, 64] {
            let q_max = 32usize;
            let (table, queries) = make_table_and_queries(n, q_max);

            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let arr_slot = b.slot((n * 8) as u32, 3, SlotKind::Temp);
            let qry_slot = b.slot((q_max * 8) as u32, 3, SlotKind::Temp);
            let [entry, h_q, b_q, h_bs, b_bs, end_bs, exit_all] =
                [(); 7].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n_q = b.param(Ty::I64);
            let n_arr = b.param(Ty::I64);
            let arr_p = b.slot_addr(arr_slot);
            let qry_p = b.slot_addr(qry_slot);
            for (k, &v) in table.iter().enumerate() {
                let cv = b.iconst(Ty::I64, u128::from(v));
                b.store(Flags::NONE, cv, arr_p, (k * 8) as i32, MemAttrs::aligned(3));
            }
            for (k, &v) in queries.iter().enumerate() {
                let cv = b.iconst(Ty::I64, u128::from(v));
                b.store(Flags::NONE, cv, qry_p, (k * 8) as i32, MemAttrs::aligned(3));
            }
            let zero = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            let three = b.iconst(Ty::I64, 3);
            b.jump((h_q, &[zero, zero]));

            b.begin(h_q);
            let q = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep_q = b.icmp(IntCond::Ult, q, n_q);
            b.br(Flags::NONE, keep_q, (b_q, &[]), (exit_all, &[acc]));

            b.begin(b_q);
            let q_off = b.binary(Op::Shl, Flags::NONE, q, three);
            let qp = b.ptr_add(Flags::NONE, qry_p, q_off);
            let key = b.load(Ty::I64, Flags::NONE, qp, 0, MemAttrs::aligned(3));
            b.jump((h_bs, &[zero, n_arr]));

            // Uniform condition: `len > 1` (`1 < len`)!
            b.begin(h_bs);
            let base = b.param(Ty::I64);
            let len = b.param(Ty::I64);
            let keep_bs = b.icmp(IntCond::Ugt, len, one);
            b.br(Flags::NONE, keep_bs, (b_bs, &[]), (end_bs, &[base]));

            b.begin(b_bs);
            let half = b.binary(Op::Lshr, Flags::NONE, len, one);
            let mid = b.binary(Op::Add, Flags::NONE, base, half);
            let m_off = b.binary(Op::Shl, Flags::NONE, mid, three);
            let mp = b.ptr_add(Flags::NONE, arr_p, m_off);
            let mv = b.load(Ty::I64, Flags::NONE, mp, 0, MemAttrs::aligned(3));
            let le = b.icmp(IntCond::Ule, mv, key);
            let base2 = b.select(Flags::NONE, le, mid, base);
            let len2 = b.binary(Op::Sub, Flags::NONE, len, half);
            b.jump((h_bs, &[base2, len2]));

            b.begin(end_bs);
            let res_base = b.param(Ty::I64);
            let acc2 = b.binary(Op::Xor, Flags::NONE, acc, res_base);
            let q2 = b.binary(Op::Add, Flags::NONE, q, one);
            b.jump((h_q, &[q2, acc2]));

            b.begin(exit_all);
            let final_acc = b.param(Ty::I64);
            b.ret(&[final_acc]);
            let orig = b.finish_checked().unwrap();

            let out_pf = interleave_amac_loops(orig.view(), AmacOpts::arm64(), TestHeap).unwrap();
            verify(out_pf.func.view()).expect("uniform AMAC must verify");
            assert_eq!(out_pf.stats.nests_interleaved, 1);
            assert!(out_pf.stats.prefetches_inserted >= 4);

            // Prove ZERO active-lane masking overhead: zero `Ty::PTR` selects and zero `Ty::I1` Or!
            let v = out_pf.func.view();
            assert_eq!(
                (0..v.len()).filter(|&i| v.op(i) == Op::Select && Ty(v.tys[i]) == Ty::PTR).count(),
                0,
                "fixed-trip uniform inner loop must emit ZERO pointer fallback selects"
            );
            assert_eq!(
                (0..v.len()).filter(|&i| v.op(i) == Op::Or && Ty(v.tys[i]) == Ty::I1).count(),
                0,
                "fixed-trip uniform inner loop must emit ZERO active-lane Or reductions"
            );

            let eval_fn = strip_prefetch_intrinsics(v);
            for &q in &[0usize, 1, 3, 4, 5, 11, 32] {
                let args = [Val::Int(q as u128), Val::Int(n as u128)];
                let expected = eval_u64(orig.view(), &args);
                let got = eval_u64(eval_fn.view(), &args);
                assert_eq!(expected, got, "uniform Shar bsearch mismatch at N={n}, Q={q}");
            }
        }
    }

    #[test]
    fn amac_unit_stride_output_store_noalias_matches_eval() {
        for &n in &[1usize, 7, 13] {
            let q_max = 17usize;
            let (table, queries) = make_table_and_queries(n, q_max);

            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let arr_slot = b.slot((n * 8) as u32, 3, SlotKind::Temp);
            let qry_slot = b.slot((q_max * 8) as u32, 3, SlotKind::Temp);
            let out_slot = b.slot((q_max * 8) as u32, 3, SlotKind::Temp);
            let [entry, h_q, b_q, h_bs, b_bs, end_bs, h_chk, b_chk, exit_all] =
                [(); 9].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n_q = b.param(Ty::I64);
            let n_arr = b.param(Ty::I64);
            let arr_p = b.slot_addr(arr_slot);
            let qry_p = b.slot_addr(qry_slot);
            let out_p = b.slot_addr(out_slot);
            for (k, &v) in table.iter().enumerate() {
                let cv = b.iconst(Ty::I64, u128::from(v));
                b.store(Flags::NONE, cv, arr_p, (k * 8) as i32, MemAttrs::aligned(3));
            }
            for (k, &v) in queries.iter().enumerate() {
                let cv = b.iconst(Ty::I64, u128::from(v));
                b.store(Flags::NONE, cv, qry_p, (k * 8) as i32, MemAttrs::aligned(3));
            }
            let zero = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            let three = b.iconst(Ty::I64, 3);
            let mult = b.iconst(Ty::I64, 131);
            b.jump((h_q, &[zero]));

            // Outer query loop with ZERO reductions and a unit-stride output store `out[q] = lo`.
            b.begin(h_q);
            let q = b.param(Ty::I64);
            let keep_q = b.icmp(IntCond::Ult, q, n_q);
            b.br(Flags::NONE, keep_q, (b_q, &[]), (h_chk, &[zero, zero]));

            b.begin(b_q);
            let q_off = b.binary(Op::Shl, Flags::NONE, q, three);
            let qp = b.ptr_add(Flags::NONE, qry_p, q_off);
            let key = b.load(Ty::I64, Flags::NONE, qp, 0, MemAttrs::aligned(3));
            b.jump((h_bs, &[zero, n_arr]));

            b.begin(h_bs);
            let lo = b.param(Ty::I64);
            let hi = b.param(Ty::I64);
            let keep_bs = b.icmp(IntCond::Ult, lo, hi);
            b.br(Flags::NONE, keep_bs, (b_bs, &[]), (end_bs, &[lo]));

            b.begin(b_bs);
            let sum_lh = b.binary(Op::Add, Flags::NONE, lo, hi);
            let mid = b.binary(Op::Lshr, Flags::NONE, sum_lh, one);
            let m_off = b.binary(Op::Shl, Flags::NONE, mid, three);
            let mp = b.ptr_add(Flags::NONE, arr_p, m_off);
            let mv = b.load(Ty::I64, Flags::NONE, mp, 0, MemAttrs::aligned(3));
            let lt = b.icmp(IntCond::Ult, mv, key);
            let mid1 = b.binary(Op::Add, Flags::NONE, mid, one);
            let lo2 = b.select(Flags::NONE, lt, mid1, lo);
            let hi2 = b.select(Flags::NONE, lt, hi, mid);
            b.jump((h_bs, &[lo2, hi2]));

            b.begin(end_bs);
            let res_lo = b.param(Ty::I64);
            let out_off = b.binary(Op::Shl, Flags::NONE, q, three);
            let op_ptr = b.ptr_add(Flags::NONE, out_p, out_off);
            b.store(Flags::NONE, res_lo, op_ptr, 0, MemAttrs::aligned(3));
            let q2 = b.binary(Op::Add, Flags::NONE, q, one);
            b.jump((h_q, &[q2]));

            // Order-sensitive polynomial hash over `out[0..n_q]` verifies exact per-index store placement!
            b.begin(h_chk);
            let i = b.param(Ty::I64);
            let hash = b.param(Ty::I64);
            let keep_c = b.icmp(IntCond::Ult, i, n_q);
            b.br(Flags::NONE, keep_c, (b_chk, &[]), (exit_all, &[hash]));

            b.begin(b_chk);
            let ioff = b.binary(Op::Shl, Flags::NONE, i, three);
            let ip = b.ptr_add(Flags::NONE, out_p, ioff);
            let ov = b.load(Ty::I64, Flags::NONE, ip, 0, MemAttrs::aligned(3));
            let hm = b.binary(Op::Mul, Flags::NONE, hash, mult);
            let h2 = b.binary(Op::Add, Flags::NONE, hm, ov);
            let i2 = b.binary(Op::Add, Flags::NONE, i, one);
            b.jump((h_chk, &[i2, h2]));

            b.begin(exit_all);
            let final_hash = b.param(Ty::I64);
            b.ret(&[final_hash]);
            let orig = b.finish_checked().unwrap();

            let out_pf = interleave_amac_loops(orig.view(), AmacOpts::arm64(), TestHeap).unwrap();
            verify(out_pf.func.view()).expect("store-output AMAC must verify");
            assert_eq!(out_pf.stats.nests_interleaved, 1);
            assert!(out_pf.stats.prefetches_inserted >= 4);

            let eval_fn = strip_prefetch_intrinsics(out_pf.func.view());
            for &q in &[0usize, 1, 3, 4, 5, 11, 17] {
                let args = [Val::Int(q as u128), Val::Int(n as u128)];
                let expected = eval_u64(orig.view(), &args);
                let got = eval_u64(eval_fn.view(), &args);
                assert_eq!(expected, got, "unit-stride store AMAC mismatch at N={n}, Q={q}");
            }
        }
    }

    #[test]
    fn amac_refuses_inner_store_and_aliased_outer_store() {
        // Function with potentially aliasing raw pointer params `table`, `queries`, `out` (no NOALIAS fact).
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR),
                Param::of(Ty::PTR),
                Param::of(Ty::PTR),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[],
        );
        b.set_signature(sig);
        let [entry, h_q, b_q, h_bs, b_bs, end_bs, exit_all] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let arr_p = b.param(Ty::PTR);
        let qry_p = b.param(Ty::PTR);
        let out_p = b.param(Ty::PTR);
        let n_q = b.param(Ty::I64);
        let n_arr = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        b.jump((h_q, &[zero]));

        b.begin(h_q);
        let q = b.param(Ty::I64);
        let keep_q = b.icmp(IntCond::Ult, q, n_q);
        b.br(Flags::NONE, keep_q, (b_q, &[]), (exit_all, &[]));

        b.begin(b_q);
        let q_off = b.binary(Op::Shl, Flags::NONE, q, three);
        let qp = b.ptr_add(Flags::NONE, qry_p, q_off);
        let key = b.load(Ty::I64, Flags::NONE, qp, 0, MemAttrs::aligned(3));
        b.jump((h_bs, &[zero, n_arr]));

        b.begin(h_bs);
        let lo = b.param(Ty::I64);
        let hi = b.param(Ty::I64);
        let keep_bs = b.icmp(IntCond::Ult, lo, hi);
        b.br(Flags::NONE, keep_bs, (b_bs, &[]), (end_bs, &[lo]));

        b.begin(b_bs);
        let sum_lh = b.binary(Op::Add, Flags::NONE, lo, hi);
        let mid = b.binary(Op::Lshr, Flags::NONE, sum_lh, one);
        let m_off = b.binary(Op::Shl, Flags::NONE, mid, three);
        let mp = b.ptr_add(Flags::NONE, arr_p, m_off);
        let mv = b.load(Ty::I64, Flags::NONE, mp, 0, MemAttrs::aligned(3));
        let lt = b.icmp(IntCond::Ult, mv, key);
        let mid1 = b.binary(Op::Add, Flags::NONE, mid, one);
        let lo2 = b.select(Flags::NONE, lt, mid1, lo);
        let hi2 = b.select(Flags::NONE, lt, hi, mid);
        b.jump((h_bs, &[lo2, hi2]));

        b.begin(end_bs);
        let res_lo = b.param(Ty::I64);
        let out_off = b.binary(Op::Shl, Flags::NONE, q, three);
        let op_ptr = b.ptr_add(Flags::NONE, out_p, out_off);
        b.store(Flags::NONE, res_lo, op_ptr, 0, MemAttrs::aligned(3));
        let q2 = b.binary(Op::Add, Flags::NONE, q, one);
        b.jump((h_q, &[q2]));

        b.begin(exit_all);
        b.ret(&[]);
        let aliased = b.finish_checked().unwrap();

        let out = interleave_amac_loops(aliased.view(), AmacOpts::arm64(), TestHeap).unwrap();
        assert_eq!(out.stats.nests_interleaved, 0, "must NOT interleave when output store may alias inputs");
    }

    #[derive(Clone, Copy)]
    enum InnerBsearchForm {
        IfConverted,
        BranchDirectToHdr,
        BranchViaMerge,
    }

    /// Builds exact post-inlining, post-BCE `bsearch_batch`:
    /// - Pointer-IV outer loop over `queries` (`p != end`, `p_next = ptr_add(p, 8)` in `outer_body`,
    ///   `q = load i64 [p]`),
    /// - `mid = lo + ((hi - lo) >> 1)` inner binary search (if-converted or branching),
    /// - `acc = acc.wrapping_add(lo)` reduction in `outer_latch`.
    fn build_ptr_iv_bsearch_batch<A: Allocator + Copy>(
        table: &[u64],
        queries: &[u64],
        inner_form: InnerBsearchForm,
        alloc: A,
    ) -> Func<A> {
        let mut b = Builder::<A, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let table_bytes = (table.len().max(1) * 8) as u32;
        let query_bytes = (queries.len().max(1) * 8) as u32;
        let arr_slot = b.slot(table_bytes, 3, SlotKind::Temp);
        let qry_slot = b.slot(query_bytes, 3, SlotKind::Temp);

        let [entry, h_q, b_q, h_bs, b_bs, end_bs, exit_all] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));
        let then_bs = b.create_block(BlockKind::Normal);
        let else_bs = b.create_block(BlockKind::Normal);
        let merge_bs = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n_q = b.param(Ty::I64);
        let n_arr = b.param(Ty::I64);
        let arr_p = b.slot_addr(arr_slot);
        let qry_p = b.slot_addr(qry_slot);
        for (k, &v) in table.iter().enumerate() {
            let cv = b.iconst(Ty::I64, u128::from(v));
            b.store(Flags::NONE, cv, arr_p, (k * 8) as i32, MemAttrs::aligned(3));
        }
        for (k, &v) in queries.iter().enumerate() {
            let cv = b.iconst(Ty::I64, u128::from(v));
            b.store(Flags::NONE, cv, qry_p, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        let eight = b.iconst(Ty::I64, 8);
        let qry_bytes_v = b.binary(Op::Shl, Flags::NONE, n_q, three);
        let qry_end = b.ptr_add(Flags::NONE, qry_p, qry_bytes_v);
        b.jump((h_q, &[qry_p, zero]));

        // Pointer-IV outer header: `if p == qry_end { break; }`
        b.begin(h_q);
        let p = b.param(Ty::PTR);
        let acc = b.param(Ty::I64);
        let done = b.icmp(IntCond::Eq, p, qry_end);
        b.br(Flags::NONE, done, (exit_all, &[acc]), (b_q, &[]));

        // `slice::iter::next` body: advance `p_next = ptr_add(p, 8)` and load `q = *p`.
        b.begin(b_q);
        let p_next = b.ptr_add(Flags::NONE, p, eight);
        let key = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        b.jump((h_bs, &[zero, n_arr]));

        b.begin(h_bs);
        let lo = b.param(Ty::I64);
        let hi = b.param(Ty::I64);
        let keep_bs = b.icmp(IntCond::Ult, lo, hi);
        b.br(Flags::NONE, keep_bs, (b_bs, &[]), (end_bs, &[lo]));

        b.begin(b_bs);
        let diff = b.binary(Op::Sub, Flags::NONE, hi, lo);
        let half = b.binary(Op::Lshr, Flags::NONE, diff, one);
        let mid = b.binary(Op::Add, Flags::NONE, lo, half);
        let m_off = b.binary(Op::Shl, Flags::NONE, mid, three);
        let mp = b.ptr_add(Flags::NONE, arr_p, m_off);
        let mv = b.load(Ty::I64, Flags::NONE, mp, 0, MemAttrs::aligned(3));
        let lt = b.icmp(IntCond::Ult, mv, key);
        match inner_form {
            InnerBsearchForm::IfConverted => {
                let mid1 = b.binary(Op::Add, Flags::NONE, mid, one);
                let lo2 = b.select(Flags::NONE, lt, mid1, lo);
                let hi2 = b.select(Flags::NONE, lt, hi, mid);
                b.jump((h_bs, &[lo2, hi2]));
            }
            InnerBsearchForm::BranchDirectToHdr => {
                b.br(Flags::NONE, lt, (then_bs, &[]), (else_bs, &[]));

                b.begin(then_bs);
                let mid1 = b.binary(Op::Add, Flags::NONE, mid, one);
                b.jump((h_bs, &[mid1, hi]));

                b.begin(else_bs);
                b.jump((h_bs, &[lo, mid]));
            }
            InnerBsearchForm::BranchViaMerge => {
                b.br(Flags::NONE, lt, (then_bs, &[]), (else_bs, &[]));

                b.begin(then_bs);
                let mid1 = b.binary(Op::Add, Flags::NONE, mid, one);
                b.jump((merge_bs, &[mid1, hi]));

                b.begin(else_bs);
                b.jump((merge_bs, &[lo, mid]));

                b.begin(merge_bs);
                let m_lo = b.param(Ty::I64);
                let m_hi = b.param(Ty::I64);
                b.jump((h_bs, &[m_lo, m_hi]));
            }
        }

        b.begin(end_bs);
        let res_lo = b.param(Ty::I64);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, res_lo);
        b.jump((h_q, &[p_next, acc2]));

        b.begin(exit_all);
        let final_acc = b.param(Ty::I64);
        b.ret(&[final_acc]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn amac_real_rust_ptr_iv_bsearch_batch_if_converted_branching_and_rotated_matches_eval() {
        let alloc = blaze_own::testing::heap().alloc();
        for &n in &[0usize, 1, 7, 13, 64] {
            let q_max = 32usize;
            let (table, queries) = make_table_and_queries(n, q_max);
            for form in [
                InnerBsearchForm::IfConverted,
                InnerBsearchForm::BranchDirectToHdr,
                InnerBsearchForm::BranchViaMerge,
            ] {
                let orig = build_ptr_iv_bsearch_batch(&table, &queries, form, alloc);

                let out_pf = interleave_amac_loops(orig.view(), AmacOpts::arm64(), alloc).unwrap();
                verify(out_pf.func.view()).expect("pointer-IV AMAC output must verify");
                assert_eq!(out_pf.stats.nests_interleaved, 1);
                assert_eq!(out_pf.stats.group_width_used, 4);
                assert!(out_pf.stats.prefetches_inserted >= 4);
                assert_eq!(
                    count_prefetches(out_pf.func.view()),
                    out_pf.stats.prefetches_inserted as usize
                );

                // Idempotence: second pass must be a no-op.
                let again = interleave_amac_loops(out_pf.func.view(), AmacOpts::arm64(), alloc).unwrap();
                assert_eq!(again.stats.nests_interleaved, 0);

                let eval_fn = strip_prefetch_intrinsics(out_pf.func.view());
                for &q in &[0usize, 1, 3, 4, 5, 11, 32] {
                    let args = [Val::Int(q as u128), Val::Int(n as u128)];
                    let expected = eval_u64(orig.view(), &args);
                    let got = eval_u64(eval_fn.view(), &args);
                    assert_eq!(expected, got, "pointer-IV AMAC mismatch at N={n}, Q={q}");
                }
            }

            // Also verify post-`rotate_loops` pointer-IV `bsearch_batch` (`Rotated` outer + `SingleBlock` inner).
            let orig_if = build_ptr_iv_bsearch_batch(&table, &queries, InnerBsearchForm::IfConverted, alloc);
            let rot = rotate_loops(orig_if.view(), alloc).unwrap();
            let out_rot = interleave_amac_loops(rot.func.view(), AmacOpts::arm64(), alloc).unwrap();
            verify(out_rot.func.view()).expect("rotated pointer-IV AMAC output must verify");
            assert_eq!(out_rot.stats.nests_interleaved, 1);
            assert_eq!(out_rot.stats.group_width_used, 4);
            assert!(out_rot.stats.prefetches_inserted >= 4);
            let eval_rot = strip_prefetch_intrinsics(out_rot.func.view());
            for &q in &[0usize, 1, 3, 4, 5, 11, 32] {
                let args = [Val::Int(q as u128), Val::Int(n as u128)];
                let expected = eval_u64(orig_if.view(), &args);
                let got = eval_u64(eval_rot.view(), &args);
                assert_eq!(expected, got, "rotated pointer-IV AMAC mismatch at N={n}, Q={q}");
            }
        }
    }

    /// Builds the exact post-inlining, post-inner-rotation `bsearch_batch` LIR CFG:
    /// `outer_hdr(acc, p)` (`Icmp(Ne, p, end)`) -> `outer_body` (`p_next = PtrAdd[INBOUNDS](p, 8)`,
    /// `q = Load(I64, p, 0)`, outer-invariant guard `0 < table_len` / `table_len == 0`) ->
    /// 1-block rotated `inner_hdr(lo, hi)` (`mid = lo + ((hi - lo) >> 1)`, `Load`, `Select`) vs
    /// skip to `outer_latch(0)` -> `outer_latch(res)` (`acc_next = Add(acc, res)`).
    fn build_guarded_rotated_bsearch_batch<A: Allocator + Copy>(
        table: &[u64],
        queries: &[u64],
        enter_is_then: bool,
        alloc: A,
    ) -> Func<A> {
        let mut b = Builder::<A, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let table_bytes = (table.len().max(1) * 8) as u32;
        let query_bytes = (queries.len().max(1) * 8) as u32;
        let arr_slot = b.slot(table_bytes, 3, SlotKind::Temp);
        let qry_slot = b.slot(query_bytes, 3, SlotKind::Temp);

        let [entry, outer_hdr, outer_body, inner_hdr, outer_latch, exit] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let table_len = b.param(Ty::I64);
        let q_len = b.param(Ty::I64);
        let table_ptr = b.slot_addr(arr_slot);
        let q_ptr = b.slot_addr(qry_slot);
        if table.is_empty() {
            // Seed slot[0] = 0 so a buggy AMAC that enters `inner_hdr` when `table_len == 0`
            // sees `0 < q` and produces a wrong non-zero `lo_next` instead of `0`.
            let z = b.iconst(Ty::I64, 0);
            b.store(Flags::NONE, z, table_ptr, 0, MemAttrs::aligned(3));
        }
        for (k, &v) in table.iter().enumerate() {
            let cv = b.iconst(Ty::I64, u128::from(v));
            b.store(Flags::NONE, cv, table_ptr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        for (k, &v) in queries.iter().enumerate() {
            let cv = b.iconst(Ty::I64, u128::from(v));
            b.store(Flags::NONE, cv, q_ptr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        let eight = b.iconst(Ty::I64, 8);
        let q_bytes = b.binary(Op::Shl, Flags::NONE, q_len, three);
        let end = b.ptr_add(Flags::INBOUNDS, q_ptr, q_bytes);
        b.jump((outer_hdr, &[zero, q_ptr]));

        b.begin(outer_hdr);
        let acc = b.param(Ty::I64);
        let p = b.param(Ty::PTR);
        let keep_outer = b.icmp(IntCond::Ne, p, end);
        b.br(Flags::NONE, keep_outer, (outer_body, &[]), (exit, &[acc]));

        b.begin(outer_body);
        let p_next = b.ptr_add(Flags::INBOUNDS, p, eight);
        let q = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        if enter_is_then {
            let nonempty = b.icmp(IntCond::Ult, zero, table_len);
            b.br(Flags::NONE, nonempty, (inner_hdr, &[zero, table_len]), (outer_latch, &[zero]));
        } else {
            let empty = b.icmp(IntCond::Eq, table_len, zero);
            b.br(Flags::NONE, empty, (outer_latch, &[zero]), (inner_hdr, &[zero, table_len]));
        }

        b.begin(inner_hdr);
        let lo = b.param(Ty::I64);
        let hi = b.param(Ty::I64);
        let diff = b.binary(Op::Sub, Flags::NONE, hi, lo);
        let half = b.binary(Op::Lshr, Flags::NONE, diff, one);
        let mid = b.binary(Op::Add, Flags::NONE, lo, half);
        let mid_off = b.binary(Op::Shl, Flags::NONE, mid, three);
        let mid_ptr = b.ptr_add(Flags::NONE, table_ptr, mid_off);
        let val = b.load(Ty::I64, Flags::NONE, mid_ptr, 0, MemAttrs::aligned(3));
        let lt = b.icmp(IntCond::Ult, val, q);
        let mid_p1 = b.binary(Op::Add, Flags::NONE, mid, one);
        let lo_next = b.select(Flags::NONE, lt, mid_p1, lo);
        let hi_next = b.select(Flags::NONE, lt, hi, mid);
        let keep_inner = b.icmp(IntCond::Ult, lo_next, hi_next);
        b.br(Flags::NONE, keep_inner, (inner_hdr, &[lo_next, hi_next]), (outer_latch, &[lo_next]));

        b.begin(outer_latch);
        let res = b.param(Ty::I64);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, res);
        b.jump((outer_hdr, &[acc_next, p_next]));

        b.begin(exit);
        let final_acc = b.param(Ty::I64);
        b.ret(&[final_acc]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn select_group_width_respects_apple_silicon_register_budget() {
        let m = AmacOpts::apple_silicon();
        assert_eq!(m.group_width, 8);
        assert_eq!(m.max_state_regs, 18);
        // 2-reg pointer chase -> K = 8 (16 <= 18)
        assert_eq!(select_group_width(m.group_width, 0, 2, m.max_state_regs), 8);
        // 3-reg bsearch_batch (`lo, hi, q`) -> K = 6 (18 <= 18, zero GPR spills)
        assert_eq!(select_group_width(m.group_width, 0, 3, m.max_state_regs), 6);
        // 4-reg state -> K = 4 (16 <= 18)
        assert_eq!(select_group_width(m.group_width, 0, 4, m.max_state_regs), 4);
        // 5-reg state -> K = 2 (10 <= 18)
        assert_eq!(select_group_width(m.group_width, 0, 5, m.max_state_regs), 2);
        // 10-reg state -> exceeds budget even at K = 2
        assert_eq!(select_group_width(m.group_width, 0, 10, m.max_state_regs), 0);
        // Uniform param + 2-reg lane state -> K = 8 (1 + 16 = 17 <= 18)
        assert_eq!(select_group_width(m.group_width, 1, 2, m.max_state_regs), 8);

        let w = AmacOpts::wide_8();
        assert_eq!(select_group_width(w.group_width, 0, 3, w.max_state_regs), 8);
        assert_eq!(select_group_width(w.group_width, 0, 4, w.max_state_regs), 6);

        let a = AmacOpts::arm64();
        assert_eq!(select_group_width(a.group_width, 0, 3, a.max_state_regs), 4);
    }

    #[test]
    fn amac_8way_guarded_rotated_bsearch_batch_matches_eval() {
        let alloc = blaze_own::testing::heap().alloc();
        // Verify Apple Silicon preset targets up to 8-way with a 18-reg spill-free budget
        // (`K = 6` on 3-reg `bsearch_batch`) and passes `table_len = 1_048_576`.
        let m_opts = AmacOpts::apple_silicon();
        assert_eq!(m_opts.group_width, 8);
        assert_eq!(m_opts.max_state_regs, 18);
        assert!(m_opts.min_var_trip_bound <= 1_048_576);

        let configs = [
            (AmacOpts::wide_8(), 8u32),
            (AmacOpts::apple_silicon(), 6u32),
            (AmacOpts { min_var_trip_bound: 0, ..AmacOpts::apple_silicon() }, 6u32),
        ];

        for enter_is_then in [true, false] {
            for &table_len in &[0usize, 1, 16, 64] {
                let q_max = 25usize;
                let (table, queries) = make_table_and_queries(table_len, q_max);
                let orig = build_guarded_rotated_bsearch_batch(&table, &queries, enter_is_then, alloc);

                for (opts, expected_k) in configs {
                    let out = interleave_amac_loops(orig.view(), opts, alloc).unwrap();
                    let v = out.func.view();
                    verify(v).expect("guarded rotated AMAC output must verify");
                    assert_eq!(out.stats.nests_interleaved, 1);
                    assert_eq!(out.stats.group_width_used, expected_k);
                    assert_eq!(out.stats.prefetches_inserted, expected_k);
                    assert_eq!(count_prefetches(v), expected_k as usize);

                    // `bound = PtrAdd[INBOUNDS](q_ptr, Shl(q_len, 3))` MUST take the fast 2-check
                    // path in `b_amac_entry` (block 1) with `<= 1` `Op::Select` (zero pointer-step chain!).
                    let entry_selects = v
                        .block(Block(1))
                        .filter(|&inst| v.op(inst) == Op::Select)
                        .count();
                    assert!(
                        entry_selects <= 1,
                        "expected <= 1 Op::Select in b_amac_entry for INBOUNDS slice::Iter bound, got {entry_selects}"
                    );

                    // Guard was hoisted into `b_amac_entry`, so `b_amac_outer_body` (block 2) MUST
                    // jump unconditionally into `b_amac_inner_hdr` (block 3) with ZERO skip branch!
                    assert_eq!(
                        v.op(v.terminator(Block(2))),
                        Op::Jump,
                        "b_amac_outer_body must enter b_amac_inner_hdr unconditionally"
                    );

                    // All `expected_k` software prefetches in `b_amac_inner_hdr` (block 3) MUST precede all loads.
                    let mut pf_count = 0usize;
                    let mut ld_count = 0usize;
                    let mut last_pf = 0usize;
                    let mut first_ld = usize::MAX;
                    for inst in v.block(Block(3)) {
                        if v.op(inst) == Op::TargetIntrinsic
                            && v.intrinsic_name(IntrinsicId(v.extra[v.data[inst] as usize]))
                                == PREFETCH_READ_L1_INTRINSIC.as_bytes()
                        {
                            pf_count += 1;
                            last_pf = inst;
                        } else if v.op(inst) == Op::Load {
                            ld_count += 1;
                            first_ld = first_ld.min(inst);
                        }
                    }
                    assert_eq!(pf_count, expected_k as usize, "expected {expected_k} prefetches in b_amac_inner_hdr");
                    assert_eq!(ld_count, expected_k as usize, "expected {expected_k} loads in b_amac_inner_hdr");
                    assert!(last_pf < first_ld, "all {expected_k} prefetches must be emitted before all loads");

                    // Idempotence: second pass must be a no-op.
                    let again = interleave_amac_loops(v, opts, alloc).unwrap();
                    assert_eq!(again.stats.nests_interleaved, 0);

                    let eval_fn = strip_prefetch_intrinsics(v);
                    for &q_len in &[0usize, 1, 5, 6, 7, 8, 11, 12, 13, 15, 19, 25] {
                        let args = [Val::Int(table_len as u128), Val::Int(q_len as u128)];
                        let expected = eval_u64(orig.view(), &args);
                        let got = eval_u64(eval_fn.view(), &args);
                        assert_eq!(
                            expected, got,
                            "guarded rotated bsearch_batch mismatch at K={expected_k}, enter_is_then={enter_is_then}, table_len={table_len}, q_len={q_len}, opts={opts:?}"
                        );
                    }
                }
            }
        }
    }
}
