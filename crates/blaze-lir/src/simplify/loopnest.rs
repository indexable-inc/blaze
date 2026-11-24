//! Loop-Nest Optimizer: Adjacent Loop Fusion ([`fuse_adjacent_loops`]) and 2D Loop Interchange
//! + L1 Cache Tiling ([`interchange_and_tile_loops`]).
//!
//! - **Adjacent Loop Fusion**: merges consecutive counted loops over the same `[0, n)` iteration
//!   space when Loop 1 produces `tmp[i] = v` and Loop 2 consumes `tmp[j]`, forwarding `v`
//!   directly in SSA registers (eliminating memory reads of `tmp[j]` completely and cutting
//!   branch/IV overhead in half).
//! - **2D Loop Interchange & L1 Cache Tiling**: detects perfectly nested 2D rectangular reduction
//!   loops whose inner induction variable has strided column-major access (`mat[(j * M + i) * e]`)
//!   while the outer induction variable has unit stride (`1`), interchanging them so the inner
//!   loop streams linearly at stride 1 (unlocking 4x SIMD auto-vectorization and hardware
//!   prefetching) and optionally strip-mining into L1-resident tiles of width `tile_size`.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, dead, each, filled},
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    op::{Field, Layout},
    verify,
};

/// Telemetry returned by [`fuse_adjacent_loops`] and [`interchange_and_tile_loops`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct LoopNestStats {
    /// Number of adjacent loop pairs fused into a single loop.
    pub loops_fused: u32,
    /// Number of 2D strided loop nests interchanged to unit-stride inner order.
    pub loops_interchanged: u32,
    /// Number of interchanged loop nests tiled into L1-resident strips.
    pub loops_tiled: u32,
}

/// Output of [`fuse_adjacent_loops`] and [`interchange_and_tile_loops`].
#[derive(Debug)]
pub struct LoopNestOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: LoopNestStats,
}

// ---------------------------------------------------------------------------
// Low-level LIR emission & inspection helpers
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
fn trunc_u64(v: u128) -> u64 {
    u64::try_from(v & u128::from(u64::MAX)).unwrap_or(0)
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
fn block_kind_byte(kind: BlockKind) -> u8 {
    BlockKind::ALL.iter().position(|&k| k == kind).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn int_cond_word(c: IntCond) -> u32 {
    IntCond::ALL.iter().position(|&x| x == c).and_then(|i| u32::try_from(i).ok()).unwrap_or(0)
}

fn push_inst<A: Allocator>(
    func: &mut Func<A>,
    op: Op,
    flags: Flags,
    ty: Ty,
    data: u32,
    attr: u32,
) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(flags.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(0);
    idx
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
        push_inst(func, Op::IconstWide, Flags::NONE, ty, at, 0)
    }
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

fn copy_inst_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    let op = view.op(inst);
    let op_raw = at_u8(view.ops, inst);
    let flags = at_u8(view.flags, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let inst_data = at_u32(view.data, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at_u32(val_map, to_idx(inst_data)),
        Layout::Data(Field::Block) => at_u32(blk_map, to_idx(inst_data)),
        Layout::Data(_) => inst_data,
        Layout::Extra(fields) => {
            let src = to_idx(inst_data);
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p))));
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
                            func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p + k))));
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
                        let b = to_idx(at_u32(view.extra, p));
                        let n_u32 = at_u32(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(at_u32(blk_map, b));
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p + k))));
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
                            let b = to_idx(at_u32(view.extra, p));
                            let an_u32 = at_u32(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(at_u32(blk_map, b));
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p + k))));
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
    let idx = to_u32(func.ops.len());
    func.ops.push(op_raw);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

#[inline]
fn const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = Ty(view.tys.get(inst).copied()?);
    if !ty.is_scalar() || !ty.is_int() || ty.bits(view.head.ptr_bits) > 64 {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(view.data.get(inst).copied()?, width),
        Op::IconstWide => {
            let at = to_idx(view.data.get(inst).copied()?);
            let end = at.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(at..end)?, width)
        }
        _ => return None,
    };
    Some(trunc_u64(v))
}

#[inline]
fn same_val_or_const(view: &View<'_>, a: u32, b: u32) -> bool {
    if a == b {
        return true;
    }
    let ai = to_idx(a);
    let bi = to_idx(b);
    if at_u16(view.tys, ai) != at_u16(view.tys, bi) {
        return false;
    }
    match (const_u64(view, ai), const_u64(view, bi)) {
        (Some(ca), Some(cb)) => ca == cb,
        _ => false,
    }
}

#[inline]
fn access_bytes(ty: Ty, ptr_bits: u32) -> Option<u64> {
    if ty.is_ptr() {
        Some(u64::from(ptr_bits / 8))
    } else if ty.is_vector() {
        Some(u64::from(ty.elem().bits(ptr_bits) * ty.lanes() / 8))
    } else if ty.is_scalar() {
        let b = ty.bits(ptr_bits);
        if matches!(b, 8 | 16 | 32 | 64) { Some(u64::from(b / 8)) } else { None }
    } else {
        None
    }
}

/// Pure, non-trapping, side-effect-free operation safe to hoist or duplicate.
#[inline]
fn is_pure_op(op: Op) -> bool {
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Vconst
            | Op::Null
            | Op::Add
            | Op::Sub
            | Op::Mul
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
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
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
            | Op::Fneg
            | Op::Fabs
            | Op::Fma
            | Op::Fmuladd
            | Op::PtrAdd
            | Op::SlotAddr
    )
}

/// Visits every SSA value read by `inst`, including terminator target arguments.
fn for_each_use(view: &View<'_>, inst: usize, mut f: impl FnMut(u32)) {
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

fn build_cfg_info<A: Allocator + Copy>(view: &View<'_>, alloc: &A) -> (Vec<Vec<u32, A>, A>, Vec<u32, A>) {
    let blocks = view.blocks();
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(*alloc));
    }
    let mut inst_block = filled(view.len(), 0u32, alloc);
    for b in 0..blocks {
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            set_at(&mut inst_block, inst, b32);
        }
        let term = view.terminator(Block(b32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(pt) = preds.get_mut(tb) {
                    if pt.scan_for(&b32).is_none() {
                        pt.push(b32);
                    }
                }
            }
        });
    }
    (preds, inst_block)
}

/// Identifies local stack slots whose address never escapes (only flows into `PtrAdd`, `Load` ptr,
/// or `Store` ptr — never stored as a value, passed to a call, or converted to an integer).
fn compute_non_escaping_slots<A: Allocator + Copy>(view: &View<'_>, alloc: &A) -> Vec<bool, A> {
    let nslots = view.slots.len();
    let mut non_esc = filled(nslots, true, alloc);
    if nslots == 0 {
        return non_esc;
    }
    let len = view.len();
    let mut ptr_slot = filled(len, NONE, alloc);
    for inst in 0..len {
        match view.op(inst) {
            Op::SlotAddr => set_at(&mut ptr_slot, inst, at_u32(view.data, inst)),
            Op::PtrAdd => {
                let base = to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst))));
                let base_slot = at_u32(&ptr_slot, base);
                set_at(&mut ptr_slot, inst, base_slot);
            }
            _ => {}
        }
    }
    for inst in 0..len {
        let op = view.op(inst);
        if op == Op::PtrAdd {
            continue;
        }
        if op == Op::Load {
            continue;
        }
        if op == Op::Store {
            let stored = to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst))));
            let s = at_u32(&ptr_slot, stored);
            if s != NONE {
                set_at(&mut non_esc, to_idx(s), false);
            }
            continue;
        }
        for_each_use(view, inst, |v| {
            let s = at_u32(&ptr_slot, to_idx(v));
            if s != NONE {
                set_at(&mut non_esc, to_idx(s), false);
            }
        });
    }
    non_esc
}

/// Returns true if two loop-invariant pointer bases are guaranteed never to alias.
fn bases_provably_disjoint(view: &View<'_>, a: u32, b: u32, non_esc: &[bool]) -> bool {
    if a == b {
        return false;
    }
    let ai = to_idx(a);
    let bi = to_idx(b);
    let sa = if view.op(ai) == Op::SlotAddr { Some(at_u32(view.data, ai)) } else { None };
    let sb = if view.op(bi) == Op::SlotAddr { Some(at_u32(view.data, bi)) } else { None };
    match (sa, sb) {
        (Some(x), Some(y)) => x != y,
        (Some(x), None) => at_bool(non_esc, to_idx(x)),
        (None, Some(y)) => at_bool(non_esc, to_idx(y)),
        (None, None) => false,
    }
}

/// Checks whether `inst` computes `iv + 1` or `1 + iv`.
fn is_step_one(view: &View<'_>, inst: u32, iv: u32) -> bool {
    let i = to_idx(inst);
    if view.op(i) != Op::Add || view.flags(i) != Flags::NONE {
        return false;
    }
    let at = to_idx(at_u32(view.data, i));
    let (a, b) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
    (a == iv && const_u64(view, to_idx(b)) == Some(1))
        || (b == iv && const_u64(view, to_idx(a)) == Some(1))
}

/// Parsed loop header branch: `iv < bound` (unsigned or signed).
struct HeaderBranch<A: Allocator> {
    cond_inst: u32,
    bound: u32,
    is_signed: bool,
    body_blk: u32,
    exit_blk: u32,
    exit_args: Vec<u32, A>,
    /// True if `Br` takes `then` to `body_blk` and `else` to `exit_blk`.
    body_is_then: bool,
}

fn parse_header_branch<A: Allocator + Copy>(
    view: &View<'_>,
    header: u32,
    iv_inst: u32,
    expected_body: u32,
    alloc: &A,
) -> Option<HeaderBranch<A>> {
    let range = view.block(Block(header));
    let params = view.params(Block(header));
    let term = range.end - 1;
    if view.op(term) != Op::Br {
        return None;
    }
    // Header body must only contain pure instructions (Icmp + constants).
    for inst in params.end..term {
        if !is_pure_op(view.op(inst)) || view.flags(inst) != Flags::NONE {
            return None;
        }
    }
    let at = to_idx(at_u32(view.data, term));
    let cond_val = at_u32(view.extra, at);
    let t_blk = at_u32(view.extra, at + 1);
    let t_argc = to_idx(at_u32(view.extra, at + 2));
    let e_at = at + 3 + t_argc;
    let e_blk = at_u32(view.extra, e_at);
    let e_argc = to_idx(at_u32(view.extra, e_at + 1));

    let ci = to_idx(cond_val);
    if view.op(ci) != Op::Icmp {
        return None;
    }
    let cat = to_idx(at_u32(view.data, ci));
    let cond = IntCond::from_word(at_u32(view.extra, cat))?;
    let (lhs, rhs) = (at_u32(view.extra, cat + 1), at_u32(view.extra, cat + 2));

    let (body_is_then, exit_blk, exit_start, exit_argc, body_argc) = if t_blk == expected_body && e_blk != expected_body
    {
        (true, e_blk, e_at + 2, e_argc, t_argc)
    } else if e_blk == expected_body && t_blk != expected_body {
        (false, t_blk, at + 3, t_argc, e_argc)
    } else {
        return None;
    };
    if body_argc != 0 {
        return None;
    }

    let (bound, is_signed) = if body_is_then {
        if lhs == iv_inst && cond == IntCond::Ult {
            (rhs, false)
        } else if lhs == iv_inst && cond == IntCond::Slt {
            (rhs, true)
        } else if rhs == iv_inst && cond == IntCond::Ugt {
            (lhs, false)
        } else if rhs == iv_inst && cond == IntCond::Sgt {
            (lhs, true)
        } else {
            return None;
        }
    } else if lhs == iv_inst && cond == IntCond::Uge {
        (rhs, false)
    } else if lhs == iv_inst && cond == IntCond::Sge {
        (rhs, true)
    } else if rhs == iv_inst && cond == IntCond::Ule {
        (lhs, false)
    } else if rhs == iv_inst && cond == IntCond::Sle {
        (lhs, true)
    } else {
        return None;
    };

    let mut exit_args: Vec<u32, A> = Vec::with_capacity_in(exit_argc, *alloc);
    for k in 0..exit_argc {
        exit_args.push(at_u32(view.extra, exit_start + k));
    }

    Some(HeaderBranch {
        cond_inst: cond_val,
        bound,
        is_signed,
        body_blk: expected_body,
        exit_blk,
        exit_args,
        body_is_then,
    })
}

