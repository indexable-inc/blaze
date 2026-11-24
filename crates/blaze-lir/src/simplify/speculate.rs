//! Live Speculative JIT Transforms:
//! - **Guarded Runtime Value Specialization** ([`specialize_guarded_value`]): emits an entry
//!   guard `param[param_idx] == hot_value` branching between a specialized fast clone (with
//!   `param[param_idx]` replaced by `Iconst(hot_value)` so subsequent `propagate` + `egraph`
//!   strength-reduce divisions to magic multiplies or dynamic `stride == 1` to SIMD) and the
//!   original generic slow clone.
//! - **Speculative Runtime Alias Guarding** ([`guard_alias_for_simd`]): emits a 4-instruction
//!   unsigned wraparound non-overlap check `((dst - src) >= bytes) & ((src - dst) >= bytes)` at
//!   entry, splicing an auto-vectorized 4x-unrolled SIMD fast path on the disjoint arm and the
//!   exact scalar loop on the overlapping fallback arm.
//! - **On-Stack Replacement (OSR) Entrypoint Synthesis** ([`synthesize_osr_entry`]): builds a
//!   standalone callable LIR function rooted at a hot `loop_header`, taking its loop-carried
//!   parameters plus all outside SSA values live across the loop subgraph as entry arguments.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
    vectorize::auto_vectorize_loops,
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Ty, Value, View,
    func::{Ext, ParamKind, ParamRec, SigId, SigRec},
    op::{Field, Layout},
    verify,
};

/// Runtime value specialization hint for an entry parameter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ValueSpecHint {
    /// Zero-based index into the entry block's `Op::Param` list.
    pub param_idx: usize,
    /// Hot runtime value observed for `param[param_idx]`.
    pub hot_value: u64,
}

/// Telemetry returned by speculative JIT transforms.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SpeculateStats {
    /// Number of guarded value-specialization checks inserted.
    pub guards_inserted: u32,
    /// Number of runtime alias non-overlap guards inserted.
    pub alias_guards_inserted: u32,
    /// Number of standalone OSR entrypoints synthesized.
    pub osr_entries_built: u32,
}

/// Output of [`specialize_guarded_value`] and [`guard_alias_for_simd`].
#[derive(Debug)]
pub struct SpeculateOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: SpeculateStats,
}

/// Output of [`synthesize_osr_entry`]: a standalone OSR-callable function and its live-in ABI.
#[derive(Debug)]
pub struct OsrPlan<A: Allocator> {
    /// Standalone OSR function whose entry block jumps directly into `osr_header`.
    pub func: Func<A>,
    /// Block index of the remapped `loop_header` inside `func` (`1`).
    pub osr_header: u32,
    /// Original instruction indices in the source function corresponding 1-to-1 with `func`'s
    /// entry parameters: `loop_header`'s own `Op::Param`s first, followed by sorted outside
    /// SSA values read anywhere reachable from `loop_header`.
    pub live_in_insts: Vec<u32, A>,
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn trunc_u32(v: u64) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn at<T: Copy + Default>(s: &[T], i: usize) -> T {
    s.get(i).copied().unwrap_or_default()
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&c| c == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
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
const fn block_kind_byte(k: BlockKind) -> u8 {
    match k {
        BlockKind::Normal => 0,
        BlockKind::Cold => 1,
        BlockKind::Cleanup => 2,
        BlockKind::Catch => 3,
        BlockKind::Terminate => 4,
    }
}

#[inline]
const fn ext_byte(e: Ext) -> u8 {
    match e {
        Ext::None => 0,
        Ext::Zext => 1,
        Ext::Sext => 2,
    }
}

#[inline]
const fn param_kind_byte(k: ParamKind) -> u8 {
    match k {
        ParamKind::Value => 0,
        ParamKind::Sret => 1,
        ParamKind::Byval => 2,
    }
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
        let idx = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, idx, 0)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, idx, 0)
}

fn push_icmp<A: Allocator>(func: &mut Func<A>, cond: IntCond, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Icmp, Flags::NONE, Ty::I1, idx, 0)
}

fn push_jump<A: Allocator>(func: &mut Func<A>, target: u32, args: &[u32]) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len()));
    for &a in args {
        func.extra.push(a);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, idx, 0)
}

