//! Whole-Program Escape Analysis and Heap-to-Stack Promotion (`blaze_lir::escape`).
//!
//! Identifies fixed-size heap allocations (`Op::Call` classified as [`AllocRole::Alloc`]) whose
//! returned pointer and all derived `Op::PtrAdd` pointers never escape the function (never
//! returned, never stored as a value into memory, never passed to an escaping call or indirect
//! call, never cast to an integer, and never carried across loop iterations).
//!
//! Each non-escaping allocation of constant size `1..=4096` bytes and power-of-two alignment
//! `1..=64` bytes (default 8) is promoted to a fresh local [`SlotRec`] (`Op::SlotAddr`), and every
//! matching [`AllocRole::Dealloc`] call on that promoted base pointer is elided. Downstream
//! [`super::sroa::scalar_replace`], [`super::propagate`], and [`super::dead`] then fold the
//! promoted slot completely into SSA registers.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, constant_of, each, filled, owners},
};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    func::{SlotKind, SlotRec},
    op::{Field, Layout},
    verify,
};

/// Classification of a direct-call target (`RefId`) for escape analysis and heap-to-stack
/// promotion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AllocRole {
    /// Heap allocation returning a fresh non-aliasing pointer (`Ty::PTR`).
    Alloc {
        /// Argument index of the byte size.
        size_arg: usize,
        /// Optional argument index of the byte alignment (defaults to 8 when `None`).
        align_arg: Option<usize>,
    },
    /// Heap deallocation freeing the pointer at `ptr_arg`.
    Dealloc { ptr_arg: usize },
    /// Callee proven not to capture, escape, or return aliases of its pointer arguments.
    NonEscapingCall,
    /// Unknown or escaping callee: conservatively captures every pointer argument.
    EscapingCall,
}

/// Per-function intraprocedural escape summary for interprocedural call-graph propagation.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct EscapeSummary {
    /// Bit `k` (`0 <= k < 64`) is set iff entry parameter `k` escapes to memory, a return value,
    /// an escaping call, or an integer/address cast.
    pub param_escapes: u64,
    /// Whether every return of this function provably returns a fresh non-aliasing allocation.
    pub returns_fresh_alloc: bool,
}

/// Telemetry returned by [`promote_heap_to_stack`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct EscapeStats {
    pub allocations_analyzed: u32,
    pub promoted_to_stack: u32,
    pub deallocs_elided: u32,
}

/// Output of [`promote_heap_to_stack`].
#[derive(Debug)]
pub struct EscapeOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: EscapeStats,
}

const MAX_STACK_PROMOTE_SIZE: u128 = 4096;
const MAX_STACK_PROMOTE_ALIGN: u128 = 64;
const DEFAULT_ALIGN_LOG2: u8 = 3; // 8 bytes