// ---------------------------------------------------------------------------
// Pass 1: Adjacent Loop Fusion (`fuse_adjacent_loops`)
// ---------------------------------------------------------------------------

struct SimpleLoop<A: Allocator> {
    header: u32,
    body: u32,
    exit: u32,
    exit_args: Vec<u32, A>,
    body_is_then: bool,
    pre_args: Vec<u32, A>,
    back_args: Vec<u32, A>,
    iv_pos: usize,
    iv_inst: u32,
    iv_ty: Ty,
    iv_init: u32,
    iv_next: u32,
    bound: u32,
    is_signed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct IndexedAccess {
    inst: u32,
    base: u32,
    elem_bytes: u64,
    const_off: i32,
    ty: Ty,
    store_val: u32,
}

/// Peels `iv`, `Zext(iv)`, `Shl(iv, s)`, or `Mul(iv, k)` iteratively to extract element byte stride.
fn extract_iv_byte_scale(view: &View<'_>, mut cur: u32, iv_inst: u32) -> Option<u64> {
    let mut scale = 1u64;
    for _ in 0..32 {
        if cur == iv_inst {
            return Some(scale);
        }
        let vi = to_idx(cur);
        match view.op(vi) {
            Op::Zext | Op::Sext => {
                return if at_u32(view.data, vi) == iv_inst { Some(scale) } else { None };
            }
            Op::Shl => {
                let at = to_idx(at_u32(view.data, vi));
                let (a, b) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                let s = const_u64(view, to_idx(b))?;
                if s > 4 {
                    return None;
                }
                scale = scale.checked_shl(u32::try_from(s).ok()?)?;
                cur = a;
            }
            Op::Mul => {
                let at = to_idx(at_u32(view.data, vi));
                let (a, b) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                if let Some(k) = const_u64(view, to_idx(b)).filter(|&k| k > 0) {
                    scale = scale.checked_mul(k)?;
                    cur = a;
                } else if let Some(k) = const_u64(view, to_idx(a)).filter(|&k| k > 0) {
                    scale = scale.checked_mul(k)?;
                    cur = b;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

/// Recognizes `Load`/`Store` at `PtrAdd(base, iv * elem_bytes) + const_off` with invariant `base`.
fn match_indexed_access(
    view: &View<'_>,
    inst: usize,
    iv_inst: u32,
    inst_block: &[u32],
    is_outside: impl Fn(u32) -> bool,
) -> Option<IndexedAccess> {
    let op = view.op(inst);
    if view.flags(inst).contains(Flags::VOLATILE) {
        return None;
    }
    let (ptr, const_off, ty, store_val) = match op {
        Op::Load => {
            let at = to_idx(at_u32(view.data, inst));
            (at_u32(view.extra, at), at_u32(view.extra, at + 1).cast_signed(), Ty(at_u16(view.tys, inst)), NONE)
        }
        Op::Store => {
            let at = to_idx(at_u32(view.data, inst));
            let sv = at_u32(view.extra, at);
            (
                at_u32(view.extra, at + 1),
                at_u32(view.extra, at + 2).cast_signed(),
                Ty(at_u16(view.tys, to_idx(sv))),
                sv,
            )
        }
        _ => return None,
    };
    let pi = to_idx(ptr);
    if view.op(pi) != Op::PtrAdd {
        return None;
    }
    let pat = to_idx(at_u32(view.data, pi));
    let base = at_u32(view.extra, pat);
    let byte_off = at_u32(view.extra, pat + 1);
    if !is_outside(at_u32(inst_block, to_idx(base))) {
        return None;
    }
    let elem_bytes = extract_iv_byte_scale(view, byte_off, iv_inst)?;
    let acc_bytes = access_bytes(ty, view.head.ptr_bits)?;
    if elem_bytes < acc_bytes {
        return None;
    }
    Some(IndexedAccess {
        inst: to_u32(inst),
        base,
        elem_bytes,
        const_off,
        ty,
        store_val,
    })
}

/// Parses a single counted loop `(header, body)` given its entering predecessor `enter_pred` and
/// incoming argument list `pre_args`.
fn parse_simple_loop<A: Allocator + Copy>(
    view: &View<'_>,
    header: u32,
    enter_pred: u32,
    pre_args: Vec<u32, A>,
    preds: &[Vec<u32, A>],
    inst_block: &[u32],
    alloc: &A,
) -> Option<SimpleLoop<A>> {
    if header == 0 || view.block_kind(Block(header)) != BlockKind::Normal {
        return None;
    }
    let h_preds = preds.get(to_idx(header))?;
    if h_preds.len() != 2 {
        return None;
    }
    let (p0, p1) = (*h_preds.first()?, *h_preds.get(1)?);
    let body = if p0 == enter_pred {
        p1
    } else if p1 == enter_pred {
        p0
    } else {
        return None;
    };
    if body == header || view.block_kind(Block(body)) != BlockKind::Normal {
        return None;
    }
    let b_preds = preds.get(to_idx(body))?;
    if b_preds.len() != 1 || b_preds.first().copied() != Some(header) {
        return None;
    }
    let body_term = view.terminator(Block(body));
    if view.op(body_term) != Op::Jump {
        return None;
    }
    let bat = to_idx(at_u32(view.data, body_term));
    if at_u32(view.extra, bat) != header {
        return None;
    }
    let h_params = view.params(Block(header));
    let b_argc = to_idx(at_u32(view.extra, bat + 1));
    if h_params.is_empty() || b_argc != h_params.len() || pre_args.len() != h_params.len() {
        return None;
    }
    let mut back_args: Vec<u32, A> = Vec::with_capacity_in(b_argc, *alloc);
    for k in 0..b_argc {
        back_args.push(at_u32(view.extra, bat + 2 + k));
    }

    // Find induction variable param (`step == +1`, `I64` or `I32`).
    for pos in 0..h_params.len() {
        let iv_inst = to_u32(h_params.start + pos);
        let iv_ty = Ty(at_u16(view.tys, to_idx(iv_inst)));
        if iv_ty != Ty::I64 && iv_ty != Ty::I32 {
            continue;
        }
        let iv_next = at_u32(&back_args, pos);
        if at_u32(inst_block, to_idx(iv_next)) != body || !is_step_one(view, iv_next, iv_inst) {
            continue;
        }
        let Some(hb) = parse_header_branch(view, header, iv_inst, body, alloc) else {
            continue;
        };
        let bound_b = at_u32(inst_block, to_idx(hb.bound));
        if bound_b == header || bound_b == body {
            continue;
        }
        let _ = hb.cond_inst;
        let iv_init = at_u32(&pre_args, pos);
        return Some(SimpleLoop {
            header,
            body,
            exit: hb.exit_blk,
            exit_args: hb.exit_args,
            body_is_then: hb.body_is_then,
            pre_args,
            back_args,
            iv_pos: pos,
            iv_inst,
            iv_ty,
            iv_init,
            iv_next,
            bound: hb.bound,
            is_signed: hb.is_signed,
        });
    }
    None
}

struct FusionPlan<A: Allocator> {
    pre1: u32,
    loop1: SimpleLoop<A>,
    /// Optional pure connector block (`loop1.exit == connector` jumping to `loop2.header`).
    connector: Option<u32>,
    loop2: SimpleLoop<A>,
    /// Per instruction index of `view`: `NONE`, or the SSA value in `body1` forwarded directly to
    /// replace a load in `body2`.
    forward: Vec<u32, A>,
}

fn find_fusion_candidate<A: Allocator + Copy>(view: &View<'_>, alloc: &A) -> Option<FusionPlan<A>> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 5 {
        return None;
    }
    let (preds, inst_block) = build_cfg_info(view, alloc);
    let non_esc = compute_non_escaping_slots(view, alloc);

    for h1 in 1..blocks {
        let header1 = to_u32(h1);
        let Some(h1_preds) = preds.get(h1) else {
            continue;
        };
        if h1_preds.len() != 2 {
            continue;
        }
        // Identify `pre1` as the predecessor of `header1` whose terminator is `Jump(header1)` and
        // is not the single-pred body block.
        let mut pre1 = NONE;
        for &p in h1_preds {
            if let Some(pp) = preds.get(to_idx(p)) {
                if pp.len() == 1 && pp.first().copied() == Some(header1) {
                    continue;
                }
            }
            if view.op(view.terminator(Block(p))) == Op::Jump {
                pre1 = p;
            }
        }
        if pre1 == NONE {
            continue;
        }
        let p1_term = view.terminator(Block(pre1));
        let p1_at = to_idx(at_u32(view.data, p1_term));
        if at_u32(view.extra, p1_at) != header1 {
            continue;
        }
        let p1_argc = to_idx(at_u32(view.extra, p1_at + 1));
        let mut pre1_args: Vec<u32, A> = Vec::with_capacity_in(p1_argc, *alloc);
        for k in 0..p1_argc {
            pre1_args.push(at_u32(view.extra, p1_at + 2 + k));
        }

        let Some(loop1) = parse_simple_loop(view, header1, pre1, pre1_args, &preds, &inst_block, alloc) else {
            continue;
        };

        // Check how `loop1.exit` connects to `loop2`:
        // Case A: `loop1.exit` is a pure single-predecessor preheader jumping to `header2`.
        // Case B: `loop1.exit == header2` directly.
        let (connector, header2, enter_pred2, pre2_args) = {
            let ex = loop1.exit;
            if view.block_kind(Block(ex)) != BlockKind::Normal {
                continue;
            }
            let Some(ex_preds) = preds.get(to_idx(ex)) else {
                continue;
            };
            if ex_preds.len() == 1 && ex_preds.first().copied() == Some(loop1.header) {
                let ex_range = view.block(Block(ex));
                let ex_params = view.params(Block(ex));
                let ex_term = ex_range.end - 1;
                if !ex_params.is_empty() || view.op(ex_term) != Op::Jump {
                    continue;
                }
                let mut conn_ok = true;
                for inst in ex_range.start..ex_term {
                    if !is_pure_op(view.op(inst)) || view.flags(inst) != Flags::NONE {
                        conn_ok = false;
                        break;
                    }
                    for_each_use(view, inst, |v| {
                        let b = at_u32(&inst_block, to_idx(v));
                        if b == loop1.header || b == loop1.body {
                            conn_ok = false;
                        }
                    });
                }
                if !conn_ok {
                    continue;
                }
                let jat = to_idx(at_u32(view.data, ex_term));
                let h2 = at_u32(view.extra, jat);
                let argc2 = to_idx(at_u32(view.extra, jat + 1));
                let mut args2: Vec<u32, A> = Vec::with_capacity_in(argc2, *alloc);
                for k in 0..argc2 {
                    let a = at_u32(view.extra, jat + 2 + k);
                    let b = at_u32(&inst_block, to_idx(a));
                    if b == loop1.header || b == loop1.body {
                        conn_ok = false;
                    }
                    args2.push(a);
                }
                if !conn_ok {
                    continue;
                }
                (Some(ex), h2, ex, args2)
            } else if ex_preds.len() == 2 {
                // Direct edge `header1 -> header2`: ensure `exit_args` don't depend on loop1.
                let mut ok = true;
                for &a in &loop1.exit_args {
                    let b = at_u32(&inst_block, to_idx(a));
                    if b == loop1.header || b == loop1.body {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    continue;
                }
                let mut args2: Vec<u32, A> = Vec::with_capacity_in(loop1.exit_args.len(), *alloc);
                for &a in &loop1.exit_args {
                    args2.push(a);
                }
                (None, ex, loop1.header, args2)
            } else {
                continue;
            }
        };

        if header2 == loop1.header || header2 == loop1.body || header2 == pre1 {
            continue;
        }
        let Some(loop2) = parse_simple_loop(view, header2, enter_pred2, pre2_args, &preds, &inst_block, alloc) else {
            continue;
        };
        if loop2.body == loop1.header || loop2.body == loop1.body || loop2.body == pre1 {
            continue;
        }

        // Trip count, IV type, signedness, and start value must match identically!
        if loop1.iv_ty != loop2.iv_ty
            || loop1.is_signed != loop2.is_signed
            || !same_val_or_const(view, loop1.iv_init, loop2.iv_init)
            || !same_val_or_const(view, loop1.bound, loop2.bound)
        {
            continue;
        }

        let in_either_loop = |b: u32| {
            b == loop1.header
                || b == loop1.body
                || b == loop2.header
                || b == loop2.body
                || connector == Some(b)
        };
        if in_either_loop(at_u32(&inst_block, to_idx(loop1.bound))) {
            continue;
        }

        // Inspect all instructions in `body1` and `body2` for safety and memory dependences.
        let mut stores1: Vec<IndexedAccess, A> = Vec::new_in(*alloc);
        let mut loads2: Vec<IndexedAccess, A> = Vec::new_in(*alloc);
        let mut body_ok = true;

        let b1_range = view.block(Block(loop1.body));
        for inst in b1_range.start..(b1_range.end - 1) {
            let op = view.op(inst);
            if op == Op::Store {
                let Some(acc) = match_indexed_access(view, inst, loop1.iv_inst, &inst_block, |b| !in_either_loop(b))
                else {
                    body_ok = false;
                    break;
                };
                stores1.push(acc);
            } else if op == Op::Load {
                if view.flags(inst).contains(Flags::VOLATILE) {
                    body_ok = false;
                    break;
                }
            } else if !is_pure_op(op) {
                body_ok = false;
                break;
            }
        }
        if !body_ok {
            continue;
        }

        let b2_range = view.block(Block(loop2.body));
        for inst in b2_range.start..(b2_range.end - 1) {
            let op = view.op(inst);
            if op == Op::Load {
                let Some(acc) = match_indexed_access(view, inst, loop2.iv_inst, &inst_block, |b| !in_either_loop(b))
                else {
                    body_ok = false;
                    break;
                };
                loads2.push(acc);
            } else if op == Op::Store {
                // Only fuse pure-consumer / reduction Loop 2 (no stores in body2 to perturb body1 reads).
                body_ok = false;
                break;
            } else if !is_pure_op(op) {
                body_ok = false;
                break;
            }
            // Ensure no instruction in `body2` depends directly on `loop2.iv_next`.
            if inst != to_idx(loop2.iv_next) {
                for_each_use(view, inst, |v| {
                    if v == loop2.iv_next {
                        body_ok = false;
                    }
                });
            }
        }
        if !body_ok {
            continue;
        }

        // Check store-to-store aliasing inside `body1` (ensure each forwarded store is never
        // overwritten by another store in `body1`).
        let mut s1_disjoint = true;
        for i in 0..stores1.len() {
            let Some(si) = stores1.get(i) else {
                continue;
            };
            for j in (i + 1)..stores1.len() {
                let Some(sj) = stores1.get(j) else {
                    continue;
                };
                if !bases_provably_disjoint(view, si.base, sj.base, &non_esc) {
                    s1_disjoint = false;
                    break;
                }
            }
        }
        if !s1_disjoint {
            continue;
        }

        // Match every load `L2` in `body2`: either forward directly from an exact matching store
        // `S1` in `body1`, or prove `L2` is disjoint from every store in `body1`.
        let mut forward = filled(len, NONE, alloc);
        let mut mem_legal = true;
        for l2 in &loads2 {
            let mut matched = None;
            for s1 in &stores1 {
                if s1.base == l2.base
                    && s1.elem_bytes == l2.elem_bytes
                    && s1.const_off == l2.const_off
                    && s1.ty == l2.ty
                {
                    matched = Some(s1.store_val);
                    break;
                }
            }
            if let Some(sv) = matched {
                set_at(&mut forward, to_idx(l2.inst), sv);
            } else {
                for s1 in &stores1 {
                    if !bases_provably_disjoint(view, l2.base, s1.base, &non_esc) {
                        mem_legal = false;
                        break;
                    }
                }
                if !mem_legal {
                    break;
                }
            }
        }
        if !mem_legal {
            continue;
        }

        // Verify `loop2.iv_next` is not used as an exit value or anywhere outside `body2`'s jump.
        let mut iv2_ok = true;
        for b in 0..blocks {
            let b32 = to_u32(b);
            if b32 == loop2.body {
                continue;
            }
            for inst in view.block(Block(b32)) {
                for_each_use(view, inst, |v| {
                    if v == loop2.iv_next {
                        iv2_ok = false;
                    }
                });
            }
        }
        if !iv2_ok {
            continue;
        }

        return Some(FusionPlan {
            pre1,
            loop1,
            connector,
            loop2,
            forward,
        });
    }

    None
}

/// Fuses consecutive counted loops over the same `[0, n)` trip count into a single loop,
/// forwarding producer stores (`tmp[i] = v`) directly in SSA registers to eliminate consumer
/// loads (`x = tmp[j]`).
pub fn fuse_adjacent_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<LoopNestOutput<A>, SimplifyError> {
    let mut cur = view.to_func_in(alloc);
    let mut fused_count = 0u32;

    for _ in 0..8 {
        let Some(plan) = find_fusion_candidate(&cur.view(), &alloc) else {
            break;
        };
        let v = cur.view();
        let blocks = v.blocks();
        let len = v.len();

        let is_removed = |b: usize| -> bool {
            let bu = to_u32(b);
            bu == plan.loop2.header || bu == plan.loop2.body || plan.connector == Some(bu)
        };

        let mut blk_map = filled(blocks, NONE, &alloc);
        let mut next_blk = 0u32;
        for b in 0..blocks {
            if !is_removed(b) {
                set_at(&mut blk_map, b, next_blk);
                next_blk += 1;
            }
        }

        let mut func = v.to_func_in(alloc);
        clear_code_columns(&mut func);
        let mut val_map = filled(len, NONE, &alloc);

        let h1_params = v.params(Block(plan.loop1.header));
        let h2_params = v.params(Block(plan.loop2.header));
        let h1_new = at_u32(&blk_map, to_idx(plan.loop1.header));
        let b1_new = at_u32(&blk_map, to_idx(plan.loop1.body));
        let exit2_new = at_u32(&blk_map, to_idx(plan.loop2.exit));

        for b in 0..blocks {
            if is_removed(b) {
                continue;
            }
            let bu = to_u32(b);
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(at_u8(v.block_kinds, b));
            let range = v.block(Block(bu));

            if bu == plan.pre1 {
                let term = range.end - 1;
                for inst in range.start..term {
                    let mapped = copy_inst_remapped(&v, inst, &val_map, &blk_map, &mut func);
                    set_at(&mut val_map, inst, mapped);
                }
                // Hoist connector's pure setup instructions (e.g., `init_acc = iconst 0`) into `pre1`.
                if let Some(conn) = plan.connector {
                    let cr = v.block(Block(conn));
                    for inst in cr.start..(cr.end - 1) {
                        let mapped = copy_inst_remapped(&v, inst, &val_map, &blk_map, &mut func);
                        set_at(&mut val_map, inst, mapped);
                    }
                }
                let mut fused_pre_args: Vec<u32, A> =
                    Vec::with_capacity_in(plan.loop1.pre_args.len() + plan.loop2.pre_args.len(), alloc);
                for &a in &plan.loop1.pre_args {
                    fused_pre_args.push(at_u32(&val_map, to_idx(a)));
                }
                for (pos, &a) in plan.loop2.pre_args.iter().enumerate() {
                    if pos != plan.loop2.iv_pos {
                        fused_pre_args.push(at_u32(&val_map, to_idx(a)));
                    }
                }
                push_jump(&mut func, h1_new, &fused_pre_args);
            } else if bu == plan.loop1.header {
                let term = range.end - 1;
                // Emit `header1` params, then `header2` non-IV params.
                for p in h1_params.start..h1_params.end {
                    let mapped = copy_inst_remapped(&v, p, &val_map, &blk_map, &mut func);
                    set_at(&mut val_map, p, mapped);
                }
                let iv1_mapped = at_u32(&val_map, to_idx(plan.loop1.iv_inst));
                set_at(&mut val_map, to_idx(plan.loop2.iv_inst), iv1_mapped);
                for pos in 0..h2_params.len() {
                    if pos != plan.loop2.iv_pos {
                        let p2 = h2_params.start + pos;
                        let new_p = push_inst(&mut func, Op::Param, Flags::NONE, Ty(at_u16(v.tys, p2)), 0, 0);
                        set_at(&mut val_map, p2, new_p);
                    }
                }
                // Copy `header1` condition instructions.
                for inst in h1_params.end..term {
                    let mapped = copy_inst_remapped(&v, inst, &val_map, &blk_map, &mut func);
                    set_at(&mut val_map, inst, mapped);
                }
                let br_at = to_idx(at_u32(v.data, term));
                let cond_new = at_u32(&val_map, to_idx(at_u32(v.extra, br_at)));
                let mut exit2_args_new: Vec<u32, A> =
                    Vec::with_capacity_in(plan.loop2.exit_args.len(), alloc);
                for &ea in &plan.loop2.exit_args {
                    exit2_args_new.push(at_u32(&val_map, to_idx(ea)));
                }
                if plan.loop1.body_is_then {
                    push_br(&mut func, cond_new, b1_new, &[], exit2_new, &exit2_args_new);
                } else {
                    push_br(&mut func, cond_new, exit2_new, &exit2_args_new, b1_new, &[]);
                }
            } else if bu == plan.loop1.body {
                let term1 = range.end - 1;
                for inst in range.start..term1 {
                    let mapped = copy_inst_remapped(&v, inst, &val_map, &blk_map, &mut func);
                    set_at(&mut val_map, inst, mapped);
                }
                let r2 = v.block(Block(plan.loop2.body));
                let term2 = r2.end - 1;
                for inst in r2.start..term2 {
                    if inst == to_idx(plan.loop2.iv_next) {
                        continue;
                    }
                    let fwd = at_u32(&plan.forward, inst);
                    if fwd != NONE {
                        // Forward producer's SSA value directly, eliminating the load of `tmp[j]`!
                        let fwd_mapped = at_u32(&val_map, to_idx(fwd));
                        set_at(&mut val_map, inst, fwd_mapped);
                    } else {
                        let mapped = copy_inst_remapped(&v, inst, &val_map, &blk_map, &mut func);
                        set_at(&mut val_map, inst, mapped);
                    }
                }
                let mut fused_back_args: Vec<u32, A> =
                    Vec::with_capacity_in(plan.loop1.back_args.len() + plan.loop2.back_args.len(), alloc);
                for &a in &plan.loop1.back_args {
                    fused_back_args.push(at_u32(&val_map, to_idx(a)));
                }
                for (pos, &a) in plan.loop2.back_args.iter().enumerate() {
                    if pos != plan.loop2.iv_pos {
                        fused_back_args.push(at_u32(&val_map, to_idx(a)));
                    }
                }
                push_jump(&mut func, h1_new, &fused_back_args);
            } else {
                for inst in range {
                    let mapped = copy_inst_remapped(&v, inst, &val_map, &blk_map, &mut func);
                    set_at(&mut val_map, inst, mapped);
                }
            }
        }

        verify(func.view()).map_err(SimplifyError::Verify)?;
        cur = dead(func.view(), alloc)?;
        fused_count += 1;
    }

    Ok(LoopNestOutput {
        func: cur,
        stats: LoopNestStats {
            loops_fused: fused_count,
            loops_interchanged: 0,
            loops_tiled: 0,
        },
    })
}

// ---------------------------------------------------------------------------
// Pass 2: 2D Loop Interchange + L1 Cache Tiling (`interchange_and_tile_loops`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stride {
    Zero,
    Const(u64),
    Dynamic,
    Complex,
}

impl Stride {
    fn scale_const(self, k: u64) -> Stride {
        if k == 0 {
            return Stride::Zero;
        }
        match self {
            Stride::Zero => Stride::Zero,
            Stride::Const(c) => c.checked_mul(k).map_or(Stride::Complex, Stride::Const),
            Stride::Dynamic => Stride::Dynamic,
            Stride::Complex => Stride::Complex,
        }
    }

    fn scale_dynamic(self) -> Stride {
        match self {
            Stride::Zero => Stride::Zero,
            Stride::Const(_) | Stride::Dynamic => Stride::Dynamic,
            Stride::Complex => Stride::Complex,
        }
    }

    fn combine_add(a: Stride, b: Stride) -> Stride {
        match (a, b) {
            (Stride::Zero, x) | (x, Stride::Zero) => x,
            _ => Stride::Complex,
        }
    }
}

#[derive(Clone, Copy)]
enum StrideBinOp {
    Add,
    Sub,
    Mul { a: u32, b: u32 },
}

#[derive(Clone, Copy)]
enum StrideFrame {
    Empty,
    ShlWait { b: u32 },
    BinWaitLhs { kind: StrideBinOp, rhs: u32, rhs_depth: u32 },
    BinWaitRhs { kind: StrideBinOp, lhs: Stride },
}

/// Computes the byte stride of `target_iv` inside an address or index expression `val` iteratively.
fn analyze_iv_stride(
    view: &View<'_>,
    val: u32,
    target_iv: u32,
    other_iv: u32,
    inst_block: &[u32],
    in_nest: &[bool],
    depth: u32,
) -> Stride {
    let mut frames = [StrideFrame::Empty; 16];
    let mut sp = 0usize;
    let mut cur_val = val;
    let mut cur_depth = depth;

    for _ in 0..16_384 {
        let mut result: Option<Stride> = None;
        loop {
            if cur_depth > 12 {
                result = Some(Stride::Complex);
                break;
            }
            if cur_val == target_iv {
                result = Some(Stride::Const(1));
                break;
            }
            if cur_val == other_iv {
                result = Some(Stride::Zero);
                break;
            }
            let vi = to_idx(cur_val);
            if !at_bool(in_nest, to_idx(at_u32(inst_block, vi))) {
                result = Some(Stride::Zero);
                break;
            }
            match view.op(vi) {
                Op::Iconst | Op::IconstWide => {
                    result = Some(Stride::Zero);
                    break;
                }
                Op::Zext | Op::Sext | Op::Trunc => {
                    cur_val = at_u32(view.data, vi);
                    cur_depth += 1;
                }
                Op::Add | Op::PtrAdd => {
                    let at = to_idx(at_u32(view.data, vi));
                    let (a, b) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    if sp >= frames.len() {
                        result = Some(Stride::Complex);
                        break;
                    }
                    set_at(
                        &mut frames,
                        sp,
                        StrideFrame::BinWaitLhs { kind: StrideBinOp::Add, rhs: b, rhs_depth: cur_depth + 1 },
                    );
                    sp += 1;
                    cur_val = a;
                    cur_depth += 1;
                }
                Op::Sub => {
                    let at = to_idx(at_u32(view.data, vi));
                    let (a, b) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    if sp >= frames.len() {
                        result = Some(Stride::Complex);
                        break;
                    }
                    set_at(
                        &mut frames,
                        sp,
                        StrideFrame::BinWaitLhs { kind: StrideBinOp::Sub, rhs: b, rhs_depth: cur_depth + 1 },
                    );
                    sp += 1;
                    cur_val = a;
                    cur_depth += 1;
                }
                Op::Shl => {
                    let at = to_idx(at_u32(view.data, vi));
                    let (a, b) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    if sp >= frames.len() {
                        result = Some(Stride::Complex);
                        break;
                    }
                    set_at(&mut frames, sp, StrideFrame::ShlWait { b });
                    sp += 1;
                    cur_val = a;
                    cur_depth += 1;
                }
                Op::Mul => {
                    let at = to_idx(at_u32(view.data, vi));
                    let (a, b) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    if sp >= frames.len() {
                        result = Some(Stride::Complex);
                        break;
                    }
                    set_at(
                        &mut frames,
                        sp,
                        StrideFrame::BinWaitLhs {
                            kind: StrideBinOp::Mul { a, b },
                            rhs: b,
                            rhs_depth: cur_depth + 1,
                        },
                    );
                    sp += 1;
                    cur_val = a;
                    cur_depth += 1;
                }
                _ => {
                    result = Some(Stride::Complex);
                    break;
                }
            }
        }

        let Some(mut res) = result else {
            return Stride::Complex;
        };
        let mut descend_rhs = false;
        while sp > 0 {
            sp -= 1;
            match frames.get(sp).copied().unwrap_or(StrideFrame::Empty) {
                StrideFrame::Empty => return Stride::Complex,
                StrideFrame::ShlWait { b } => {
                    res = if res == Stride::Zero {
                        Stride::Zero
                    } else if let Some(sh) = const_u64(view, to_idx(b)).filter(|&sh| sh <= 12) {
                        res.scale_const(1u64 << sh)
                    } else if !at_bool(in_nest, to_idx(at_u32(inst_block, to_idx(b)))) {
                        res.scale_dynamic()
                    } else {
                        Stride::Complex
                    };
                }
                StrideFrame::BinWaitLhs { kind, rhs, rhs_depth } => {
                    set_at(&mut frames, sp, StrideFrame::BinWaitRhs { kind, lhs: res });
                    sp += 1;
                    cur_val = rhs;
                    cur_depth = rhs_depth;
                    descend_rhs = true;
                    break;
                }
                StrideFrame::BinWaitRhs { kind, lhs } => {
                    res = match kind {
                        StrideBinOp::Add => Stride::combine_add(lhs, res),
                        StrideBinOp::Sub => if res == Stride::Zero { lhs } else { Stride::Complex },
                        StrideBinOp::Mul { a, b } => match (lhs, res) {
                            (Stride::Zero, Stride::Zero) => Stride::Zero,
                            (x, Stride::Zero) => {
                                if let Some(k) = const_u64(view, to_idx(b)) {
                                    x.scale_const(k)
                                } else if !at_bool(in_nest, to_idx(at_u32(inst_block, to_idx(b)))) {
                                    x.scale_dynamic()
                                } else {
                                    Stride::Complex
                                }
                            }
                            (Stride::Zero, y) => {
                                if let Some(k) = const_u64(view, to_idx(a)) {
                                    y.scale_const(k)
                                } else if !at_bool(in_nest, to_idx(at_u32(inst_block, to_idx(a)))) {
                                    y.scale_dynamic()
                                } else {
                                    Stride::Complex
                                }
                            }
                            _ => Stride::Complex,
                        },
                    };
                }
            }
        }
        if !descend_rhs {
            return res;
        }
    }
    Stride::Complex
}

#[inline]
fn is_assoc_comm_reduction(op: Op, flags: Flags) -> bool {
    match op {
        Op::Add | Op::Mul | Op::Xor | Op::Or | Op::And => flags == Flags::NONE,
        Op::Fadd | Op::Fmul => flags.contains(Flags::REASSOC),
        _ => false,
    }
}

#[derive(Clone, Copy, Debug)]
struct NestReduction {
    outer_param_inst: u32,
    inner_param_inst: u32,
    init_val: u32,
    update_inst: u32,
    ty: Ty,
}

struct InterchangePlan<A: Allocator> {
    preheader: u32,
    outer_hdr: u32,
    inner_pre: Option<u32>,
    inner_hdr: u32,
    inner_body: u32,
    outer_latch: u32,
    exit: u32,
    exit_args: Vec<u32, A>,
    iv_ty: Ty,
    is_signed: bool,
    outer_iv: u32,
    outer_init: u32,
    outer_bound: u32,
    inner_iv: u32,
    inner_init: u32,
    inner_bound: u32,
    inner_iv_next: u32,
    reductions: Vec<NestReduction, A>,
    /// Instructions in `inner_body` hoisted into the new outer loop body (`j_body`).
    hoisted_j_insts: Vec<u32, A>,
    /// Remaining payload instructions executed inside the new unit-stride inner loop (`i_body`).
    payload_insts: Vec<u32, A>,
    /// Mask of blocks belonging to the original 2D loop nest.
    in_nest: Vec<bool, A>,
}

fn find_interchange_candidate<A: Allocator + Copy>(
    view: &View<'_>,
    alloc: &A,
) -> Option<InterchangePlan<A>> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 5 {
        return None;
    }
    let (preds, inst_block) = build_cfg_info(view, alloc);

    for oh in 1..blocks {
        let outer_hdr = to_u32(oh);
        let Some(oh_preds) = preds.get(oh) else {
            continue;
        };
        if view.block_kind(Block(outer_hdr)) != BlockKind::Normal || oh_preds.len() != 2 {
            continue;
        }
        // Identify `preheader` (outside) and `outer_latch` (back-edge) of `outer_hdr`.
        let (Some(&p0), Some(&p1)) = (oh_preds.first(), oh_preds.get(1)) else {
            continue;
        };
        let (preheader, outer_latch) = if p0 < outer_hdr && p1 >= outer_hdr {
            (p0, p1)
        } else if p1 < outer_hdr && p0 >= outer_hdr {
            (p1, p0)
        } else {
            continue;
        };
        if view.block_kind(Block(outer_latch)) != BlockKind::Normal {
            continue;
        }
        let pre_term = view.terminator(Block(preheader));
        let latch_term = view.terminator(Block(outer_latch));
        if view.op(pre_term) != Op::Jump || view.op(latch_term) != Op::Jump {
            continue;
        }
        let pre_at = to_idx(at_u32(view.data, pre_term));
        let lat_at = to_idx(at_u32(view.data, latch_term));
        if at_u32(view.extra, pre_at) != outer_hdr || at_u32(view.extra, lat_at) != outer_hdr {
            continue;
        }
        let oh_params = view.params(Block(outer_hdr));
        let oh_pc = oh_params.len();
        if oh_pc < 2
            || to_idx(at_u32(view.extra, pre_at + 1)) != oh_pc
            || to_idx(at_u32(view.extra, lat_at + 1)) != oh_pc
        {
            continue;
        }

        // Find outer IV `i` (incremented by +1 inside `outer_latch`).
        let mut outer_iv_pos = None;
        for pos in 0..oh_pc {
            let p_inst = to_u32(oh_params.start + pos);
            let ty = Ty(at_u16(view.tys, to_idx(p_inst)));
            if ty != Ty::I64 && ty != Ty::I32 {
                continue;
            }
            let back_v = at_u32(view.extra, lat_at + 2 + pos);
            if at_u32(&inst_block, to_idx(back_v)) == outer_latch && is_step_one(view, back_v, p_inst) {
                outer_iv_pos = Some(pos);
                break;
            }
        }
        let Some(outer_iv_pos) = outer_iv_pos else {
            continue;
        };
        let outer_iv = to_u32(oh_params.start + outer_iv_pos);
        let iv_ty = Ty(at_u16(view.tys, to_idx(outer_iv)));
        let outer_init = at_u32(view.extra, pre_at + 2 + outer_iv_pos);

        // Parse `outer_hdr`'s branch (`i < M` -> `outer_body_target`, else `exit`).
        // Discover `outer_body_target` from `outer_hdr`'s `Op::Br`.
        let oh_term = view.terminator(Block(outer_hdr));
        if view.op(oh_term) != Op::Br {
            continue;
        }
        let oh_br_at = to_idx(at_u32(view.data, oh_term));
        let oh_t = at_u32(view.extra, oh_br_at + 1);
        let oh_t_argc = to_idx(at_u32(view.extra, oh_br_at + 2));
        let oh_e = at_u32(view.extra, oh_br_at + 3 + oh_t_argc);
        // Try both arms as the loop-entry target (`outer_body_target`).
        let outer_branch = parse_header_branch(view, outer_hdr, outer_iv, oh_t, alloc)
            .or_else(|| parse_header_branch(view, outer_hdr, outer_iv, oh_e, alloc));
        let Some(outer_hb) = outer_branch else {
            continue;
        };
        let outer_bound = outer_hb.bound;
        let exit = outer_hb.exit_blk;
        let enter_target = outer_hb.body_blk;

        // `enter_target` is either `inner_pre` (pure setup block jumping to `inner_hdr`) or `inner_hdr` itself.
        let (inner_pre, inner_hdr, inner_enter_pred, inner_pre_args) =
            if let Some(et_preds) = preds.get(to_idx(enter_target))
                && et_preds.len() == 1
                && et_preds.first().copied() == Some(outer_hdr)
            {
                let ip = enter_target;
                if view.block_kind(Block(ip)) != BlockKind::Normal || !view.params(Block(ip)).is_empty() {
                    continue;
                }
                let ip_range = view.block(Block(ip));
                let ip_term = ip_range.end - 1;
                if view.op(ip_term) != Op::Jump {
                    continue;
                }
                let mut pure_ok = true;
                for inst in ip_range.start..ip_term {
                    if !is_pure_op(view.op(inst)) || view.flags(inst) != Flags::NONE {
                        pure_ok = false;
                        break;
                    }
                    for_each_use(view, inst, |v| {
                        let b = at_u32(&inst_block, to_idx(v));
                        if b == outer_hdr {
                            pure_ok = false;
                        }
                    });
                }
                if !pure_ok {
                    continue;
                }
                let jat = to_idx(at_u32(view.data, ip_term));
                let ih = at_u32(view.extra, jat);
                let argc = to_idx(at_u32(view.extra, jat + 1));
                let mut args: Vec<u32, A> = Vec::with_capacity_in(argc, *alloc);
                for k in 0..argc {
                    args.push(at_u32(view.extra, jat + 2 + k));
                }
                (Some(ip), ih, ip, args)
            } else {
                // Direct `outer_hdr -> inner_hdr` (only when `inner_hdr` receives args from `Br`,
                // which `parse_header_branch` requires empty, so inner_pre is the normal path).
                continue;
            };

        let Some(inner_loop) =
            parse_simple_loop(view, inner_hdr, inner_enter_pred, inner_pre_args, &preds, &inst_block, alloc)
        else {
            continue;
        };
        if inner_loop.exit != outer_latch || !inner_loop.exit_args.is_empty() {
            continue;
        }
        if inner_loop.iv_ty != iv_ty || inner_loop.is_signed != outer_hb.is_signed {
            continue;
        }
        let Some(ol_preds) = preds.get(to_idx(outer_latch)) else {
            continue;
        };
        if ol_preds.len() != 1
            || ol_preds.first().copied() != Some(inner_hdr)
            || !view.params(Block(outer_latch)).is_empty()
        {
            continue;
        }
        // `outer_latch` must only contain pure instructions (computing `i_next`).
        let lat_range = view.block(Block(outer_latch));
        let mut latch_ok = true;
        for inst in lat_range.start..(lat_range.end - 1) {
            if !is_pure_op(view.op(inst)) || view.flags(inst) != Flags::NONE {
                latch_ok = false;
                break;
            }
        }
        if !latch_ok {
            continue;
        }

        let inner_body = inner_loop.body;
        let mut in_nest = filled(blocks, false, alloc);
        set_at(&mut in_nest, to_idx(outer_hdr), true);
        if let Some(ip) = inner_pre {
            set_at(&mut in_nest, to_idx(ip), true);
        }
        set_at(&mut in_nest, to_idx(inner_hdr), true);
        set_at(&mut in_nest, to_idx(inner_body), true);
        set_at(&mut in_nest, to_idx(outer_latch), true);

        // Bounds & initial IVs must be invariant outside the ENTIRE loop nest (rectangular space!).
        if at_bool(&in_nest, to_idx(at_u32(&inst_block, to_idx(outer_bound))))
            || at_bool(&in_nest, to_idx(at_u32(&inst_block, to_idx(inner_loop.bound))))
            || at_bool(&in_nest, to_idx(at_u32(&inst_block, to_idx(outer_init))))
        {
            continue;
        }
        // `inner_init` may be defined outside the nest OR as a pure constant inside `inner_pre`.
        let ii_blk = at_u32(&inst_block, to_idx(inner_loop.iv_init));
        if at_bool(&in_nest, to_idx(ii_blk)) && inner_pre != Some(ii_blk) {
            continue;
        }

        // Verify all other params of `outer_hdr` and `inner_hdr` are 1-to-1 associative+commutative reductions!
        let ih_params = view.params(Block(inner_hdr));
        if ih_params.len() != oh_pc {
            continue;
        }
        let mut reductions: Vec<NestReduction, A> = Vec::with_capacity_in(oh_pc - 1, *alloc);
        let mut red_ok = true;
        let mut is_red_update = filled(len, false, alloc);
        for opos in 0..oh_pc {
            if opos == outer_iv_pos {
                continue;
            }
            let outer_p = to_u32(oh_params.start + opos);
            let init_val = at_u32(view.extra, pre_at + 2 + opos);
            if at_bool(&in_nest, to_idx(at_u32(&inst_block, to_idx(init_val)))) {
                red_ok = false;
                break;
            }
            // Find matching inner param that receives `outer_p` from `inner_pre`.
            let Some(ipos) = inner_loop.pre_args.iter().position(|&a| a == outer_p) else {
                red_ok = false;
                break;
            };
            if ipos == inner_loop.iv_pos {
                red_ok = false;
                break;
            }
            let inner_p = to_u32(ih_params.start + ipos);
            // `outer_latch` must pass `inner_p` straight back to `outer_hdr` at `opos`!
            if at_u32(view.extra, lat_at + 2 + opos) != inner_p {
                red_ok = false;
                break;
            }
            // `inner_body` back-edge updates `inner_p` via an associative+commutative reduction op!
            let upd = at_u32(&inner_loop.back_args, ipos);
            let ui = to_idx(upd);
            if at_u32(&inst_block, ui) != inner_body || !is_assoc_comm_reduction(view.op(ui), view.flags(ui)) {
                red_ok = false;
                break;
            }
            let uat = to_idx(at_u32(view.data, ui));
            let (ua, ub) = (at_u32(view.extra, uat), at_u32(view.extra, uat + 1));
            if ua != inner_p && ub != inner_p {
                red_ok = false;
                break;
            }
            // Ensure `inner_p` is ONLY read by `upd` inside `inner_body` (and passed through `outer_latch`),
            // and `outer_p` is NOT read inside `inner_body`!
            let ib_range = view.block(Block(inner_body));
            for inst in ib_range.start..(ib_range.end - 1) {
                if inst == ui {
                    continue;
                }
                for_each_use(view, inst, |v| {
                    if v == inner_p || v == outer_p {
                        red_ok = false;
                    }
                });
            }
            if !red_ok {
                break;
            }
            set_at(&mut is_red_update, ui, true);
            reductions.push(NestReduction {
                outer_param_inst: outer_p,
                inner_param_inst: inner_p,
                init_val,
                update_inst: upd,
                ty: Ty(at_u16(view.tys, to_idx(outer_p))),
            });
        }
        if !red_ok || reductions.is_empty() {
            continue;
        }

        // Verify `inner_body` has only pure ops + non-volatile loads (no stores, calls, atomics, traps).
        let ib_range = view.block(Block(inner_body));
        let mut loads: Vec<usize, A> = Vec::new_in(*alloc);
        let mut body_ok = true;
        for inst in ib_range.start..(ib_range.end - 1) {
            let op = view.op(inst);
            if op == Op::Load {
                if view.flags(inst).contains(Flags::VOLATILE) {
                    body_ok = false;
                    break;
                }
                loads.push(inst);
            } else if !is_pure_op(op) {
                body_ok = false;
                break;
            }
            if inst != to_idx(inner_loop.iv_next) {
                for_each_use(view, inst, |v| {
                    if v == inner_loop.iv_next {
                        body_ok = false;
                    }
                });
            }
        }
        if !body_ok || loads.is_empty() {
            continue;
        }

        // Ensure no value defined inside `in_nest` escapes outside except `outer_hdr` reduction params!
        let mut escape_ok = true;
        for b in 0..blocks {
            if at_bool(&in_nest, b) {
                continue;
            }
            for inst in view.block(Block(to_u32(b))) {
                for_each_use(view, inst, |v| {
                    if at_bool(&in_nest, to_idx(at_u32(&inst_block, to_idx(v))))
                        && !reductions.iter().any(|r| r.outer_param_inst == v)
                    {
                        escape_ok = false;
                    }
                });
            }
        }
        if !escape_ok {
            continue;
        }

        // Profitability check: analyze strides of `outer_iv` (`i`) vs `inner_iv` (`j`) on every load!
        // Interchange is profitable when inner `j` has strictly larger stride than outer `i` (e.g.,
        // `j` is multiplied by `M` -> `Dynamic` or `Const > elem_bytes`, while `i` has constant stride
        // `elem_bytes`), and no load degrades.
        let mut any_improved = false;
        let mut any_regressed = false;
        for &ld in &loads {
            let ptr = at_u32(view.extra, to_idx(at_u32(view.data, ld)));
            let s_outer = analyze_iv_stride(view, ptr, outer_iv, inner_loop.iv_inst, &inst_block, &in_nest, 0);
            let s_inner = analyze_iv_stride(view, ptr, inner_loop.iv_inst, outer_iv, &inst_block, &in_nest, 0);
            match (s_outer, s_inner) {
                (Stride::Const(co), Stride::Dynamic) if co > 0 => any_improved = true,
                (Stride::Const(co), Stride::Const(ci)) if co > 0 && ci > co => any_improved = true,
                (Stride::Dynamic, Stride::Const(ci)) if ci > 0 => any_regressed = true,
                (Stride::Const(co), Stride::Const(ci)) if ci > 0 && co > ci => any_regressed = true,
                _ => {}
            }
        }
        if !any_improved || any_regressed {
            continue;
        }

        // Partition `inner_body` instructions into `j`-only hoistable invariants (computed once per
        // outer `j` row) vs inner-loop payload instructions (executed at unit stride along `i`).
        let mut is_hoisted = filled(len, false, alloc);
        let mut hoisted_j_insts: Vec<u32, A> = Vec::new_in(*alloc);
        let mut payload_insts: Vec<u32, A> = Vec::new_in(*alloc);
        for inst in ib_range.start..(ib_range.end - 1) {
            if inst == to_idx(inner_loop.iv_next) {
                continue;
            }
            let op = view.op(inst);
            let can_hoist = !at_bool(&is_red_update, inst) && is_pure_op(op) && view.flags(inst) == Flags::NONE && {
                let mut ok = true;
                for_each_use(view, inst, |v| {
                    let vi = to_idx(v);
                    let vb = at_u32(&inst_block, vi);
                    if at_bool(&in_nest, to_idx(vb)) && v != inner_loop.iv_inst && !at_bool(&is_hoisted, vi) {
                        ok = false;
                    }
                });
                ok
            };
            if can_hoist {
                set_at(&mut is_hoisted, inst, true);
                hoisted_j_insts.push(to_u32(inst));
            } else {
                payload_insts.push(to_u32(inst));
            }
        }

        return Some(InterchangePlan {
            preheader,
            outer_hdr,
            inner_pre,
            inner_hdr,
            inner_body,
            outer_latch,
            exit,
            exit_args: outer_hb.exit_args,
            iv_ty,
            is_signed: outer_hb.is_signed,
            outer_iv,
            outer_init,
            outer_bound,
            inner_iv: inner_loop.iv_inst,
            inner_init: inner_loop.iv_init,
            inner_bound: inner_loop.bound,
            inner_iv_next: inner_loop.iv_next,
            reductions,
            hoisted_j_insts,
            payload_insts,
            in_nest,
        });
    }

    None
}

/// Interchanges a strided column-major 2D rectangular reduction loop nest
/// (`for i in 0..M { for j in 0..N { acc = op(acc, mat[j * M + i]); } }`) into a unit-stride
/// row-major nest (`for j in 0..N { for i in 0..M { ... } }`), hoisting `j`-invariant row
/// calculations out of the inner loop.
///
/// When `tile_size > 0`, also strip-mines the inner `i` dimension into L1-resident tiles of width
/// `tile_size`:
/// ```text
/// for ii in 0..M step tile_size {
///     i_end = min(ii + tile_size, M)   // saturating add -> overflow-safe!
///     for j in 0..N {
///         for i in ii..i_end {
///             acc = op(acc, mat[j * M + i]);
///         }
///     }
/// }
/// ```
pub fn interchange_and_tile_loops<A: Allocator + Copy>(
    view: View<'_>,
    tile_size: u64,
    alloc: A,
) -> Result<LoopNestOutput<A>, SimplifyError> {
    let Some(plan) = find_interchange_candidate(&view, &alloc) else {
        return Ok(LoopNestOutput {
            func: view.to_func_in(alloc),
            stats: LoopNestStats::default(),
        });
    };

    let blocks = view.blocks();
    let len = view.len();
    let do_tile = tile_size > 0;
    let new_nest_blocks: u32 = if do_tile { 8 } else { 5 };
    let pre_idx = to_idx(plan.preheader);
    let insert_at = pre_idx + 1;

    let mut blk_map = filled(blocks, NONE, &alloc);
    let mut next_blk = 0u32;
    for b in 0..=pre_idx {
        if !at_bool(&plan.in_nest, b) {
            set_at(&mut blk_map, b, next_blk);
            next_blk += 1;
        }
    }
    let nest_base = next_blk;
    next_blk += new_nest_blocks;
    for b in insert_at..blocks {
        if !at_bool(&plan.in_nest, b) {
            set_at(&mut blk_map, b, next_blk);
            next_blk += 1;
        }
    }
    let exit_new = at_u32(&blk_map, to_idx(plan.exit));

    let mut func = view.to_func_in(alloc);
    clear_code_columns(&mut func);
    let mut val_map = filled(len, NONE, &alloc);
    let cmp_cond = if plan.is_signed { IntCond::Slt } else { IntCond::Ult };
    let num_red = plan.reductions.len();
    let normal_kind = block_kind_byte(BlockKind::Normal);

    let emit_hoisted_and_inner = |func: &mut Func<A>,
                                  val_map: &mut [u32],
                                  j_body_iv_start: u32,
                                  j_body_iv_end: u32,
                                  j_hdr_blk: u32,
                                  j_hdr_iv: u32,
                                  j_hdr_accs: &[u32],
                                  i_hdr_blk: u32,
                                  i_body_blk: u32,
                                  j_latch_blk: u32| {
        // `j_body`: hoist `j`-invariant row expressions (e.g., `row_base = j * M`) once per row!
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(normal_kind);
        set_at(val_map, to_idx(plan.inner_iv), j_hdr_iv);
        for &hi in &plan.hoisted_j_insts {
            let hii = to_idx(hi);
            let nv = copy_inst_remapped(&view, hii, val_map, &blk_map, func);
            set_at(val_map, hii, nv);
        }
        let mut i_entry_args: Vec<u32, A> = Vec::with_capacity_in(1 + num_red, alloc);
        i_entry_args.push(j_body_iv_start);
        for &ja in j_hdr_accs {
            i_entry_args.push(ja);
        }
        push_jump(func, i_hdr_blk, &i_entry_args);

        // `i_hdr(i, acc_i...)`: unit-stride inner loop header (`i < j_body_iv_end`).
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(normal_kind);
        let i_param = push_inst(func, Op::Param, Flags::NONE, plan.iv_ty, 0, 0);
        let mut i_accs: Vec<u32, A> = Vec::with_capacity_in(num_red, alloc);
        for red in &plan.reductions {
            let ap = push_inst(func, Op::Param, Flags::NONE, red.ty, 0, 0);
            i_accs.push(ap);
        }
        let c_i = push_icmp(func, cmp_cond, i_param, j_body_iv_end);
        push_br(func, c_i, i_body_blk, &[], j_latch_blk, &[]);

        // `i_body`: unit-stride payload (`mat[j * M + i]`) + `i_next = i + 1`.
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(normal_kind);
        set_at(val_map, to_idx(plan.outer_iv), i_param);
        for (ridx, red) in plan.reductions.iter().enumerate() {
            set_at(val_map, to_idx(red.inner_param_inst), at_u32(&i_accs, ridx));
        }
        for &pi in &plan.payload_insts {
            let pii = to_idx(pi);
            let nv = copy_inst_remapped(&view, pii, val_map, &blk_map, func);
            set_at(val_map, pii, nv);
        }
        let k1_i = push_iconst(func, plan.iv_ty, 1);
        let i_next = push_bin(func, Op::Add, Flags::NONE, plan.iv_ty, i_param, k1_i);
        let mut i_back_args: Vec<u32, A> = Vec::with_capacity_in(1 + num_red, alloc);
        i_back_args.push(i_next);
        for red in &plan.reductions {
            i_back_args.push(at_u32(val_map, to_idx(red.update_inst)));
        }
        push_jump(func, i_hdr_blk, &i_back_args);

        // `j_latch`: `j_next = j + 1`, jump back to `j_hdr(j_next, acc_i...)`.
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(normal_kind);
        let k1_j = push_iconst(func, plan.iv_ty, 1);
        let j_next = push_bin(func, Op::Add, Flags::NONE, plan.iv_ty, j_hdr_iv, k1_j);
        let mut j_back_args: Vec<u32, A> = Vec::with_capacity_in(1 + num_red, alloc);
        j_back_args.push(j_next);
        for &ia in &i_accs {
            j_back_args.push(ia);
        }
        push_jump(func, j_hdr_blk, &j_back_args);
    };

    // Emit blocks `0..=preheader`, then the new interchanged/tiled loop nest, then remaining blocks.
    for b in 0..=pre_idx {
        if at_bool(&plan.in_nest, b) {
            continue;
        }
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));
        if b32 == plan.preheader {
            let term = range.end - 1;
            for inst in range.start..term {
                let mapped = copy_inst_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            // Hoist any pure setup instructions from `inner_pre` (e.g., `j_init = iconst 0`).
            if let Some(ip) = plan.inner_pre {
                let ip_r = view.block(Block(ip));
                for inst in ip_r.start..(ip_r.end - 1) {
                    let mapped = copy_inst_remapped(&view, inst, &val_map, &blk_map, &mut func);
                    set_at(&mut val_map, inst, mapped);
                }
            }
            let outer_init_new = at_u32(&val_map, to_idx(plan.outer_init));
            let outer_bound_new = at_u32(&val_map, to_idx(plan.outer_bound));
            let inner_init_new = at_u32(&val_map, to_idx(plan.inner_init));
            let inner_bound_new = at_u32(&val_map, to_idx(plan.inner_bound));

            if !do_tile {
                // 5 blocks: `j_hdr` (0), `j_body` (1), `i_hdr` (2), `i_body` (3), `j_latch` (4).
                let j_hdr_blk = nest_base;
                let j_body_blk = nest_base + 1;
                let i_hdr_blk = nest_base + 2;
                let i_body_blk = nest_base + 3;
                let j_latch_blk = nest_base + 4;

                let mut pre_args: Vec<u32, A> = Vec::with_capacity_in(1 + num_red, alloc);
                pre_args.push(inner_init_new);
                for red in &plan.reductions {
                    pre_args.push(at_u32(&val_map, to_idx(red.init_val)));
                }
                push_jump(&mut func, j_hdr_blk, &pre_args);

                // `j_hdr(j, acc_j...)`: outer loop header (`j < N`).
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(normal_kind);
                let j_param = push_inst(&mut func, Op::Param, Flags::NONE, plan.iv_ty, 0, 0);
                let mut j_accs: Vec<u32, A> = Vec::with_capacity_in(num_red, alloc);
                for red in &plan.reductions {
                    let ap = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0, 0);
                    j_accs.push(ap);
                    set_at(&mut val_map, to_idx(red.outer_param_inst), ap);
                }
                let mut exit_args_new: Vec<u32, A> = Vec::with_capacity_in(plan.exit_args.len(), alloc);
                for &ea in &plan.exit_args {
                    exit_args_new.push(at_u32(&val_map, to_idx(ea)));
                }
                let c_j = push_icmp(&mut func, cmp_cond, j_param, inner_bound_new);
                push_br(&mut func, c_j, j_body_blk, &[], exit_new, &exit_args_new);

                emit_hoisted_and_inner(
                    &mut func,
                    &mut val_map,
                    outer_init_new,
                    outer_bound_new,
                    j_hdr_blk,
                    j_param,
                    &j_accs,
                    i_hdr_blk,
                    i_body_blk,
                    j_latch_blk,
                );
            } else {
                // 8 blocks: `tile_hdr` (0), `tile_body` (1), `j_hdr` (2), `j_body` (3),
                //           `i_hdr` (4), `i_body` (5), `j_latch` (6), `tile_latch` (7).
                let tile_hdr_blk = nest_base;
                let tile_body_blk = nest_base + 1;
                let j_hdr_blk = nest_base + 2;
                let j_body_blk = nest_base + 3;
                let i_hdr_blk = nest_base + 4;
                let i_body_blk = nest_base + 5;
                let j_latch_blk = nest_base + 6;
                let tile_latch_blk = nest_base + 7;

                let clamped_tile = match (plan.iv_ty == Ty::I32, plan.is_signed) {
                    (true, true) => tile_size.min(u64::from(i32::MAX.cast_unsigned())),
                    (true, false) => tile_size.min(u64::from(u32::MAX)),
                    (false, true) => tile_size.min(i64::MAX.cast_unsigned()),
                    (false, false) => tile_size,
                };

                let mut pre_args: Vec<u32, A> = Vec::with_capacity_in(1 + num_red, alloc);
                pre_args.push(outer_init_new);
                for red in &plan.reductions {
                    pre_args.push(at_u32(&val_map, to_idx(red.init_val)));
                }
                push_jump(&mut func, tile_hdr_blk, &pre_args);

                // `tile_hdr(ii, acc_tile...)`: `ii < M` -> `tile_body` | `exit`.
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(normal_kind);
                let ii_param = push_inst(&mut func, Op::Param, Flags::NONE, plan.iv_ty, 0, 0);
                let mut tile_accs: Vec<u32, A> = Vec::with_capacity_in(num_red, alloc);
                for red in &plan.reductions {
                    let ap = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0, 0);
                    tile_accs.push(ap);
                    set_at(&mut val_map, to_idx(red.outer_param_inst), ap);
                }
                let mut exit_args_new: Vec<u32, A> = Vec::with_capacity_in(plan.exit_args.len(), alloc);
                for &ea in &plan.exit_args {
                    exit_args_new.push(at_u32(&val_map, to_idx(ea)));
                }
                let c_tile = push_icmp(&mut func, cmp_cond, ii_param, outer_bound_new);
                push_br(&mut func, c_tile, tile_body_blk, &[], exit_new, &exit_args_new);

                // `tile_body`: `i_end = min(sat_add(ii, T), M)`, jump to `j_hdr(j_init, acc_tile...)`.
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(normal_kind);
                let k_tile = push_iconst(&mut func, plan.iv_ty, clamped_tile);
                let sat_op = if plan.is_signed { Op::SaddSat } else { Op::UaddSat };
                let min_op = if plan.is_signed { Op::Smin } else { Op::Umin };
                let ii_sat = push_bin(&mut func, sat_op, Flags::NONE, plan.iv_ty, ii_param, k_tile);
                let i_end = push_bin(&mut func, min_op, Flags::NONE, plan.iv_ty, ii_sat, outer_bound_new);
                let mut j_entry_args: Vec<u32, A> = Vec::with_capacity_in(1 + num_red, alloc);
                j_entry_args.push(inner_init_new);
                for &ta in &tile_accs {
                    j_entry_args.push(ta);
                }
                push_jump(&mut func, j_hdr_blk, &j_entry_args);

                // `j_hdr(j, acc_j...)`: `j < N` -> `j_body` | `tile_latch`.
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(normal_kind);
                let j_param = push_inst(&mut func, Op::Param, Flags::NONE, plan.iv_ty, 0, 0);
                let mut j_accs: Vec<u32, A> = Vec::with_capacity_in(num_red, alloc);
                for red in &plan.reductions {
                    let ap = push_inst(&mut func, Op::Param, Flags::NONE, red.ty, 0, 0);
                    j_accs.push(ap);
                }
                let c_j = push_icmp(&mut func, cmp_cond, j_param, inner_bound_new);
                push_br(&mut func, c_j, j_body_blk, &[], tile_latch_blk, &[]);

                emit_hoisted_and_inner(
                    &mut func,
                    &mut val_map,
                    ii_param,
                    i_end,
                    j_hdr_blk,
                    j_param,
                    &j_accs,
                    i_hdr_blk,
                    i_body_blk,
                    j_latch_blk,
                );

                // `tile_latch`: advance `ii -> i_end` (guaranteed strictly increasing & overflow-free).
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(normal_kind);
                let mut tile_back_args: Vec<u32, A> = Vec::with_capacity_in(1 + num_red, alloc);
                tile_back_args.push(i_end);
                for &ja in &j_accs {
                    tile_back_args.push(ja);
                }
                push_jump(&mut func, tile_hdr_blk, &tile_back_args);
            }
        } else {
            for inst in range {
                let mapped = copy_inst_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    // Emit remaining non-nest blocks (`exit` and anything after).
    for b in insert_at..blocks {
        if at_bool(&plan.in_nest, b) {
            continue;
        }
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(to_u32(b))) {
            let mapped = copy_inst_remapped(&view, inst, &val_map, &blk_map, &mut func);
            set_at(&mut val_map, inst, mapped);
        }
    }

    let _ = (plan.outer_hdr, plan.inner_hdr, plan.inner_body, plan.outer_latch, plan.inner_iv_next);
    verify(func.view()).map_err(SimplifyError::Verify)?;
    let cleaned = dead(func.view(), alloc)?;
    Ok(LoopNestOutput {
        func: cleaned,
        stats: LoopNestStats {
            loops_fused: 0,
            loops_interchanged: 1,
            loops_tiled: u32::from(do_tile),
        },
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Bodies, BodyId, Outcome, Val},
        func::{Conv, RefId, RefKind, SlotKind},
    };

    const FUEL: u64 = 500_000;
    const CALLER_ID: BodyId = BodyId([1; 32]);
    const CALLEE_ID: BodyId = BodyId([2; 32]);

    struct SingleCallee<'a> {
        callee: View<'a>,
    }

    impl Bodies for SingleCallee<'_> {
        fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
            match (from, reference) {
                (CALLER_ID, RefId(0)) => Some((CALLEE_ID, self.callee)),
                _ => None,
            }
        }
    }