fn push_br<A: Allocator>(
    func: &mut Func<A>,
    flags: Flags,
    cond: u32,
    t_blk: u32,
    t_args: &[u32],
    e_blk: u32,
    e_args: &[u32],
) -> u32 {
    let idx = to_u32(func.extra.len());
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
    push_inst(func, Op::Br, flags, Ty::NONE, idx, 0)
}

fn copy_inst_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    let op = view.op(inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attr = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at(val_map, to_idx(at(view.data, inst))),
        Layout::Data(Field::Block) => at(blk_map, to_idx(at(view.data, inst))),
        Layout::Data(_) => at(view.data, inst),
        Layout::Extra(fields) => {
            let src = to_idx(at(view.data, inst));
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        func.extra.push(at(val_map, to_idx(at(view.extra, p))));
                        p += 1;
                    }
                    Field::Block => {
                        func.extra.push(at(blk_map, to_idx(at(view.extra, p))));
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(blk_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Target => {
                        let b = to_idx(at(view.extra, p));
                        let n_u32 = at(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(at(blk_map, b));
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let scrutinee = at(view.extra, src);
                        let w = to_idx(Ty(at(view.tys, to_idx(scrutinee))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at(view.extra, p + k));
                            }
                            p += w;
                            let b = to_idx(at(view.extra, p));
                            let an_u32 = at(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(at(blk_map, b));
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at(view.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        func.extra.push(at(view.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = to_u32(func.ops.len());
    func.ops.push(at(view.ops, inst));
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

/// Clears the 9 instruction and block columns of a cloned `Func` while preserving all metadata
/// tables (`sigs`, `slots`, `mem_attrs`, `refs`, `datas`, `compounds`, `spans`, etc.).
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

/// Appends a complete body clone of `src` into `func`, skipping `src`'s entry `Op::Param`s and
/// substituting them with `entry_param_subst`. All other instructions map 1-to-1 with exact
/// precomputed indices, so non-RPO block orders remain valid.
fn splice_body_clone<A: Allocator + Copy>(
    src: &View<'_>,
    entry_param_subst: &[u32],
    blk_offset: u32,
    func: &mut Func<A>,
    alloc: &A,
) {
    let blocks = src.blocks();
    let len = src.len();
    let params = src.params(Block(0));
    let param_count = params.len();

    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
    for b in 0..blocks {
        blk_map.push(blk_offset + to_u32(b));
    }

    let base_idx = to_u32(func.ops.len());
    let mut val_map = filled(len, NONE, alloc);
    for (p, &sub) in entry_param_subst.iter().enumerate() {
        set_at(&mut val_map, params.start + p, sub);
    }
    for inst in params.end..len {
        set_at(&mut val_map, inst, base_idx + to_u32(inst - param_count));
    }

    for b in 0..blocks {
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(src.block_kinds, b));
        let range = src.block(Block(to_u32(b)));
        let start = if b == 0 { params.end } else { range.start };
        for inst in start..range.end {
            let emitted = copy_inst_remapped(src, inst, &val_map, &blk_map, func);
            debug_assert_eq!(emitted, at(&val_map, inst));
        }
    }
}

/// **Guarded Runtime Value Specialization**:
///
/// Inserts an entry guard `param[hint.param_idx] == hint.hot_value` branching between:
/// - **Fast clone**: identical body with every use of `param[hint.param_idx]` replaced by
///   `Iconst(hint.hot_value)` (unlocking constant folding, magic-multiply division reduction,
///   and unit-stride SIMD vectorization).
/// - **Slow clone**: original generic body using the runtime parameter untouched.
///
/// If `hint.param_idx` is out of range or not a scalar integer (`<= 64` bits), returns the
/// original function unchanged with `guards_inserted = 0`.
pub fn specialize_guarded_value<A: Allocator + Copy>(
    view: View<'_>,
    hint: ValueSpecHint,
    alloc: A,
) -> Result<SpeculateOutput<A>, SimplifyError> {
    let entry_params = view.params(Block(0));
    let param_count = entry_params.len();
    if hint.param_idx >= param_count {
        return Ok(SpeculateOutput {
            func: view.to_func_in(alloc),
            stats: SpeculateStats::default(),
        });
    }
    let target_param = entry_params.start + hint.param_idx;
    let param_ty = view.ty(Value(to_u32(target_param)));
    let bits = param_ty.bits(view.head.ptr_bits);
    if !param_ty.is_scalar() || !param_ty.is_int() || bits > 64 {
        return Ok(SpeculateOutput {
            func: view.to_func_in(alloc),
            stats: SpeculateStats::default(),
        });
    }

    let blocks = view.blocks();
    let fast_entry = 1u32;
    let slow_entry = 1u32 + to_u32(blocks);

    let mut func = view.to_func_in(alloc);
    clear_code_columns(&mut func);

    // Block 0: entry guard block.
    func.block_starts.push(0);
    func.block_kinds.push(block_kind_byte(BlockKind::Normal));
    let mut new_params: Vec<u32, A> = Vec::with_capacity_in(param_count, alloc);
    for p in entry_params.start..entry_params.end {
        let np = push_inst(&mut func, Op::Param, Flags::NONE, Ty(at(view.tys, p)), 0, 0);
        new_params.push(np);
    }
    let k_hot = push_iconst(&mut func, param_ty, hint.hot_value);
    let is_hot = push_icmp(&mut func, IntCond::Eq, at(&new_params, hint.param_idx), k_hot);
    push_br(&mut func, Flags::NONE, is_hot, fast_entry, &[], slow_entry, &[]);

    // Blocks 1..1+B: fast specialized clone (`param[param_idx] -> k_hot`).
    let mut fast_subst: Vec<u32, A> = Vec::with_capacity_in(param_count, alloc);
    for (idx, &np) in new_params.iter().enumerate() {
        fast_subst.push(if idx == hint.param_idx { k_hot } else { np });
    }
    splice_body_clone(&view, &fast_subst, fast_entry, &mut func, &alloc);

    // Blocks 1+B..1+2*B: generic slow fallback clone.
    splice_body_clone(&view, &new_params, slow_entry, &mut func, &alloc);

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(SpeculateOutput {
        func,
        stats: SpeculateStats {
            guards_inserted: 1,
            alias_guards_inserted: 0,
            osr_entries_built: 0,
        },
    })
}

/// **Speculative Runtime Alias Guarding (unlocking SIMD on ambiguous-alias pointer loops)**:
///
/// Emits an O(1) 4-instruction unsigned wraparound non-overlap check at entry:
/// ```text
/// d_addr     = ptr_addr(dst)
/// s_addr     = ptr_addr(src)
/// bytes      = len * elem_bytes          (or `len << log2(elem_bytes)` for power-of-2)
/// diff1      = d_addr - s_addr
/// diff2      = s_addr - d_addr
/// no_overlap = (diff1 >= bytes) & (diff2 >= bytes)
/// ```
/// Over modulo-2^W arithmetic, `(d - s) >= bytes && (s - d) >= bytes` holds if and only if
/// `[dst, dst + bytes)` and `[src, src + bytes)` are completely disjoint (or `bytes == 0`):
/// overlapping ranges (including `dst == src` and partial overlaps `dst = src + 4`) fail the
/// check and execute the untouched scalar fallback arm, while disjoint ranges branch into the
/// auto-vectorized fast arm.
pub fn guard_alias_for_simd<A: Allocator + Copy>(
    view: View<'_>,
    dst_param: usize,
    src_param: usize,
    len_param: usize,
    elem_bytes: u64,
    alloc: A,
) -> Result<SpeculateOutput<A>, SimplifyError> {
    let entry_params = view.params(Block(0));
    let param_count = entry_params.len();
    if elem_bytes == 0
        || dst_param >= param_count
        || src_param >= param_count
        || len_param >= param_count
        || dst_param == src_param
    {
        return Ok(SpeculateOutput {
            func: view.to_func_in(alloc),
            stats: SpeculateStats::default(),
        });
    }

    let ptr_bits = view.head.ptr_bits;
    let ptr_int = Ty::int(ptr_bits).unwrap_or(Ty::I64);
    let dst_ty = view.ty(Value(to_u32(entry_params.start + dst_param)));
    let src_ty = view.ty(Value(to_u32(entry_params.start + src_param)));
    let len_ty = view.ty(Value(to_u32(entry_params.start + len_param)));
    if dst_ty != Ty::PTR
        || src_ty != Ty::PTR
        || !len_ty.is_scalar()
        || !len_ty.is_int()
        || len_ty.bits(ptr_bits) > ptr_bits
    {
        return Ok(SpeculateOutput {
            func: view.to_func_in(alloc),
            stats: SpeculateStats::default(),
        });
    }

    // Build the vectorized fast-path candidate (shares identical metadata tables with `view`).
    let fast_vec = auto_vectorize_loops(view, alloc)?;
    let fast_view = fast_vec.func.view();
    let fast_entry = 1u32;
    let slow_entry = 1u32 + to_u32(fast_view.blocks());

    let mut func = view.to_func_in(alloc);
    clear_code_columns(&mut func);

    // Block 0: O(1) runtime non-overlap alias guard.
    func.block_starts.push(0);
    func.block_kinds.push(block_kind_byte(BlockKind::Normal));
    let mut new_params: Vec<u32, A> = Vec::with_capacity_in(param_count, alloc);
    for p in entry_params.start..entry_params.end {
        let np = push_inst(&mut func, Op::Param, Flags::NONE, Ty(at(view.tys, p)), 0, 0);
        new_params.push(np);
    }

    let d_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, ptr_int, at(&new_params, dst_param), 0);
    let s_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, ptr_int, at(&new_params, src_param), 0);
    let len_w = if len_ty == ptr_int {
        at(&new_params, len_param)
    } else {
        push_inst(&mut func, Op::Zext, Flags::NONE, ptr_int, at(&new_params, len_param), 0)
    };
    let bytes = if elem_bytes == 1 {
        len_w
    } else if elem_bytes.is_power_of_two() {
        let k_shift = push_iconst(&mut func, ptr_int, u64::from(elem_bytes.trailing_zeros()));
        push_bin(&mut func, Op::Shl, Flags::NONE, ptr_int, len_w, k_shift)
    } else {
        let k_eb = push_iconst(&mut func, ptr_int, elem_bytes);
        push_bin(&mut func, Op::Mul, Flags::NONE, ptr_int, len_w, k_eb)
    };
    let diff1 = push_bin(&mut func, Op::Sub, Flags::NONE, ptr_int, d_addr, s_addr);
    let diff2 = push_bin(&mut func, Op::Sub, Flags::NONE, ptr_int, s_addr, d_addr);
    let ok1 = push_icmp(&mut func, IntCond::Uge, diff1, bytes);
    let ok2 = push_icmp(&mut func, IntCond::Uge, diff2, bytes);
    let no_overlap = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, ok1, ok2);
    push_br(&mut func, Flags::NONE, no_overlap, fast_entry, &[], slow_entry, &[]);

    // Blocks 1..1+fast_blocks: vectorized fast body.
    splice_body_clone(&fast_view, &new_params, fast_entry, &mut func, &alloc);

    // Blocks 1+fast_blocks..: untouched scalar fallback body.
    splice_body_clone(&view, &new_params, slow_entry, &mut func, &alloc);

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(SpeculateOutput {
        func,
        stats: SpeculateStats {
            guards_inserted: 0,
            alias_guards_inserted: 1,
            osr_entries_built: 0,
        },
    })
}

/// **On-Stack Replacement (OSR) Entrypoint Synthesis**:
///
/// Builds a standalone OSR-callable `Func<A>` that begins execution directly at `loop_header`:
/// - Entry parameters of the synthesized function are `live_in_insts`:
///   1. `loop_header`'s own `Op::Param`s in parameter order (loop-carried state at OSR point),
///   2. all SSA values defined outside the subgraph reachable from `loop_header` that are read
///      anywhere inside that subgraph (sorted ascending by original instruction index).
/// - Block `0` jumps straight to `osr_header` (`block 1`) passing `loop_header`'s parameters.
/// - Every block reachable from `loop_header` is cloned and remapped with outside reads bound
///   to the OSR entry parameters.
pub fn synthesize_osr_entry<A: Allocator + Copy>(
    view: View<'_>,
    loop_header: Block,
    alloc: A,
) -> Result<OsrPlan<A>, SimplifyError> {
    let blocks = view.blocks();
    let h = loop_header.index();
    if h == 0 || h >= blocks || view.block_kind(loop_header).is_landing() {
        return Err(SimplifyError::Refused {
            inst: 0,
            what: "OSR loop header must be a non-entry, non-landing block",
        });
    }

    // 1. Collect all blocks reachable from `loop_header`.
    let mut reached = filled(blocks, false, &alloc);
    let mut work: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    set_at(&mut reached, h, true);
    work.push(to_u32(h));
    while let Some(b) = work.pop() {
        let term = view.terminator(Block(b));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if !at(&reached, tb) {
                    set_at(&mut reached, tb, true);
                    work.push(edge.block);
                }
            }
        });
    }

    // Deterministic block order: `loop_header` first (`block 1`), then other reachable blocks ascending.
    let mut kept_blocks: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    kept_blocks.push(to_u32(h));
    for b in 0..blocks {
        if at(&reached, b) && b != h {
            kept_blocks.push(to_u32(b));
        }
    }

    let mut blk_map = filled(blocks, NONE, &alloc);
    for (idx, &old_b) in kept_blocks.iter().enumerate() {
        set_at(&mut blk_map, to_idx(old_b), to_u32(idx + 1));
    }

    // 2. Map every instruction to its defining block.
    let len = view.len();
    let mut inst_block = filled(len, 0u32, &alloc);
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
    }

    // 3. Collect all values live at `loop_header` entry:
    //    `loop_header`'s own `Op::Param`s + all outside SSA values read in `kept_blocks`
    //    (including target edge arguments!).
    let h_params = view.params(loop_header);
    let header_param_count = h_params.len();
    let mut live_in_insts: Vec<u32, A> = Vec::new_in(alloc);
    for p in h_params.start..h_params.end {
        live_in_insts.push(to_u32(p));
    }

    let mut outside_seen = filled(len, false, &alloc);
    let mut outside_vals: Vec<u32, A> = Vec::new_in(alloc);
    for &old_b in &kept_blocks {
        for inst in view.block(Block(old_b)) {
            each(&view, inst, |item| match item {
                Item::Value(v) => {
                    let vi = to_idx(v);
                    if !at(&reached, to_idx(at(&inst_block, vi))) && !at(&outside_seen, vi) {
                        set_at(&mut outside_seen, vi, true);
                        outside_vals.push(v);
                    }
                }
                Item::Edge(edge) => {
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            let v = at(view.extra, start + k);
                            let vi = to_idx(v);
                            if !at(&reached, to_idx(at(&inst_block, vi))) && !at(&outside_seen, vi) {
                                set_at(&mut outside_seen, vi, true);
                                outside_vals.push(v);
                            }
                        }
                    }
                }
            });
        }
    }
    outside_vals.sort_unstable();
    for &ov in &outside_vals {
        live_in_insts.push(ov);
    }

    let Ok(params_u16) = u16::try_from(live_in_insts.len()) else {
        return Err(SimplifyError::Refused {
            inst: 0,
            what: "too many OSR live-in values for signature parameter count",
        });
    };

    // 4. Construct the standalone OSR function with its new entry signature.
    let mut func = view.to_func_in(alloc);
    clear_code_columns(&mut func);

    let orig_sig = view.sig(SigId(view.head.sig));
    let new_sig_idx = to_u32(func.sigs.len());
    let sig_first = to_u32(func.sig_params.len());
    for &lv in &live_in_insts {
        func.sig_params.push(ParamRec {
            facts: 0,
            ty: at(view.tys, to_idx(lv)),
            ext: ext_byte(Ext::None),
            kind: param_kind_byte(ParamKind::Value),
        });
    }
    for &ret in orig_sig.rets {
        func.sig_params.push(ret);
    }
    func.sigs.push(SigRec {
        first: sig_first,
        params: params_u16,
        rets: u16::try_from(orig_sig.rets.len()).unwrap_or(0),
        conv: orig_sig.conv,
        bits: orig_sig.bits,
        pad: [0; 2],
    });
    func.head.sig = new_sig_idx;

    // Block 0: OSR trampoline entry jumping straight into `osr_header = 1`.
    let osr_header = 1u32;
    func.block_starts.push(0);
    func.block_kinds.push(block_kind_byte(BlockKind::Normal));
    let mut entry_param_vals: Vec<u32, A> = Vec::with_capacity_in(live_in_insts.len(), alloc);
    for &lv in &live_in_insts {
        let p = push_inst(&mut func, Op::Param, Flags::NONE, Ty(at(view.tys, to_idx(lv))), 0, 0);
        entry_param_vals.push(p);
    }
    let header_args = entry_param_vals.get(..header_param_count).unwrap_or(&[]);
    push_jump(&mut func, osr_header, header_args);

    // Precompute exact value mapping for outside live-ins + all kept instructions.
    let mut val_map = filled(len, NONE, &alloc);
    for k in header_param_count..live_in_insts.len() {
        set_at(&mut val_map, to_idx(at(&live_in_insts, k)), at(&entry_param_vals, k));
    }
    let mut cursor = to_u32(func.ops.len());
    for &old_b in &kept_blocks {
        for inst in view.block(Block(old_b)) {
            set_at(&mut val_map, inst, cursor);
            cursor += 1;
        }
    }

    // Emit all reachable blocks starting from `osr_header`.
    for &old_b in &kept_blocks {
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, to_idx(old_b)));
        for inst in view.block(Block(old_b)) {
            let emitted = copy_inst_remapped(&view, inst, &val_map, &blk_map, &mut func);
            debug_assert_eq!(emitted, at(&val_map, inst));
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(OsrPlan {
        func,
        osr_header,
        live_in_insts,
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::Conv,
    };

    const FUEL: u64 = 100_000;

    #[test]
    fn specialize_guarded_value_folds_hot_divisor_and_matches_eval() {
        // `fn divrem(x: u64, d: u64) -> u64 { (x / d) * 3 + (x % d) }`
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let d = b.param(Ty::I64);
        let q = b.binary(Op::Udiv, Flags::NONE, x, d);
        let r = b.binary(Op::Urem, Flags::NONE, x, d);
        let k3 = b.iconst(Ty::I64, 3);
        let q3 = b.binary(Op::Mul, Flags::NONE, q, k3);
        let sum = b.binary(Op::Add, Flags::NONE, q3, r);
        b.ret(&[sum]);
        let func = b.finish_checked().unwrap();

        let out = specialize_guarded_value(func.view(), ValueSpecHint { param_idx: 1, hot_value: 10 }, TestHeap).unwrap();
        assert_eq!(out.stats.guards_inserted, 1);

        // Verify the fast path (block 1) divides by constant `10`!
        let v = out.func.view();
        let fast_range = v.block(Block(1));
        let fast_udiv = fast_range
            .clone()
            .find(|&inst| v.op(inst) == Op::Udiv)
            .expect("fast path must contain Udiv");
        let udiv_rhs = v.extra[v.data[fast_udiv] as usize + 1] as usize;
        assert_eq!(v.op(udiv_rhs), Op::Iconst, "fast path divisor must be folded to Iconst");
        assert_eq!(v.data[udiv_rhs], 10, "fast path divisor must equal hot_value 10");

        // Verify `eval::run_in` equivalence on BOTH fast path (`d == 10`) and slow path (`d == 7`)!
        for &d_val in &[10u128, 7u128, 1u128, 99u128] {
            for &x_val in &[0u128, 10, 42, 12345, 999_999] {
                let orig = eval::run_in(func.view(), &[Val::Int(x_val), Val::Int(d_val)], FUEL, TestHeap).unwrap();
                let spec = eval::run_in(out.func.view(), &[Val::Int(x_val), Val::Int(d_val)], FUEL, TestHeap).unwrap();
                assert_eq!(orig, spec, "mismatch for x={x_val}, d={d_val}");
            }
        }
    }

    #[test]
    fn guard_alias_for_simd_vectorizes_fast_path_and_checks_overlap_with_eval() {
        // 1. Real map/store loop: verify fast path gets 4x SIMD vectorized while slow path stays scalar.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I32)],
            &[],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let key = b.param(Ty::I32);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));
        b.begin(header);
        let i = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[]));
        b.begin(body);
        let k2 = b.iconst(Ty::I64, 2);
        let off = b.binary(Op::Shl, Flags::NONE, i, k2);
        let sp = b.ptr_add(Flags::NONE, src, off);
        let dp = b.ptr_add(Flags::NONE, dst, off);
        let val = b.load(Ty::I32, Flags::NONE, sp, 0, MemAttrs::aligned(2));
        let xored = b.binary(Op::Xor, Flags::NONE, val, key);
        b.store(Flags::NONE, xored, dp, 0, MemAttrs::aligned(2));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2]));
        b.begin(exit);
        b.ret(&[]);
        let map_fn = b.finish_checked().unwrap();

        let guarded_map = guard_alias_for_simd(map_fn.view(), 0, 1, 2, 4, TestHeap).unwrap();
        assert_eq!(guarded_map.stats.alias_guards_inserted, 1);
        let gv = guarded_map.func.view();
        let has_vec_store = (0..gv.len())
            .any(|i| gv.op(i) == Op::Store && Ty(gv.tys[gv.extra[gv.data[i] as usize] as usize]).is_vector());
        assert!(has_vec_store, "fast path must contain vectorized SIMD stores");

        // 2. Verify exact O(1) overlap vs non-overlap guard behavior via `eval::run_in`!
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let e2 = b2.create_block(BlockKind::Normal);
        b2.begin(e2);
        let d2 = b2.param(Ty::PTR);
        let s2 = b2.param(Ty::PTR);
        let n2 = b2.param(Ty::I64);
        let da = b2.convert(Op::PtrAddr, Flags::NONE, Ty::I64, d2);
        let sa = b2.convert(Op::PtrAddr, Flags::NONE, Ty::I64, s2);
        let mix = b2.binary(Op::Xor, Flags::NONE, da, sa);
        let res = b2.binary(Op::Add, Flags::NONE, mix, n2);
        b2.ret(&[res]);
        let probe_fn = b2.finish_checked().unwrap();

        let guarded_probe = guard_alias_for_simd(probe_fn.view(), 0, 1, 2, 4, TestHeap).unwrap();
        assert_eq!(guarded_probe.stats.alias_guards_inserted, 1);

        // Also build a branch-observing copy of `guarded_probe` whose block 0 is UNCHANGED
        // (exact emitted LIR alias guard) while block 1 returns `1` (fast) and block 2 returns `0` (slow),
        // proving via `eval::run_in` which arm the emitted LIR guard takes on every input!
        let gp_v = guarded_probe.func.view();
        let mut guard_obs = gp_v.to_func_in(TestHeap);
        clear_code_columns(&mut guard_obs);
        let mut obs_val_map: Vec<u32, TestHeap> = Vec::with_capacity_in(gp_v.len(), TestHeap);
        for i in 0..gp_v.len() as u32 {
            obs_val_map.push(i);
        }
        let mut obs_blk_map: Vec<u32, TestHeap> = Vec::with_capacity_in(gp_v.blocks(), TestHeap);
        for i in 0..gp_v.blocks() as u32 {
            obs_blk_map.push(i);
        }
        // Block 0: exact emitted guard from `guard_alias_for_simd`.
        guard_obs.block_starts.push(0);
        guard_obs.block_kinds.push(BlockKind::Normal as u8);
        for inst in gp_v.block(Block(0)) {
            copy_inst_remapped(&gp_v, inst, &obs_val_map, &obs_blk_map, &mut guard_obs);
        }
        // Block 1 (fast_entry): return 1.
        guard_obs.block_starts.push(guard_obs.ops.len() as u32);
        guard_obs.block_kinds.push(BlockKind::Normal as u8);
        let k1 = push_iconst(&mut guard_obs, Ty::I64, 1);
        let r1_at = guard_obs.extra.len() as u32;
        guard_obs.extra.push(1);
        guard_obs.extra.push(k1);
        push_inst(&mut guard_obs, Op::Return, Flags::NONE, Ty::NONE, r1_at, 0);
        // Block 2 (slow_entry): return 0.
        guard_obs.block_starts.push(guard_obs.ops.len() as u32);
        guard_obs.block_kinds.push(BlockKind::Normal as u8);
        let k0 = push_iconst(&mut guard_obs, Ty::I64, 0);
        let r0_at = guard_obs.extra.len() as u32;
        guard_obs.extra.push(1);
        guard_obs.extra.push(k0);
        push_inst(&mut guard_obs, Op::Return, Flags::NONE, Ty::NONE, r0_at, 0);
        verify(guard_obs.view()).unwrap();

        let src_base = 0x10000i128;
        let cases: &[(i128, u128, u128)] = &[
            (src_base + 4, 16, 0),    // dst == src + 4, bytes = 64 -> OVERLAP (slow)
            (src_base - 4, 16, 0),    // src == dst + 4, bytes = 64 -> OVERLAP (slow)
            (src_base, 16, 0),        // dst == src, bytes = 64 -> EXACT ALIAS (slow)
            (src_base + 60, 16, 0),   // dst == src + 60 < 64 -> OVERLAP (slow)
            (src_base + 64, 16, 1),   // dst == src + 64 == bytes -> DISJOINT (fast)
            (src_base - 64, 16, 1),   // src == dst + 64 == bytes -> DISJOINT (fast)
            (src_base + 1024, 16, 1), // dst == src + 1024 -> DISJOINT (fast)
            (src_base - 1024, 16, 1), // src == dst + 1024 -> DISJOINT (fast)
            (src_base + 4, 0, 1),     // len == 0 -> bytes == 0 -> DISJOINT (fast)
        ];
        for &(dst_addr, len, expect_fast) in cases {
            let args = [
                Val::Ptr { slot: Val::NO_SLOT, offset: dst_addr },
                Val::Ptr { slot: Val::NO_SLOT, offset: src_base },
                Val::Int(len),
            ];
            let orig = eval::run_in(probe_fn.view(), &args, FUEL, TestHeap).unwrap();
            let spec = eval::run_in(guarded_probe.func.view(), &args, FUEL, TestHeap).unwrap();
            assert_eq!(orig, spec, "eval mismatch for dst={dst_addr:#x}, len={len}");

            let Outcome::Returned(branch) = eval::run_in(guard_obs.view(), &args, FUEL, TestHeap).unwrap() else {
                panic!("guard observer did not return");
            };
            assert_eq!(&branch[..], &[Val::Int(expect_fast)], "wrong branch for dst={dst_addr:#x}, len={len}");
        }
    }

    #[test]
    fn synthesize_osr_entry_resumes_sum_loop_midway_with_exact_result() {
        // `fn sum(n: u64) -> u64 { let mut acc = 0; for i in 0..n { acc += i; } acc }`
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));
        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[]));
        b.begin(body);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, i);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));
        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let plan = synthesize_osr_entry(func.view(), Block(1), TestHeap).unwrap();
        assert_eq!(plan.osr_header, 1);
        assert_eq!(plan.live_in_insts.len(), 3, "expected live-ins [i, acc, n]");
        assert_eq!(plan.live_in_insts[0], i.0);
        assert_eq!(plan.live_in_insts[1], acc.0);
        assert_eq!(plan.live_in_insts[2], n.0);

        // Enter midway at `i = 50, acc = 1225 (sum 0..50), n = 100` -> must return `4950`!
        let Outcome::Returned(osr_res) =
            eval::run_in(plan.func.view(), &[Val::Int(50), Val::Int(1225), Val::Int(100)], FUEL, TestHeap).unwrap()
        else {
            panic!("OSR eval did not return");
        };
        assert_eq!(&osr_res[..], &[Val::Int(4950)]);

        // Also check full run from `i = 0, acc = 0, n = 100` matches original `sum(100)`.
        let orig = eval::run_in(func.view(), &[Val::Int(100)], FUEL, TestHeap).unwrap();
        let osr_full =
            eval::run_in(plan.func.view(), &[Val::Int(0), Val::Int(0), Val::Int(100)], FUEL, TestHeap).unwrap();
        assert_eq!(orig, osr_full);
    }
}