#[inline]
fn to_idx(x: u32) -> usize {
    usize::try_from(x).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(x: usize) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

#[inline]
fn u128_to_u32(x: u128) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

#[inline]
fn u32_to_u8(x: u32) -> u8 {
    u8::try_from(x).unwrap_or(0)
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL
        .iter()
        .position(|&o| o == op)
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(0)
}

#[inline]
const fn slot_kind_byte(kind: SlotKind) -> u8 {
    match kind {
        SlotKind::Temp => 0,
        SlotKind::Named => 1,
        SlotKind::ArgSpill => 2,
    }
}

#[inline]
fn at_u32(slice: &[u32], i: usize) -> u32 {
    slice.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u64(slice: &[u64], i: usize) -> u64 {
    slice.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u16(slice: &[u16], i: usize) -> u16 {
    slice.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u8(slice: &[u8], i: usize) -> u8 {
    slice.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_bool(slice: &[bool], i: usize) -> bool {
    slice.get(i).copied().unwrap_or(false)
}

#[inline]
fn at_origin(slice: &[Origin], i: usize) -> Origin {
    slice.get(i).copied().unwrap_or(Origin::None)
}

#[inline]
fn set_at<T>(slice: &mut [T], i: usize, val: T) {
    if let Some(slot) = slice.get_mut(i) {
        *slot = val;
    }
}

/// Summarizes which entry parameters of `view` escape (stored as a value into memory, returned,
/// passed to an unknown call, cast to integer/exposed, etc.) versus remaining strictly local /
/// read-only.
///
/// Without a callee classifier, no single-function `Op::Call` is assumed to return a fresh
/// allocation, so `returns_fresh_alloc` is conservatively `false`.
pub fn summarize_escape<A: Allocator + Copy>(view: View<'_>, alloc: A) -> EscapeSummary {
    let blocks = view.blocks();
    let len = view.len();
    if blocks == 0 || len == 0 {
        return EscapeSummary::default();
    }

    // Per-SSA-value bitmask of entry parameters (0..64) from which this value may be derived.
    let mut taint = filled(len, 0u64, &alloc);
    let entry_params = view.params(Block(0));
    for (k, inst) in entry_params.enumerate() {
        if k < 64 {
            set_at(&mut taint, inst, 1u64 << k);
        }
    }

    // Phase 1: monotone fixed-point propagation of parameter provenance across derived pointers
    // and block-parameter edges. Bounded by `len + 1` iterations (deterministic).
    for _ in 0..=len {
        let mut changed = false;
        for inst in 0..len {
            let op = view.op(inst);
            match op {
                Op::PtrAdd | Op::PtrWithAddr => {
                    let base = to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst))));
                    let cur = at_u64(&taint, inst);
                    let next = cur | at_u64(&taint, base);
                    if next != cur {
                        set_at(&mut taint, inst, next);
                        changed = true;
                    }
                }
                Op::Freeze => {
                    let v = to_idx(at_u32(view.data, inst));
                    let cur = at_u64(&taint, inst);
                    let next = cur | at_u64(&taint, v);
                    if next != cur {
                        set_at(&mut taint, inst, next);
                        changed = true;
                    }
                }
                Op::Select => {
                    let at = to_idx(at_u32(view.data, inst));
                    let a = to_idx(at_u32(view.extra, at + 1));
                    let b = to_idx(at_u32(view.extra, at + 2));
                    let cur = at_u64(&taint, inst);
                    let next = cur | at_u64(&taint, a) | at_u64(&taint, b);
                    if next != cur {
                        set_at(&mut taint, inst, next);
                        changed = true;
                    }
                }
                Op::Jump | Op::Br | Op::Switch => {
                    each(&view, inst, |item| {
                        if let Item::Edge(edge) = item {
                            if let Some((start, count)) = edge.args {
                                let params = view.params(Block(edge.block));
                                for k in 0..count {
                                    let arg = to_idx(at_u32(view.extra, start + k));
                                    let param = params.start + k;
                                    let cur = at_u64(&taint, param);
                                    let next = cur | at_u64(&taint, arg);
                                    if next != cur {
                                        set_at(&mut taint, param, next);
                                        changed = true;
                                    }
                                }
                            }
                        }
                    });
                }
                _ => {}
            }
        }
        if !changed {
            break;
        }
    }

    // Phase 2: inspect every use of every tainted value and record escaping parameters.
    let mut param_escapes = 0u64;
    for inst in 0..len {
        let op = view.op(inst);
        match op {
            // Pure reads/writes *through* a pointer address do not leak the pointer itself.
            Op::Load | Op::AtomicLoad | Op::MaskedLoad => {}
            Op::Store | Op::AtomicStore | Op::MaskedStore => {
                // `extra[at]` is the stored value (escapes!), `extra[at + 1]` is the address.
                let val = to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst))));
                param_escapes |= at_u64(&taint, val);
            }
            Op::AtomicRmw => {
                // `[rmw, addr, operand]`: operand is stored to memory and escapes.
                let at = to_idx(at_u32(view.data, inst));
                param_escapes |= at_u64(&taint, to_idx(at_u32(view.extra, at + 2)));
            }
            Op::Cmpxchg => {
                // `[addr, expected, new]`: `new` is written to memory and escapes.
                let at = to_idx(at_u32(view.data, inst));
                param_escapes |= at_u64(&taint, to_idx(at_u32(view.extra, at + 2)));
            }
            Op::Memcpy | Op::Memmove | Op::Memset | Op::Memcmp => {}
            // Pointer propagation ops handled in Phase 1.
            Op::PtrAdd | Op::Freeze => {}
            Op::PtrWithAddr => {
                // `[ptr, addr]`: pointer provenance was propagated; `addr` is an integer.
            }
            Op::Select => {
                // `[cond, a, b]`: `a` and `b` propagated in Phase 1; `cond` is `i1`.
            }
            // Pointer comparisons and local control/scalar checks do not capture a pointer.
            Op::Icmp | Op::Fcmp | Op::Assume | Op::Jump | Op::Br | Op::Switch => {}
            // Pure scalar arithmetic/bitwise/conversion ops (excluding pointer-to-int casts).
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::Udiv
            | Op::Sdiv
            | Op::Urem
            | Op::Srem
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotl
            | Op::Rotr
            | Op::Umulhi
            | Op::Smulhi
            | Op::UaddSat
            | Op::SaddSat
            | Op::UsubSat
            | Op::SsubSat
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
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
            | Op::Fdiv
            | Op::Frem
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Fcopysign
            | Op::Fneg
            | Op::Fabs
            | Op::Sqrt
            | Op::Floor
            | Op::Ceil
            | Op::Ftrunc
            | Op::Nearest
            | Op::Fround
            | Op::Fma
            | Op::Fmuladd => {}
            // Everything else (Return, Call, CallIndirect, Invoke, TailCall, PtrAddr, Expose,
            // FromExposed, Resume, Suspend, TargetIntrinsic, Asm, AsmBr, Scatter, Gather, vector
            // packing of pointers, etc.) conservatively escapes every value operand it reads.
            _ => {
                each(&view, inst, |item| {
                    if let Item::Value(v) = item {
                        param_escapes |= at_u64(&taint, to_idx(v));
                    }
                });
            }
        }
    }

    EscapeSummary { param_escapes, returns_fresh_alloc: false }
}