    #[test]
    fn fuse_adjacent_loops_eliminates_tmp_reads_and_matches_eval() {
        // `fn kernel(a: *const u64, tmp: *mut u64, n: u64) -> u64`:
        //   for i in 0..n { tmp[i] = a[i] + 1; }
        //   let mut sum = 0;
        //   for j in 0..n { sum += tmp[j] * 3; }
        //   sum
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, h1, b1, mid, h2, b2, exit] = [(); 7].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let a = b.param(Ty::PTR);
        let tmp = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let z0 = b.iconst(Ty::I64, 0);
        b.jump((h1, &[z0]));

        b.begin(h1);
        let i = b.param(Ty::I64);
        let c1 = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, c1, (b1, &[]), (mid, &[]));

        b.begin(b1);
        let k3a = b.iconst(Ty::I64, 3);
        let off1 = b.binary(Op::Shl, Flags::NONE, i, k3a);
        let ap = b.ptr_add(Flags::NONE, a, off1);
        let tp1 = b.ptr_add(Flags::NONE, tmp, off1);
        let av = b.load(Ty::I64, Flags::NONE, ap, 0, MemAttrs::aligned(3));
        let one1 = b.iconst(Ty::I64, 1);
        let v = b.binary(Op::Add, Flags::NONE, av, one1);
        b.store(Flags::NONE, v, tp1, 0, MemAttrs::aligned(3));
        let i2 = b.binary(Op::Add, Flags::NONE, i, one1);
        b.jump((h1, &[i2]));

        b.begin(mid);
        let z2 = b.iconst(Ty::I64, 0);
        b.jump((h2, &[z2, z2]));

        b.begin(h2);
        let j = b.param(Ty::I64);
        let sum = b.param(Ty::I64);
        let c2 = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, c2, (b2, &[]), (exit, &[]));

        b.begin(b2);
        let k3b = b.iconst(Ty::I64, 3);
        let off2 = b.binary(Op::Shl, Flags::NONE, j, k3b);
        let tp2 = b.ptr_add(Flags::NONE, tmp, off2);
        let tv = b.load(Ty::I64, Flags::NONE, tp2, 0, MemAttrs::aligned(3));
        let mul3 = b.iconst(Ty::I64, 3);
        let prod = b.binary(Op::Mul, Flags::NONE, tv, mul3);
        let sum2 = b.binary(Op::Add, Flags::NONE, sum, prod);
        let one2 = b.iconst(Ty::I64, 1);
        let j2 = b.binary(Op::Add, Flags::NONE, j, one2);
        b.jump((h2, &[j2, sum2]));

        b.begin(exit);
        b.ret(&[sum]);
        let orig = b.finish_checked().unwrap();

        let out = fuse_adjacent_loops(orig.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_fused, 1, "must fuse adjacent [0, n) loops");

        // Verify: only 1 loop remains AND 0 loads from `tmp`!
        let fv = out.func.view();
        let tmp_param = (fv.params(Block(0)).start + 1) as u32;
        let mut load_count = 0usize;
        for inst in 0..fv.len() {
            if fv.op(inst) == Op::Load {
                load_count += 1;
                let ptr = fv.extra[fv.data[inst] as usize] as usize;
                if fv.op(ptr) == Op::PtrAdd {
                    let base = fv.extra[fv.data[ptr] as usize];
                    assert_ne!(base, tmp_param, "fused loop must have 0 reads from `tmp`");
                }
            }
        }
        assert_eq!(load_count, 1, "only the load from `a[i]` should remain after forwarding `tmp[i]`");

        // Build a harness `caller(n)` allocating real stack slots for `a` and `tmp`, initializing `a`,
        // calling `kernel(a, tmp, n)`, and returning `sum + tmp[0]` (when `n > 0`) to check BOTH
        // the returned reduction AND the preserved contents of `tmp`!
        let mut hb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let h_sig = hb.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let k_sig = hb.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        hb.set_signature(h_sig);
        let k_ref = hb.reference(RefKind::Func, 0);
        let s_a = hb.slot(128, 3, SlotKind::Named);
        let s_tmp = hb.slot(128, 3, SlotKind::Named);
        let [he, init_h, init_b, run_b, check_b, done_b] = [(); 6].map(|_| hb.create_block(BlockKind::Normal));
        hb.begin(he);
        let hn = hb.param(Ty::I64);
        let pa = hb.slot_addr(s_a);
        let ptmp = hb.slot_addr(s_tmp);
        let hz = hb.iconst(Ty::I64, 0);
        hb.jump((init_h, &[hz]));
        hb.begin(init_h);
        let hi = hb.param(Ty::I64);
        let hc = hb.icmp(IntCond::Ult, hi, hn);
        hb.br(Flags::NONE, hc, (init_b, &[]), (run_b, &[]));
        hb.begin(init_b);
        let k7 = hb.iconst(Ty::I64, 7);
        let k5 = hb.iconst(Ty::I64, 5);
        let v1 = hb.binary(Op::Mul, Flags::NONE, hi, k7);
        let val = hb.binary(Op::Add, Flags::NONE, v1, k5);
        let sh3 = hb.iconst(Ty::I64, 3);
        let hoff = hb.binary(Op::Shl, Flags::NONE, hi, sh3);
        let hap = hb.ptr_add(Flags::NONE, pa, hoff);
        hb.store(Flags::NONE, val, hap, 0, MemAttrs::aligned(3));
        let hone = hb.iconst(Ty::I64, 1);
        let hi2 = hb.binary(Op::Add, Flags::NONE, hi, hone);
        hb.jump((init_h, &[hi2]));
        hb.begin(run_b);
        let kres = hb.call(Flags::NONE, k_ref, k_sig, &[pa, ptmp, hn]);
        let has_elem = hb.icmp(IntCond::Ugt, hn, hz);
        hb.br(Flags::NONE, has_elem, (check_b, &[]), (done_b, &[kres]));
        hb.begin(check_b);
        let t0 = hb.load(Ty::I64, Flags::NONE, ptmp, 0, MemAttrs::aligned(3));
        let comb = hb.binary(Op::Add, Flags::NONE, kres, t0);
        hb.jump((done_b, &[comb]));
        hb.begin(done_b);
        let final_v = hb.param(Ty::I64);
        hb.ret(&[final_v]);
        let harness = hb.finish_checked().unwrap();

        for &nv in &[0u128, 1, 2, 5, 12] {
            let r_orig = eval::run_calls_in(
                harness.view(),
                CALLER_ID,
                &[Val::Int(nv)],
                FUEL,
                &SingleCallee { callee: orig.view() },
                TestHeap,
            )
            .unwrap();
            let r_fused = eval::run_calls_in(
                harness.view(),
                CALLER_ID,
                &[Val::Int(nv)],
                FUEL,
                &SingleCallee { callee: out.func.view() },
                TestHeap,
            )
            .unwrap();
            assert_eq!(r_orig, r_fused, "fusion mismatch for n={nv}");
        }
    }

    #[test]
    fn interchange_and_tile_converts_column_major_to_unit_stride_and_matches_eval() {
        // `fn col_sum(mat: *const u64, m: u64, n: u64) -> u64`:
        //   let mut acc = 0;
        //   for i in 0..m {
        //       for j in 0..n {
        //           acc += mat[(j * m + i) * 8];
        //       }
        //   }
        //   acc
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, o_hdr, i_pre, i_hdr, i_body, o_latch, exit] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let mat = b.param(Ty::PTR);
        let m = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((o_hdr, &[z, z]));

        b.begin(o_hdr);
        let i = b.param(Ty::I64);
        let acc_o = b.param(Ty::I64);
        let co = b.icmp(IntCond::Ult, i, m);
        b.br(Flags::NONE, co, (i_pre, &[]), (exit, &[]));

        b.begin(i_pre);
        let j0 = b.iconst(Ty::I64, 0);
        b.jump((i_hdr, &[j0, acc_o]));

        b.begin(i_hdr);
        let j = b.param(Ty::I64);
        let acc_i = b.param(Ty::I64);
        let ci = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, ci, (i_body, &[]), (o_latch, &[]));

        b.begin(i_body);
        let jm = b.binary(Op::Mul, Flags::NONE, j, m);
        let idx = b.binary(Op::Add, Flags::NONE, jm, i);
        let sh3 = b.iconst(Ty::I64, 3);
        let off = b.binary(Op::Shl, Flags::NONE, idx, sh3);
        let ep = b.ptr_add(Flags::NONE, mat, off);
        let val = b.load(Ty::I64, Flags::NONE, ep, 0, MemAttrs::aligned(3));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc_i, val);
        let one_j = b.iconst(Ty::I64, 1);
        let j2 = b.binary(Op::Add, Flags::NONE, j, one_j);
        b.jump((i_hdr, &[j2, acc2]));

        b.begin(o_latch);
        let one_i = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one_i);
        b.jump((o_hdr, &[i2, acc_i]));

        b.begin(exit);
        b.ret(&[acc_o]);
        let orig = b.finish_checked().unwrap();

        // 1. Untiled interchange (`tile_size = 0`): inner loop IV must now have unit stride (`8` bytes = 1 u64)!
        let inter = interchange_and_tile_loops(orig.view(), 0, TestHeap).unwrap();
        assert_eq!(inter.stats.loops_interchanged, 1);
        assert_eq!(inter.stats.loops_tiled, 0);

        let iv = inter.func.view();
        let (_, ib_map) = build_cfg_info(&iv, &TestHeap);
        let ld_inst = (0..iv.len()).find(|&k| iv.op(k) == Op::Load).expect("must contain Load");
        let ld_blk = ib_map[ld_inst];
        // The inner header immediately precedes `ld_blk` (`i_hdr` -> `i_body`).
        let inner_hdr_blk = ld_blk - 1;
        let inner_iv = iv.params(Block(inner_hdr_blk)).start as u32;
        let mut in_inner = filled(iv.blocks(), false, &TestHeap);
        in_inner[inner_hdr_blk as usize] = true;
        in_inner[ld_blk as usize] = true;
        let ld_ptr = iv.extra[iv.data[ld_inst] as usize];
        let inner_stride = analyze_iv_stride(&iv, ld_ptr, inner_iv, NONE, &ib_map, &in_inner, 0);
        assert_eq!(
            inner_stride,
            Stride::Const(8),
            "interchanged inner loop IV must have unit element stride (8 bytes per u64)"
        );

        // 2. Interchange + L1 cache tiling (`tile_size = 4`)!
        let tiled = interchange_and_tile_loops(orig.view(), 4, TestHeap).unwrap();
        assert_eq!(tiled.stats.loops_interchanged, 1);
        assert_eq!(tiled.stats.loops_tiled, 1);

        // Build harness allocating an `8 * 64 = 512`-byte matrix filled with `k * 13 + 1`, calling `col_sum(mat, m, n)`.
        let mut hb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let h_sig = hb.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        let k_sig = hb.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        hb.set_signature(h_sig);
        let k_ref = hb.reference(RefKind::Func, 0);
        let s_mat = hb.slot(512, 3, SlotKind::Named);
        let [he, ih, ib, run_b] = [(); 4].map(|_| hb.create_block(BlockKind::Normal));
        hb.begin(he);
        let hm = hb.param(Ty::I64);
        let hn = hb.param(Ty::I64);
        let total = hb.binary(Op::Mul, Flags::NONE, hm, hn);
        let pmat = hb.slot_addr(s_mat);
        let hz = hb.iconst(Ty::I64, 0);
        hb.jump((ih, &[hz]));
        hb.begin(ih);
        let hk = hb.param(Ty::I64);
        let hc = hb.icmp(IntCond::Ult, hk, total);
        hb.br(Flags::NONE, hc, (ib, &[]), (run_b, &[]));
        hb.begin(ib);
        let k13 = hb.iconst(Ty::I64, 13);
        let k1 = hb.iconst(Ty::I64, 1);
        let prod = hb.binary(Op::Mul, Flags::NONE, hk, k13);
        let v = hb.binary(Op::Add, Flags::NONE, prod, k1);
        let sh3 = hb.iconst(Ty::I64, 3);
        let off = hb.binary(Op::Shl, Flags::NONE, hk, sh3);
        let ep = hb.ptr_add(Flags::NONE, pmat, off);
        hb.store(Flags::NONE, v, ep, 0, MemAttrs::aligned(3));
        let hk2 = hb.binary(Op::Add, Flags::NONE, hk, k1);
        hb.jump((ih, &[hk2]));
        hb.begin(run_b);
        let res = hb.call(Flags::NONE, k_ref, k_sig, &[pmat, hm, hn]);
        hb.ret(&[res]);
        let harness = hb.finish_checked().unwrap();

        for &(mv, nv) in &[(0u128, 0u128), (0, 5), (5, 0), (1, 1), (3, 5), (4, 4), (7, 6), (9, 5)] {
            let Outcome::Returned(r_orig) = eval::run_calls_in(
                harness.view(),
                CALLER_ID,
                &[Val::Int(mv), Val::Int(nv)],
                FUEL,
                &SingleCallee { callee: orig.view() },
                TestHeap,
            )
            .unwrap() else {
                panic!("orig eval failed");
            };
            let Outcome::Returned(r_inter) = eval::run_calls_in(
                harness.view(),
                CALLER_ID,
                &[Val::Int(mv), Val::Int(nv)],
                FUEL,
                &SingleCallee { callee: inter.func.view() },
                TestHeap,
            )
            .unwrap() else {
                panic!("interchanged eval failed");
            };
            let Outcome::Returned(r_tiled) = eval::run_calls_in(
                harness.view(),
                CALLER_ID,
                &[Val::Int(mv), Val::Int(nv)],
                FUEL,
                &SingleCallee { callee: tiled.func.view() },
                TestHeap,
            )
            .unwrap() else {
                panic!("tiled eval failed");
            };
            assert_eq!(r_orig, r_inter, "interchange mismatch for m={mv}, n={nv}");
            assert_eq!(r_orig, r_tiled, "tiled mismatch for m={mv}, n={nv}");
        }
    }
}