/// Candidate fixed-size heap allocation discovered during scan.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    inst: usize,
    block: u32,
    size: u32,
    align_log2: u8,
}

/// Points-to provenance lattice for candidate heap allocations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    /// Not derived from any candidate allocation.
    None,
    /// Derived exclusively from candidate `id`. `is_base` is true iff zero byte offset from the
    /// allocation's base pointer (valid argument to `Dealloc`).
    Cand { id: u32, is_base: bool },
    /// Merged from two or more distinct candidates (all involved candidates are marked escaped).
    Mixed,
}

#[inline]
fn join_origin(dst: Origin, src: Origin, escaped: &mut [bool]) -> Origin {
    match (dst, src) {
        (Origin::None, other) => other,
        (other, Origin::None) => other,
        (Origin::Cand { id: a, is_base: ba }, Origin::Cand { id: b, is_base: bb }) if a == b => {
            Origin::Cand { id: a, is_base: ba && bb }
        }
        (Origin::Cand { id: a, .. }, Origin::Cand { id: b, .. }) => {
            set_at(escaped, to_idx(a), true);
            set_at(escaped, to_idx(b), true);
            Origin::Mixed
        }
        (Origin::Cand { id, .. }, Origin::Mixed) | (Origin::Mixed, Origin::Cand { id, .. }) => {
            set_at(escaped, to_idx(id), true);
            Origin::Mixed
        }
        (Origin::Mixed, Origin::Mixed) => Origin::Mixed,
    }
}

/// Returns `true` iff there is a directed CFG path of length >= 0 from `from` to `target`.
fn can_reach<A: Allocator + Copy>(
    from: usize,
    target: usize,
    succs: &[Vec<u32, A>],
    alloc: &A,
) -> bool {
    if from == target {
        return true;
    }
    let blocks = succs.len();
    let mut seen = filled(blocks, false, alloc);
    let mut work: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
    set_at(&mut seen, from, true);
    work.push(to_u32(from));
    while let Some(b) = work.pop() {
        if let Some(row) = succs.get(to_idx(b)) {
            for &s in row.iter() {
                let si = to_idx(s);
                if si == target {
                    return true;
                }
                if !at_bool(&seen, si) {
                    set_at(&mut seen, si, true);
                    work.push(s);
                }
            }
        }
    }
    false
}

/// Copies instruction `inst` of `view` into `func`, remapping all value operands via `val_map`.
fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    func: &mut Func<A>,
) -> Result<(), SimplifyError> {
    let op_b = at_u8(view.ops, inst);
    let op = view.op(inst);
    let flags = at_u8(view.flags, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let map_val = |v: u32| -> Result<u32, SimplifyError> {
        match val_map.get(to_idx(v)).copied() {
            Some(NONE) | None => Err(SimplifyError::Refused { inst, what: "a use of an elided instruction" }),
            Some(mapped) => Ok(mapped),
        }
    };
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => map_val(at_u32(view.data, inst))?,
        Layout::Data(_) => at_u32(view.data, inst),
        Layout::Extra(fields) => {
            let src = to_idx(at_u32(view.data, inst));
            let dst = to_u32(func.extra.len());
            let mut p = src;
            let mut scrutinee = None;
            for &f in fields {
                match f {
                    Field::Value => {
                        let v = at_u32(view.extra, p);
                        scrutinee = Some(v);
                        func.extra.push(map_val(v)?);
                        p += 1;
                    }
                    Field::Values => {
                        let n = to_idx(at_u32(view.extra, p));
                        func.extra.push(to_u32(n));
                        p += 1;
                        for k in 0..n {
                            func.extra.push(map_val(at_u32(view.extra, p + k))?);
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(at_u32(view.extra, p));
                        let n = to_idx(at_u32(view.extra, p + 1));
                        func.extra.push(to_u32(n));
                        p += 2;
                        for k in 0..n {
                            func.extra.push(map_val(at_u32(view.extra, p + k))?);
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n = to_idx(at_u32(view.extra, p));
                        func.extra.push(to_u32(n));
                        p += 1;
                        let w = to_idx(scrutinee.map_or(0, |s| Ty(at_u16(view.tys, to_idx(s))).const_words()));
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at_u32(view.extra, p + k));
                            }
                            p += w;
                            func.extra.push(at_u32(view.extra, p));
                            let an = to_idx(at_u32(view.extra, p + 1));
                            func.extra.push(to_u32(an));
                            p += 2;
                            for k in 0..an {
                                func.extra.push(map_val(at_u32(view.extra, p + k))?);
                            }
                            p += an;
                        }
                    }
                    Field::Mask | Field::Blocks => {
                        let n = to_idx(at_u32(view.extra, p));
                        func.extra.push(to_u32(n));
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
                    Field::Block
                    | Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm => {
                        func.extra.push(at_u32(view.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    func.ops.push(op_b);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    Ok(())
}

/// Promotes non-escaping fixed-size heap allocations (`AllocRole::Alloc`) in `view` to local
/// `Op::SlotAddr` stack slots and elides matching `AllocRole::Dealloc` calls.
///
/// The resulting function is verified with [`verify`] before being returned.
pub fn promote_heap_to_stack<A: Allocator + Copy>(
    view: View<'_>,
    classify_ref: impl Fn(u32) -> AllocRole,
    alloc: A,
) -> Result<EscapeOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks == 0 || len == 0 {
        return Ok(EscapeOutput {
            func: view.to_func_in(alloc),
            stats: EscapeStats::default(),
        });
    }

    let inst_block = owners(&view, &alloc);
    let mut stats = EscapeStats::default();
    let mut cands: Vec<Candidate, A> = Vec::new_in(alloc);

    // Step 1: Discover candidate fixed-size `Op::Call` allocations.
    for inst in 0..len {
        if view.op(inst) != Op::Call {
            continue;
        }
        let at = to_idx(at_u32(view.data, inst));
        let ref_id = at_u32(view.extra, at);
        let AllocRole::Alloc { size_arg, align_arg } = classify_ref(ref_id) else {
            continue;
        };
        stats.allocations_analyzed += 1;
        if Ty(at_u16(view.tys, inst)) != Ty::PTR {
            continue;
        }
        let argc = to_idx(at_u32(view.extra, at + 2));
        if size_arg >= argc {
            continue;
        }
        let size_val = at_u32(view.extra, at + 3 + size_arg);
        let Some(sz) = constant_of(&view, size_val) else {
            continue;
        };
        if !(1..=MAX_STACK_PROMOTE_SIZE).contains(&sz) {
            continue;
        }
        let align_log2 = match align_arg {
            None => DEFAULT_ALIGN_LOG2,
            Some(ai) => {
                if ai >= argc {
                    continue;
                }
                let align_val = at_u32(view.extra, at + 3 + ai);
                let Some(al) = constant_of(&view, align_val) else {
                    continue;
                };
                if !(1..=MAX_STACK_PROMOTE_ALIGN).contains(&al) || !al.is_power_of_two() {
                    continue;
                }
                u32_to_u8(al.trailing_zeros())
            }
        };
        cands.push(Candidate {
            inst,
            block: at_u32(&inst_block, inst),
            size: u128_to_u32(sz),
            align_log2,
        });
    }

    if cands.is_empty() {
        return Ok(EscapeOutput { func: view.to_func_in(alloc), stats });
    }

    // Build CFG successors once for loop-carried allocation checks.
    let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        succs.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let term = view.terminator(Block(to_u32(b)));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                if let Some(row) = succs.get_mut(b) {
                    if row.scan_for(&edge.block).is_none() {
                        row.push(edge.block);
                    }
                }
            }
        });
    }

    // Step 2 (Phase A): Monotone fixed-point propagation of candidate pointer origins.
    let mut origin = filled(len, Origin::None, &alloc);
    let mut escaped = filled(cands.len(), false, &alloc);
    for (id, cand) in cands.iter().enumerate() {
        set_at(&mut origin, cand.inst, Origin::Cand { id: to_u32(id), is_base: true });
    }

    let max_iters = len.saturating_mul(3).saturating_add(1);
    for _ in 0..max_iters {
        let mut changed = false;
        for inst in 0..len {
            let op = view.op(inst);
            match op {
                Op::PtrAdd => {
                    let at = to_idx(at_u32(view.data, inst));
                    let base = to_idx(at_u32(view.extra, at));
                    let off = at_u32(view.extra, at + 1);
                    let derived = match at_origin(&origin, base) {
                        Origin::None => Origin::None,
                        Origin::Mixed => Origin::Mixed,
                        Origin::Cand { id, is_base } => {
                            let zero_off = constant_of(&view, off) == Some(0);
                            Origin::Cand { id, is_base: is_base && zero_off }
                        }
                    };
                    let cur = at_origin(&origin, inst);
                    let next = join_origin(cur, derived, &mut escaped);
                    if next != cur {
                        set_at(&mut origin, inst, next);
                        changed = true;
                    }
                }
                Op::Freeze => {
                    let v = to_idx(at_u32(view.data, inst));
                    let cur = at_origin(&origin, inst);
                    let next = join_origin(cur, at_origin(&origin, v), &mut escaped);
                    if next != cur {
                        set_at(&mut origin, inst, next);
                        changed = true;
                    }
                }
                Op::Select => {
                    let at = to_idx(at_u32(view.data, inst));
                    let a = to_idx(at_u32(view.extra, at + 1));
                    let b = to_idx(at_u32(view.extra, at + 2));
                    let cur = at_origin(&origin, inst);
                    let m1 = join_origin(cur, at_origin(&origin, a), &mut escaped);
                    let next = join_origin(m1, at_origin(&origin, b), &mut escaped);
                    if next != cur {
                        set_at(&mut origin, inst, next);
                        changed = true;
                    }
                }
                Op::Jump | Op::Br | Op::Switch => {
                    each(&view, inst, |item| {
                        if let Item::Edge(edge) = item {
                            if let Some((start, count)) = edge.args {
                                let params = view.params(Block(edge.block));
                                for k in 0..count {
                                    let arg = to_idx(at_u32(view.extra, start + k));
                                    let param = params.start + k;
                                    let cur = at_origin(&origin, param);
                                    let next = join_origin(cur, at_origin(&origin, arg), &mut escaped);
                                    if next != cur {
                                        set_at(&mut origin, param, next);
                                        changed = true;
                                    }
                                }
                            }
                        }
                    });
                }
                _ => {}
            }
        }
        if !changed {
            break;
        }
    }

    // Step 3 (Phase B): Sound escape verification and dealloc matching on stabilized origins.
    let mut dealloc_cand = filled(len, NONE, &alloc);
    let mark_escaped = |v: u32, origin: &[Origin], escaped: &mut [bool]| {
        if let Origin::Cand { id, .. } = at_origin(origin, to_idx(v)) {
            set_at(escaped, to_idx(id), true);
        }
    };

    for inst in 0..len {
        let op = view.op(inst);
        match op {
            Op::Load => {
                // Address operand is safe.
            }
            Op::Store => {
                // `extra[at]` is the stored value (escapes!), `extra[at + 1]` is the address.
                let val = at_u32(view.extra, to_idx(at_u32(view.data, inst)));
                mark_escaped(val, &origin, &mut escaped);
            }
            Op::PtrAdd => {
                let at = to_idx(at_u32(view.data, inst));
                let off = at_u32(view.extra, at + 1);
                mark_escaped(off, &origin, &mut escaped);
            }
            Op::Freeze => {}
            Op::Select => {
                let at = to_idx(at_u32(view.data, inst));
                let cond = at_u32(view.extra, at);
                let a = at_u32(view.extra, at + 1);
                let b = at_u32(view.extra, at + 2);
                mark_escaped(cond, &origin, &mut escaped);
                // If the select result is `Cand { id }`, BOTH arms must come from `id`; mixing
                // with a non-candidate (`Origin::None`) escapes `id`.
                if let Origin::Cand { id, .. } = at_origin(&origin, inst) {
                    let a_ok = matches!(at_origin(&origin, to_idx(a)), Origin::Cand { id: aid, .. } if aid == id);
                    let b_ok = matches!(at_origin(&origin, to_idx(b)), Origin::Cand { id: bid, .. } if bid == id);
                    if !a_ok || !b_ok {
                        set_at(&mut escaped, to_idx(id), true);
                    }
                }
            }
            Op::Icmp => {
                let at = to_idx(at_u32(view.data, inst));
                let cond = IntCond::from_word(at_u32(view.extra, at));
                let a = at_u32(view.extra, at + 1);
                let b = at_u32(view.extra, at + 2);
                let a_cand = matches!(at_origin(&origin, to_idx(a)), Origin::Cand { .. });
                let b_cand = matches!(at_origin(&origin, to_idx(b)), Origin::Cand { .. });
                if a_cand || b_cand {
                    let eq_or_ne = matches!(cond, Some(IntCond::Eq | IntCond::Ne));
                    let safe_null_check = eq_or_ne
                        && ((a_cand && view.op(to_idx(b)) == Op::Null)
                            || (b_cand && view.op(to_idx(a)) == Op::Null));
                    if !safe_null_check {
                        mark_escaped(a, &origin, &mut escaped);
                        mark_escaped(b, &origin, &mut escaped);
                    }
                }
            }
            Op::Memcpy | Op::Memmove => {
                let at = to_idx(at_u32(view.data, inst));
                mark_escaped(at_u32(view.extra, at + 2), &origin, &mut escaped);
            }
            Op::Memset => {
                let at = to_idx(at_u32(view.data, inst));
                mark_escaped(at_u32(view.extra, at + 1), &origin, &mut escaped);
                mark_escaped(at_u32(view.extra, at + 2), &origin, &mut escaped);
            }
            Op::Memcmp => {
                let at = to_idx(at_u32(view.data, inst));
                mark_escaped(at_u32(view.extra, at + 2), &origin, &mut escaped);
            }
            Op::Call => {
                let at = to_idx(at_u32(view.data, inst));
                let ref_id = at_u32(view.extra, at);
                let argc = to_idx(at_u32(view.extra, at + 2));
                match classify_ref(ref_id) {
                    AllocRole::Dealloc { ptr_arg } => {
                        for k in 0..argc {
                            let arg = at_u32(view.extra, at + 3 + k);
                            if k == ptr_arg {
                                if let Origin::Cand { id, is_base } = at_origin(&origin, to_idx(arg)) {
                                    if !is_base || !Ty(at_u16(view.tys, inst)).is_none() {
                                        set_at(&mut escaped, to_idx(id), true);
                                    } else {
                                        set_at(&mut dealloc_cand, inst, id);
                                    }
                                }
                            } else {
                                mark_escaped(arg, &origin, &mut escaped);
                            }
                        }
                    }
                    AllocRole::NonEscapingCall => {}
                    AllocRole::Alloc { .. } | AllocRole::EscapingCall => {
                        for k in 0..argc {
                            mark_escaped(at_u32(view.extra, at + 3 + k), &origin, &mut escaped);
                        }
                    }
                }
            }
            Op::Jump | Op::Br | Op::Switch => {
                each(&view, inst, |item| match item {
                    Item::Value(v) => mark_escaped(v, &origin, &mut escaped),
                    Item::Edge(edge) => {
                        if let Some((start, count)) = edge.args {
                            let tb = to_idx(edge.block);
                            let params = view.params(Block(edge.block));
                            for k in 0..count {
                                let arg = at_u32(view.extra, start + k);
                                let param = params.start + k;
                                if let Origin::Cand { id, .. } = at_origin(&origin, param) {
                                    let arg_same = matches!(
                                        at_origin(&origin, to_idx(arg)),
                                        Origin::Cand { id: aid, .. } if aid == id
                                    );
                                    let cand_blk = cands.get(to_idx(id)).map_or(0, |c| to_idx(c.block));
                                    if !arg_same || can_reach(tb, cand_blk, &succs, &alloc) {
                                        set_at(&mut escaped, to_idx(id), true);
                                    }
                                }
                            }
                        }
                    }
                });
            }
            _ => {
                each(&view, inst, |item| match item {
                    Item::Value(v) => mark_escaped(v, &origin, &mut escaped),
                    Item::Edge(edge) => {
                        if let Some((start, count)) = edge.args {
                            for k in 0..count {
                                mark_escaped(at_u32(view.extra, start + k), &origin, &mut escaped);
                            }
                        }
                    }
                });
            }
        }
    }

    // Count promotable allocations.
    let mut promote_slot = filled(len, NONE, &alloc);
    let mut elided = filled(len, false, &alloc);
    let base_slot_count = to_u32(view.slots.len());
    let mut next_slot = base_slot_count;

    for (id, cand) in cands.iter().enumerate() {
        if !at_bool(&escaped, id) {
            set_at(&mut promote_slot, cand.inst, next_slot);
            next_slot += 1;
            stats.promoted_to_stack += 1;
        }
    }

    if stats.promoted_to_stack == 0 {
        return Ok(EscapeOutput { func: view.to_func_in(alloc), stats });
    }

    for inst in 0..len {
        let cid = at_u32(&dealloc_cand, inst);
        if cid != NONE && !at_bool(&escaped, to_idx(cid)) {
            set_at(&mut elided, inst, true);
            stats.deallocs_elided += 1;
        }
    }

    // Step 4: Rebuild the function with promoted `Op::SlotAddr`s and elided `Dealloc` calls.
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

    let temp_kind = slot_kind_byte(SlotKind::Temp);
    for (id, cand) in cands.iter().enumerate() {
        if !at_bool(&escaped, id) {
            func.slots.push(SlotRec {
                size: cand.size,
                align: cand.align_log2,
                kind: temp_kind,
                pad: [0; 2],
            });
        }
    }

    let mut val_map = filled(len, NONE, &alloc);
    let mut next_val = 0u32;
    for inst in 0..len {
        if !at_bool(&elided, inst) {
            set_at(&mut val_map, inst, next_val);
            next_val += 1;
        }
    }

    let slot_addr_op = op_byte(Op::SlotAddr);
    for b in 0..blocks {
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(to_u32(b))) {
            if at_bool(&elided, inst) {
                continue;
            }
            let slot_id = at_u32(&promote_slot, inst);
            if slot_id != NONE {
                func.ops.push(slot_addr_op);
                func.flags.push(Flags::NONE.0);
                func.tys.push(Ty::PTR.0);
                func.data.push(slot_id);
                func.attrs.push(0);
                func.locs.push(at_u32(view.locs, inst));
            } else {
                copy_remapped(&view, inst, &val_map, &mut func)?;
            }
        }
    }

    if u32::try_from(func.ops.len()).is_err() || u32::try_from(func.extra.len()).is_err() {
        return Err(SimplifyError::Refused { inst: 0, what: "an output past LIR's columns" });
    }
    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(EscapeOutput { func, stats })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        BlockKind,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{Conv, RefKind},
    };

    #[test]
    fn promote_heap_to_stack_and_sroa_folds_to_pure_registers() {
        // fn(x: i64, y: i64) -> i64:
        //   p = alloc(16, 8)
        //   store x at p + 0
        //   store y at p + 8
        //   lx = load p + 0
        //   ly = load p + 8
        //   dealloc(p)
        //   return lx + ly
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let alloc_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::PTR)],
        );
        let dealloc_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        let alloc_ref = b.reference(RefKind::Func, 0);
        let dealloc_ref = b.reference(RefKind::Func, 1);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let sz = b.iconst(Ty::I64, 16);
        let al = b.iconst(Ty::I64, 8);
        let p = b.call(Flags::NONE, alloc_ref, alloc_sig, &[sz, al]);
        b.store(Flags::NONE, x, p, 0, MemAttrs::default());
        let off8 = b.iconst(Ty::I64, 8);
        let p8 = b.ptr_add(Flags::NONE, p, off8);
        b.store(Flags::NONE, y, p8, 0, MemAttrs::default());
        let lx = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::default());
        let ly = b.load(Ty::I64, Flags::NONE, p8, 0, MemAttrs::default());
        let _ = b.call(Flags::NONE, dealloc_ref, dealloc_sig, &[p]);
        let sum = b.binary(Op::Add, Flags::NONE, lx, ly);
        b.ret(&[sum]);
        let func = b.finish_checked().unwrap();

        let classify = |r: u32| {
            if r == alloc_ref.0 {
                AllocRole::Alloc { size_arg: 0, align_arg: Some(1) }
            } else if r == dealloc_ref.0 {
                AllocRole::Dealloc { ptr_arg: 0 }
            } else {
                AllocRole::EscapingCall
            }
        };
        let promoted = promote_heap_to_stack(func.view(), classify, TestHeap).unwrap();
        assert_eq!(promoted.stats.allocations_analyzed, 1);
        assert_eq!(promoted.stats.promoted_to_stack, 1);
        assert_eq!(promoted.stats.deallocs_elided, 1);
        assert_eq!(promoted.func.slots.len(), 1);
        assert_eq!(promoted.func.slots[0].size, 16);
        assert_eq!(promoted.func.slots[0].align, 3); // 1 << 3 == 8

        let pv = promoted.func.view();
        assert_eq!((0..pv.len()).filter(|&i| pv.op(i) == Op::SlotAddr).count(), 1);
        assert_eq!((0..pv.len()).filter(|&i| pv.op(i) == Op::Call).count(), 0);

        // Chain SROA -> propagate -> dead to eliminate ALL memory ops and stack slots!
        let sroaed = super::super::sroa::scalar_replace(promoted.func.view(), TestHeap).unwrap();
        let proped = super::super::propagate(sroaed.view(), TestHeap).unwrap();
        let folded = super::super::dead(proped.view(), TestHeap).unwrap();
        assert!(folded.slots.is_empty(), "SROA should eliminate the promoted stack slot");
        let fv = folded.view();
        assert_eq!((0..fv.len()).filter(|&i| matches!(fv.op(i), Op::Load | Op::Store | Op::SlotAddr | Op::Call)).count(), 0);

        for &(a, c) in &[(0u128, 0u128), (7, 35), (123_456, 789_012), (u64::MAX as u128, 1)] {
            let Outcome::Returned(r_stack) =
                eval::run_in(promoted.func.view(), &[Val::Int(a), Val::Int(c)], 10_000, TestHeap).unwrap()
            else {
                panic!("promoted stack version did not return");
            };
            let Outcome::Returned(r_reg) =
                eval::run_in(folded.view(), &[Val::Int(a), Val::Int(c)], 10_000, TestHeap).unwrap()
            else {
                panic!("folded register version did not return");
            };
            let expected = (a as u64).wrapping_add(c as u64) as u128;
            assert_eq!(r_stack.as_slice(), &[Val::Int(expected)]);
            assert_eq!(r_reg.as_slice(), &[Val::Int(expected)]);
        }
    }

    #[test]
    fn escaping_allocations_are_not_promoted() {
        // Case 1: pointer returned directly from the function.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
        b.set_signature(sig);
        let alloc_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::PTR)]);
        let alloc_ref = b.reference(RefKind::Func, 0);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let sz = b.iconst(Ty::I64, 32);
        let p = b.call(Flags::NONE, alloc_ref, alloc_sig, &[sz]);
        b.ret(&[p]);
        let func_ret = b.finish_checked().unwrap();

        let out_ret = promote_heap_to_stack(
            func_ret.view(),
            |r| {
                if r == alloc_ref.0 {
                    AllocRole::Alloc { size_arg: 0, align_arg: None }
                } else {
                    AllocRole::EscapingCall
                }
            },
            TestHeap,
        )
        .unwrap();
        assert_eq!(out_ret.stats.allocations_analyzed, 1);
        assert_eq!(out_ret.stats.promoted_to_stack, 0);
        assert_eq!(out_ret.stats.deallocs_elided, 0);
        assert!(out_ret.func.slots.is_empty());

        // Case 2: pointer stored as a value into a caller-provided pointer.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        b.set_signature(sig);
        let alloc_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::PTR)]);
        let alloc_ref = b.reference(RefKind::Func, 0);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let out_slot = b.param(Ty::PTR);
        let sz = b.iconst(Ty::I64, 64);
        let p = b.call(Flags::NONE, alloc_ref, alloc_sig, &[sz]);
        b.store(Flags::NONE, p, out_slot, 0, MemAttrs::default());
        b.ret(&[]);
        let func_store = b.finish_checked().unwrap();

        let out_store = promote_heap_to_stack(
            func_store.view(),
            |r| {
                if r == alloc_ref.0 {
                    AllocRole::Alloc { size_arg: 0, align_arg: None }
                } else {
                    AllocRole::EscapingCall
                }
            },
            TestHeap,
        )
        .unwrap();
        assert_eq!(out_store.stats.allocations_analyzed, 1);
        assert_eq!(out_store.stats.promoted_to_stack, 0);
        assert!(out_store.func.slots.is_empty());
    }

    #[test]
    fn summarize_escape_distinguishes_readonly_and_escaping_params() {
        // fn(ro: ptr, ret_p: ptr, stored_p: ptr, cast_p: ptr, called_p: ptr, sink: ptr) -> ptr
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR), // 0: read-only local -> does NOT escape
                Param::of(Ty::PTR), // 1: returned -> escapes
                Param::of(Ty::PTR), // 2: stored as value -> escapes
                Param::of(Ty::PTR), // 3: ptr_addr cast -> escapes
                Param::of(Ty::PTR), // 4: passed to unknown call -> escapes
                Param::of(Ty::PTR), // 5: store destination address -> does NOT escape
            ],
            &[Param::of(Ty::PTR)],
        );
        b.set_signature(sig);
        let ext_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        let ext_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let ro = b.param(Ty::PTR);
        let ret_p = b.param(Ty::PTR);
        let stored_p = b.param(Ty::PTR);
        let cast_p = b.param(Ty::PTR);
        let called_p = b.param(Ty::PTR);
        let sink = b.param(Ty::PTR);

        let off = b.iconst(Ty::I64, 8);
        let ro_off = b.ptr_add(Flags::NONE, ro, off);
        let _loaded = b.load(Ty::I64, Flags::NONE, ro_off, 0, MemAttrs::default());
        b.store(Flags::NONE, stored_p, sink, 0, MemAttrs::default());
        let _addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, cast_p);
        let _ = b.call(Flags::NONE, ext_ref, ext_sig, &[called_p]);
        b.ret(&[ret_p]);
        let func = b.finish_checked().unwrap();

        let summary = summarize_escape(func.view(), TestHeap);
        assert_eq!(summary.param_escapes & (1 << 0), 0, "read-only param 0 must not escape");
        assert_ne!(summary.param_escapes & (1 << 1), 0, "returned param 1 must escape");
        assert_ne!(summary.param_escapes & (1 << 2), 0, "stored-value param 2 must escape");
        assert_ne!(summary.param_escapes & (1 << 3), 0, "ptr_addr param 3 must escape");
        assert_ne!(summary.param_escapes & (1 << 4), 0, "call-arg param 4 must escape");
        assert_eq!(summary.param_escapes & (1 << 5), 0, "store-dest param 5 must not escape");
        assert!(!summary.returns_fresh_alloc);
    }
}
