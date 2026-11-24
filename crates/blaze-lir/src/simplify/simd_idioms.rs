//! SIMD Data-Parallel Idiom Lowering for Apple Silicon / AArch64 NEON:
//!
//! 1. **Byte-Table / LUT Loop Vectorization (`lower_byte_lut_loops`)**: detects counted loops
//!    `for i in init..n` that load `b = src[i]` (`i8`), mask/shift into a 16-byte or 64-byte
//!    loop-invariant lookup table (`b & 15`, `b & 63`, or `b >> 4`), load `out = lut[idx]`, and
//!    either store `dst[i] = out` (with wraparound-safe runtime alias guards against `src` and
//!    `lut`) or accumulate `acc += zext(out)` / `acc ^= out`. Rewrites into a 4x-unrolled
//!    (64 bytes/iter) NEON `TBL` (`"blaze.neon.tbl16"` / `"blaze.neon.tbl64"`) loop with scalar
//!    tail for `n % 64`.
//! 2. **Widening `i8`/`u8` Dot-Product Vectorization (`lower_dotprod_loops`)**: detects counted
//!    reduction loops `acc += sext(a[i]) * sext(b[i])` or `acc += zext(a[i]) * zext(b[i])` into
//!    `i32` or `i64` accumulators and rewrites into a 4x-unrolled (64 MACs/iter) `FEAT_DotProd`
//!    NEON `SDOT`/`UDOT` (`"blaze.neon.sdot"` / `"blaze.neon.udot"`) loop with 4 independent
//!    `<4 x i32>` accumulators (and overflow-free 16,384-element outer chunking for `i64`),
//!    horizontal `Op::ReduceAdd`, and exact scalar tail.
//! 3. **Branchless Compare-Exchange Sorting Networks (`lower_sorting_networks`)**: canonicalizes
//!    both `Select`-based compare-swap pairs and conditional compare-and-swap CFG diamonds
//!    (`if a > b { swap(a, b) }`) into branchless `Smin`/`Smax` or `Umin`/`Umax` (lowering to
//!    1-cycle CSSC `smin`/`smax` or NEON `smin.4s`/`smax.4s`), plus verified branchless
//!    constant-width builders [`emit_sort4_network`] (5 comparators, depth 3) and
//!    [`emit_sort8_network`] (19 comparators, depth 6).
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), bounded (Rule 18), explicit
//! allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, dead, each, filled},
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Scalar, Ty, Value, View,
    arith::{iconst_value, wide_value},
    builder::{Builder, Full, Param, Sink, Variadic},
    func::{Conv, Ext, IntrinsicId, IntrinsicRec, ParamKind, ParamRec, SigRec},
    op::{Field, Layout},
    verify,
};

/// 1-register NEON `tbl.16b` intrinsic: `(tbl: <16 x i8>, idx: <16 x i8>) -> <16 x i8>`.
pub const NEON_TBL16_INTRINSIC: &str = "blaze.neon.tbl16";

/// 4-register NEON `tbl.16b {v0..v3}` intrinsic:
/// `(t0: <16 x i8>, t1: <16 x i8>, t2: <16 x i8>, t3: <16 x i8>, idx: <16 x i8>) -> <16 x i8>`.
pub const NEON_TBL64_INTRINSIC: &str = "blaze.neon.tbl64";

/// `FEAT_DotProd` signed `sdot.4s` intrinsic:
/// `(acc: <4 x i32>, a: <16 x i8>, b: <16 x i8>) -> <4 x i32>`.
pub const NEON_SDOT_INTRINSIC: &str = "blaze.neon.sdot";

/// `FEAT_DotProd` unsigned `udot.4s` intrinsic:
/// `(acc: <4 x i32>, a: <16 x i8>, b: <16 x i8>) -> <4 x i32>`.
pub const NEON_UDOT_INTRINSIC: &str = "blaze.neon.udot";

/// Bounded maximum number of loops transformed per idiom pass in a single function (Rule 18).
const MAX_LOOPS_PER_FUNC: usize = 8;

/// Outer chunk size (elements) for `i64` widening reductions accumulated in `<4 x i32>`.
///
/// 16,384 elements = 256 iterations of 64 bytes:
/// - Signed `i8 * i8`: worst-case total sum is in `[-266_338_304, +268_435_456]`, well inside `i32`.
/// - Unsigned `u8 * u8`: worst-case total sum is `1_065_369_600`, well inside `u32` (and `i32`).
/// - Unsigned `u8` sum (`lut` + `udot` with 1s): worst-case total sum is `4_177_920`, well inside `u32`.
const I64_ACC_CHUNK_ELEMS: u64 = 16_384;

/// Configuration for [`lower_simd_idioms`] (Rule 83: single options struct).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SimdIdiomOpts {
    pub enable_tbl: bool,
    pub enable_dotprod: bool,
    pub enable_sort_net: bool,
}

impl SimdIdiomOpts {
    /// Enables all Apple Silicon NEON / `FEAT_DotProd` / CSSC idiom lowerings.
    pub const fn apple_silicon() -> Self {
        Self {
            enable_tbl: true,
            enable_dotprod: true,
            enable_sort_net: true,
        }
    }
}

impl Default for SimdIdiomOpts {
    fn default() -> Self {
        Self::apple_silicon()
    }
}

/// Telemetry returned by [`lower_simd_idioms`] and individual idiom passes.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SimdIdiomStats {
    pub tbl_loops: u32,
    pub dotprod_loops: u32,
    pub sort_swaps_canonicalized: u32,
}

/// Output of [`lower_simd_idioms`] and individual idiom passes.
#[derive(Debug)]
pub struct SimdIdiomOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: SimdIdiomStats,
}

// ============================================================================
// Ratchet-Clean Low-Level Helpers (zero panics, zero `as`, zero unchecked [])
// ============================================================================

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn to_u16(v: usize) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

#[inline]
fn trunc_u32(v: u64) -> u32 {
    u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0)
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
fn const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = Ty(view.tys.get(inst).copied()?);
    if !ty.is_scalar() || !ty.is_int() || ty.bits(64) > 64 {
        return None;
    }
    let bits = ty.bits(64);
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(view.data.get(inst).copied()?, bits),
        Op::IconstWide => {
            let start = to_idx(view.data.get(inst).copied()?);
            let words = to_idx(ty.const_words());
            let end = start.checked_add(words)?;
            wide_value(view.extra.get(start..end)?, bits)
        }
        _ => return None,
    };
    u64::try_from(v).ok()
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

fn push_unary<A: Allocator>(func: &mut Func<A>, op: Op, ty: Ty, a: u32) -> u32 {
    push_inst(func, op, Flags::NONE, ty, a, 0)
}

fn push_icmp<A: Allocator>(func: &mut Func<A>, cond: IntCond, ty: Ty, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Icmp, Flags::NONE, ty, idx, 0)
}

fn push_load<A: Allocator>(func: &mut Func<A>, ty: Ty, ptr: u32, off: i32, attr: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(ptr);
    func.extra.push(off.cast_unsigned());
    push_inst(func, Op::Load, Flags::NONE, ty, idx, attr)
}

fn push_store<A: Allocator>(func: &mut Func<A>, val: u32, ptr: u32, off: i32, attr: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(val);
    func.extra.push(ptr);
    func.extra.push(off.cast_unsigned());
    push_inst(func, Op::Store, Flags::NONE, Ty::NONE, idx, attr)
}

fn push_jump<A: Allocator>(func: &mut Func<A>, target: u32, args: &[u32]) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len()));
    for &arg in args {
        func.extra.push(arg);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, idx, 0)
}

fn push_br<A: Allocator>(
    func: &mut Func<A>,
    cond: u32,
    t_blk: u32,
    t_args: &[u32],
    f_blk: u32,
    f_args: &[u32],
) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(cond);
    func.extra.push(t_blk);
    func.extra.push(to_u32(t_args.len()));
    for &arg in t_args {
        func.extra.push(arg);
    }
    func.extra.push(f_blk);
    func.extra.push(to_u32(f_args.len()));
    for &arg in f_args {
        func.extra.push(arg);
    }
    push_inst(func, Op::Br, Flags::NONE, Ty::NONE, idx, 0)
}

fn push_target_intrinsic<A: Allocator>(
    func: &mut Func<A>,
    id: IntrinsicId,
    ret_ty: Ty,
    args: &[u32],
) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(id.0);
    func.extra.push(to_u32(args.len()));
    for &arg in args {
        func.extra.push(arg);
    }
    push_inst(func, Op::TargetIntrinsic, Flags::NONE, ret_ty, idx, 0)
}

fn clear_func_code<A: Allocator>(func: &mut Func<A>) {
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

/// Ensures target intrinsic `name` with signature `(params...) -> ret` exists in `func.intrinsics`,
/// preserving canonical CSR tiling of `sigs`/`sig_params` and `intrinsics`/`names`.
fn ensure_intrinsic<A: Allocator>(
    func: &mut Func<A>,
    name: &str,
    params: &[Ty],
    ret: Ty,
) -> IntrinsicId {
    let want = name.as_bytes();
    let want_params_u16 = to_u16(params.len());
    let c_conv = conv_byte(Conv::C);
    let no_ext = ext_byte(Ext::None);
    let val_kind = param_kind_byte(ParamKind::Value);

    let sig_matches = |sig: SigRec, sig_params: &[ParamRec]| -> bool {
        if sig.conv != c_conv || sig.bits != 0 || sig.params != want_params_u16 || sig.rets != 1 {
            return false;
        }
        let first = to_idx(sig.first);
        let Some(total) = params.len().checked_add(1) else {
            return false;
        };
        let Some(end) = first.checked_add(total) else {
            return false;
        };
        let Some(slice) = sig_params.get(first..end) else {
            return false;
        };
        for (idx, &p_ty) in params.iter().enumerate() {
            let expected = ParamRec { facts: 0, ty: p_ty.0, ext: no_ext, kind: val_kind };
            if slice.get(idx).copied() != Some(expected) {
                return false;
            }
        }
        let expected_ret = ParamRec { facts: 0, ty: ret.0, ext: no_ext, kind: val_kind };
        slice.get(params.len()).copied() == Some(expected_ret)
    };

    for (row, rec) in func.intrinsics.iter().enumerate() {
        let start = to_idx(rec.name_first);
        let Some(end) = start.checked_add(to_idx(rec.name_len)) else {
            continue;
        };
        if func.names.get(start..end) == Some(want)
            && let Some(&sig) = func.sigs.get(to_idx(rec.sig))
            && sig_matches(sig, &func.sig_params)
        {
            return IntrinsicId(to_u32(row));
        }
    }

    let existing_sig = func.sigs.iter().enumerate().find_map(|(r, &s)| {
        sig_matches(s, &func.sig_params).then_some(to_u32(r))
    });
    let sig_row = match existing_sig {
        Some(row) => row,
        None => {
            let first = to_u32(func.sig_params.len());
            for &p_ty in params {
                func.sig_params.push(ParamRec {
                    facts: 0,
                    ty: p_ty.0,
                    ext: no_ext,
                    kind: val_kind,
                });
            }
            func.sig_params.push(ParamRec {
                facts: 0,
                ty: ret.0,
                ext: no_ext,
                kind: val_kind,
            });
            let row = to_u32(func.sigs.len());
            func.sigs.push(SigRec {
                first,
                params: want_params_u16,
                rets: 1,
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

#[derive(Clone, Copy)]
enum ValFixup {
    Data { inst: usize, old_val: u32 },
    Extra { pos: usize, old_val: u32 },
}

fn remap_blk(blk_map: Option<&[u32]>, old: u32) -> u32 {
    match blk_map {
        Some(m) => at(m, to_idx(old)),
        None => old,
    }
}

fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: Option<&[u32]>,
    func: &mut Func<A>,
    fixups: &mut Vec<ValFixup, A>,
) -> u32 {
    let op = view.op(inst);
    let op_raw = at(view.ops, inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attr = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let inst_data = at(view.data, inst);
    let new_inst = func.ops.len();
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            let mapped = at(val_map, to_idx(inst_data));
            if mapped == NONE {
                fixups.push(ValFixup::Data { inst: new_inst, old_val: inst_data });
                0
            } else {
                mapped
            }
        }
        Layout::Data(Field::Block) => remap_blk(blk_map, inst_data),
        Layout::Data(_) => inst_data,
        Layout::Extra(fields) => {
            let src = to_idx(inst_data);
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        let old = at(view.extra, p);
                        let mapped = at(val_map, to_idx(old));
                        if mapped == NONE {
                            fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                            func.extra.push(0);
                        } else {
                            func.extra.push(mapped);
                        }
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            let old = at(view.extra, p + k);
                            let mapped = at(val_map, to_idx(old));
                            if mapped == NONE {
                                fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                                func.extra.push(0);
                            } else {
                                func.extra.push(mapped);
                            }
                        }
                        p += n;
                    }
                    Field::Block => {
                        func.extra.push(remap_blk(blk_map, at(view.extra, p)));
                        p += 1;
                    }
                    Field::Blocks => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(remap_blk(blk_map, at(view.extra, p + k)));
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(remap_blk(blk_map, at(view.extra, p)));
                        let n_u32 = at(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            let old = at(view.extra, p + k);
                            let mapped = at(val_map, to_idx(old));
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
                            func.extra.push(remap_blk(blk_map, at(view.extra, p)));
                            let an_u32 = at(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                let old = at(view.extra, p + k);
                                let mapped = at(val_map, to_idx(old));
                                if mapped == NONE {
                                    fixups.push(ValFixup::Extra {
                                        pos: func.extra.len(),
                                        old_val: old,
                                    });
                                    func.extra.push(0);
                                } else {
                                    func.extra.push(mapped);
                                }
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
                    Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm => {
                        func.extra.push(at(view.extra, p));
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

fn apply_val_fixups<A: Allocator>(func: &mut Func<A>, val_map: &[u32], fixups: &[ValFixup]) {
    for &fixup in fixups {
        match fixup {
            ValFixup::Data { inst, old_val } => {
                let mapped = at(val_map, to_idx(old_val));
                set_at(&mut func.data, inst, mapped);
            }
            ValFixup::Extra { pos, old_val } => {
                let mapped = at(val_map, to_idx(old_val));
                set_at(&mut func.extra, pos, mapped);
            }
        }
    }
}

/// Computes immediate dominators (`idom[b]`, `NONE` for unreachable blocks) via Cooper-Harvey-Kennedy.
fn compute_idom<A: Allocator + Copy>(view: &View<'_>, alloc: &A) -> Vec<u32, A> {
    let blocks = view.blocks();
    let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        succs.push(Vec::new_in(*alloc));
        preds.push(Vec::new_in(*alloc));
    }
    for b in 0..blocks {
        let b32 = to_u32(b);
        each(view, view.terminator(Block(b32)), |item| {
            if let Item::Edge(edge) = item {
                if let Some(sb) = succs.get_mut(b) {
                    sb.push(edge.block);
                }
                if let Some(pe) = preds.get_mut(to_idx(edge.block)) {
                    pe.push(b32);
                }
            }
        });
    }

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
            if !at(&seen, si) {
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
    let max_iters = blocks.saturating_mul(2).saturating_add(2);
    for _ in 0..max_iters {
        let mut changed = false;
        for &b in rpo.iter().skip(1) {
            let bi = to_idx(b);
            let mut new_idom = NONE;
            if let Some(b_preds) = preds.get(bi) {
                for &p in b_preds.iter() {
                    if at(&idom, to_idx(p)) == NONE {
                        continue;
                    }
                    new_idom = if new_idom == NONE {
                        p
                    } else {
                        intersect_idom(&idom, &order, p, new_idom, max_iters)
                    };
                }
            }
            if new_idom != NONE && at(&idom, bi) != new_idom {
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

fn intersect_idom(idom: &[u32], order: &[u32], mut a: u32, mut b: u32, max_steps: usize) -> u32 {
    for _ in 0..max_steps {
        if a == b {
            return a;
        }
        if at(order, to_idx(a)) > at(order, to_idx(b)) {
            a = at(idom, to_idx(a));
        } else {
            b = at(idom, to_idx(b));
        }
    }
    a
}

fn dominates(idom: &[u32], a: u32, mut b: u32) -> bool {
    if a == NONE || b == NONE {
        return false;
    }
    for _ in 0..idom.len() {
        if a == b {
            return true;
        }
        if b == 0 || b == NONE {
            return false;
        }
        b = at(idom, to_idx(b));
    }
    false
}

/// Checks that SSA value `val` is defined in a block dominating `target_blk` and laid out at or
/// before `target_blk` (so its remapped value is available immediately when emitting at `target_blk`).
fn is_ready_invariant(inst_block: &[u32], idom: &[u32], val: u32, target_blk: u32) -> bool {
    let def_blk = at(inst_block, to_idx(val));
    def_blk <= target_blk && dominates(idom, def_blk, target_blk)
}

// ============================================================================
// Shared Counted-Loop Header / Latch Analysis (used by Idioms 1 & 2)
// ============================================================================

struct CfgInfo<A: Allocator> {
    inst_block: Vec<u32, A>,
    preds: Vec<Vec<u32, A>, A>,
    idom: Vec<u32, A>,
}

fn build_cfg_info<A: Allocator + Copy>(view: &View<'_>, alloc: &A) -> CfgInfo<A> {
    let blocks = view.blocks();
    let len = view.len();
    let mut inst_block = filled(len, 0u32, alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(*alloc));
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
        let term = view.terminator(Block(b_u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(p) = preds.get_mut(tb)
                    && p.iter().all(|&x| x != b_u32)
                {
                    p.push(b_u32);
                }
            }
        });
    }
    let idom = compute_idom(view, alloc);
    CfgInfo { inst_block, preds, idom }
}

/// Structural header/body/latch skeleton of a counted `for i in iv_init..bound` loop.
#[derive(Clone, Copy)]
struct CountedLoop {
    preheader: u32,
    header: u32,
    body: u32,
    param_count: usize,
    iv_pos: usize,
    iv_inst: u32,
    iv_init: u32,
    bound: u32,
    /// If `param_count == 2`, the non-IV accumulator parameter metadata.
    acc_pos: Option<usize>,
    acc_inst: u32,
    acc_init: u32,
    acc_next: u32,
    acc_ty: Ty,
}

/// Matches a 2-block counted loop (`preheader -> header -> body -> header`, `header -> exit`)
/// where `header` has 1 parameter (`iv: I64`) or 2 parameters (`iv: I64` and `acc`).
fn match_counted_loop(view: &View<'_>, cfg: &CfgInfo<impl Allocator>, header: u32) -> Option<CountedLoop> {
    if view.block_kind(Block(header)) != BlockKind::Normal {
        return None;
    }
    let h_idx = to_idx(header);
    let (p0, p1) = match cfg.preds.get(h_idx).map(Vec::as_slice) {
        Some(&[p0, p1]) => (p0, p1),
        _ => return None,
    };
    let (preheader, body) = if dominates(&cfg.idom, header, p1) && !dominates(&cfg.idom, header, p0) {
        (p0, p1)
    } else if dominates(&cfg.idom, header, p0) && !dominates(&cfg.idom, header, p1) {
        (p1, p0)
    } else {
        return None;
    };
    if preheader == header || body == header || preheader == body {
        return None;
    }
    if view.block_kind(Block(body)) != BlockKind::Normal {
        return None;
    }

    // `preheader` must end with `Jump(header, pre_args)`.
    let pre_term = view.terminator(Block(preheader));
    if view.op(pre_term) != Op::Jump {
        return None;
    }
    let pre_at = to_idx(at(view.data, pre_term));
    let h_params = view.params(Block(header));
    let param_count = h_params.len();
    if param_count != 1 && param_count != 2 {
        return None;
    }
    if at(view.extra, pre_at) != header || to_idx(at(view.extra, pre_at + 1)) != param_count {
        return None;
    }

    // `body` must have 1 pred (`header`), 0 params, and end with `Jump(header, latch_args)`.
    if cfg.preds.get(to_idx(body)).map(Vec::len) != Some(1)
        || !view.params(Block(body)).is_empty()
    {
        return None;
    }
    let b_term = view.terminator(Block(body));
    if view.op(b_term) != Op::Jump {
        return None;
    }
    let latch_at = to_idx(at(view.data, b_term));
    if at(view.extra, latch_at) != header || to_idx(at(view.extra, latch_at + 1)) != param_count {
        return None;
    }

    // `header` must end with `Br(cmp, ...)` entering `body` iff `iv < bound`.
    let h_range = view.block(Block(header));
    let h_term = h_range.end.checked_sub(1)?;
    if view.op(h_term) != Op::Br {
        return None;
    }
    let h_at = to_idx(at(view.data, h_term));
    let cmp_inst = to_idx(at(view.extra, h_at));
    let t_blk = at(view.extra, h_at + 1);
    let t_argc = to_idx(at(view.extra, h_at + 2));
    let f_blk = at(view.extra, h_at + 3 + t_argc);
    let f_argc = to_idx(at(view.extra, h_at + 4 + t_argc));

    // Only allow `Param`, `Icmp`, and constants in `header`.
    for inst in h_range.start..h_term {
        if inst != cmp_inst
            && !matches!(view.op(inst), Op::Param | Op::Iconst | Op::IconstWide)
        {
            return None;
        }
    }

    let body_on_true = if t_blk == body && t_argc == 0 && f_blk != body {
        true
    } else if f_blk == body && f_argc == 0 && t_blk != body {
        false
    } else {
        return None;
    };

    if view.op(cmp_inst) != Op::Icmp {
        return None;
    }
    let cmp_at = to_idx(at(view.data, cmp_inst));
    let cond = IntCond::from_word(at(view.extra, cmp_at))?;
    let cmp_a = at(view.extra, cmp_at + 1);
    let cmp_b = at(view.extra, cmp_at + 2);

    // Match strict unsigned upper bound `enter body iff iv < bound`.
    let (iv_inst, bound) = match (body_on_true, cond) {
        (true, IntCond::Ult) | (false, IntCond::Uge) => (cmp_a, cmp_b),
        (true, IntCond::Ugt) | (false, IntCond::Ule) => (cmp_b, cmp_a),
        _ => return None,
    };

    // Identify `iv_pos` among `header` parameters; must be `Ty::I64`.
    let iv_pos = (0..param_count).find(|&k| to_u32(h_params.start + k) == iv_inst)?;
    if Ty(at(view.tys, to_idx(iv_inst))) != Ty::I64 {
        return None;
    }
    if !is_ready_invariant(&cfg.inst_block, &cfg.idom, bound, preheader) {
        return None;
    }

    let iv_init = at(view.extra, pre_at + 2 + iv_pos);
    if !is_ready_invariant(&cfg.inst_block, &cfg.idom, iv_init, preheader) {
        return None;
    }

    // Verify `iv_next = Add(iv, 1)` in `body`.
    let iv_next = at(view.extra, latch_at + 2 + iv_pos);
    let iv_next_idx = to_idx(iv_next);
    if at(&cfg.inst_block, iv_next_idx) != body || view.op(iv_next_idx) != Op::Add {
        return None;
    }
    let add_at = to_idx(at(view.data, iv_next_idx));
    let (al, ar) = (at(view.extra, add_at), at(view.extra, add_at + 1));
    let step_is_1 = (al == iv_inst && const_u64(view, to_idx(ar)) == Some(1))
        || (ar == iv_inst && const_u64(view, to_idx(al)) == Some(1));
    if !step_is_1 {
        return None;
    }

    let (acc_pos, acc_inst, acc_init, acc_next, acc_ty) = if param_count == 2 {
        let ap = 1 - iv_pos;
        let ai = to_u32(h_params.start + ap);
        let a_init = at(view.extra, pre_at + 2 + ap);
        let a_next = at(view.extra, latch_at + 2 + ap);
        if !is_ready_invariant(&cfg.inst_block, &cfg.idom, a_init, preheader) {
            return None;
        }
        (Some(ap), ai, a_init, a_next, Ty(at(view.tys, to_idx(ai))))
    } else {
        (None, NONE, NONE, NONE, Ty::NONE)
    };

    Some(CountedLoop {
        preheader,
        header,
        body,
        param_count,
        iv_pos,
        iv_inst,
        iv_init,
        bound,
        acc_pos,
        acc_inst,
        acc_init,
        acc_next,
        acc_ty,
    })
}

/// Checks that `load_inst` is a non-volatile `Ty::I8` load at `PtrAdd(base, iv) + const_off`
/// where `base` is invariant and ready at `preheader`.
fn match_unit_i8_load(
    view: &View<'_>,
    cfg: &CfgInfo<impl Allocator>,
    load_inst: usize,
    body: u32,
    iv_inst: u32,
    preheader: u32,
) -> Option<(u32, i32)> {
    if at(&cfg.inst_block, load_inst) != body
        || view.op(load_inst) != Op::Load
        || Ty(at(view.tys, load_inst)) != Ty::I8
        || Flags(at(view.flags, load_inst)).contains(Flags::VOLATILE)
    {
        return None;
    }
    let lat = to_idx(at(view.data, load_inst));
    let ptr_v = to_idx(at(view.extra, lat));
    let off = at(view.extra, lat + 1).cast_signed();
    if view.op(ptr_v) != Op::PtrAdd {
        return None;
    }
    let pat = to_idx(at(view.data, ptr_v));
    let base = at(view.extra, pat);
    let idx = at(view.extra, pat + 1);
    if idx != iv_inst || !is_ready_invariant(&cfg.inst_block, &cfg.idom, base, preheader) {
        return None;
    }
    Some((base, off))
}

fn emit_offset_base<A: Allocator>(func: &mut Func<A>, base: u32, off: i32) -> u32 {
    if off == 0 {
        base
    } else {
        let koff = push_iconst(func, Ty::I64, i64::from(off).cast_unsigned());
        push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, base, koff)
    }
}

/// Builds a 1- or 2-element argument list for `orig_header` in the exact original parameter order.
fn ordered_header_args(
    param_count: usize,
    iv_pos: usize,
    iv_val: u32,
    acc_pos: Option<usize>,
    acc_val: u32,
) -> ([u32; 2], usize) {
    let mut buf = [0u32; 2];
    set_at(&mut buf, iv_pos, iv_val);
    if let Some(ap) = acc_pos {
        set_at(&mut buf, ap, acc_val);
    }
    (buf, param_count)
}

// ============================================================================
// Idiom 1: Byte-Table / LUT Loop Vectorization (`NEON TBL` 16-way / 64-way)
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LutIndexMode {
    /// `b & 15` -> 16-byte LUT (`tbl.16b` 1 register).
    Mask16,
    /// `b & 63` -> 64-byte LUT (`tbl.16b` 4 registers).
    Mask64,
    /// `b >> 4` -> 16-byte LUT (`tbl.16b` 1 register, high nibble).
    ShiftRight4,
}

impl LutIndexMode {
    const fn table_bytes(self) -> u64 {
        match self {
            LutIndexMode::Mask16 | LutIndexMode::ShiftRight4 => 16,
            LutIndexMode::Mask64 => 64,
        }
    }

    const fn is_64(self) -> bool {
        matches!(self, LutIndexMode::Mask64)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LutSink {
    /// `dst[i] = out` (map loop).
    Store { dst_base: u32, dst_off: i32 },
    /// `acc = acc + zext(out)` (or `acc + out` for `Ty::I8`).
    ReduceAdd,
    /// `acc = acc ^ zext(out)` (or `acc ^ out` for `Ty::I8`).
    ReduceXor,
}

#[derive(Clone, Copy)]
struct LutPlan {
    lp: CountedLoop,
    src_base: u32,
    src_off: i32,
    lut_base: u32,
    lut_off: i32,
    mode: LutIndexMode,
    sink: LutSink,
}

/// Traces `idx_val` feeding `PtrAdd(lut_base, idx_val)` back to `(src_load_inst, LutIndexMode)`.
fn match_lut_index_expr(
    view: &View<'_>,
    cfg: &CfgInfo<impl Allocator>,
    idx_val: u32,
    body: u32,
) -> Option<(usize, LutIndexMode)> {
    let mut cur = to_idx(idx_val);
    // Strip at most two leading `Zext` layers (e.g. `i8 -> i32 -> i64` or `i8 -> i64`).
    for _ in 0..2 {
        if at(&cfg.inst_block, cur) == body && view.op(cur) == Op::Zext {
            cur = to_idx(at(view.data, cur));
        } else {
            break;
        }
    }
    if at(&cfg.inst_block, cur) != body {
        return None;
    }

    let (src_expr, mode) = match view.op(cur) {
        Op::And => {
            let aat = to_idx(at(view.data, cur));
            let (l, r) = (at(view.extra, aat), at(view.extra, aat + 1));
            if let Some(c) = const_u64(view, to_idx(r)) {
                let m = match c {
                    15 => LutIndexMode::Mask16,
                    63 => LutIndexMode::Mask64,
                    _ => return None,
                };
                (l, m)
            } else if let Some(c) = const_u64(view, to_idx(l)) {
                let m = match c {
                    15 => LutIndexMode::Mask16,
                    63 => LutIndexMode::Mask64,
                    _ => return None,
                };
                (r, m)
            } else {
                return None;
            }
        }
        Op::Lshr => {
            let sat = to_idx(at(view.data, cur));
            let (l, r) = (at(view.extra, sat), at(view.extra, sat + 1));
            if const_u64(view, to_idx(r)) != Some(4) {
                return None;
            }
            (l, LutIndexMode::ShiftRight4)
        }
        _ => return None,
    };

    // `src_expr` is either the `Ty::I8` load directly, or `Zext(Ty::I8 load)`.
    let mut load_cand = to_idx(src_expr);
    for _ in 0..2 {
        if at(&cfg.inst_block, load_cand) == body && view.op(load_cand) == Op::Zext {
            load_cand = to_idx(at(view.data, load_cand));
        } else {
            break;
        }
    }
    // For `Lshr(Zext(b), 4)` on an `i8` byte, unsigned shift right by 4 is identical to `b >> 4`
    // because zero-extension fills upper bits with 0!
    if at(&cfg.inst_block, load_cand) != body
        || view.op(load_cand) != Op::Load
        || Ty(at(view.tys, load_cand)) != Ty::I8
    {
        return None;
    }
    Some((load_cand, mode))
}

fn find_lut_plan(view: &View<'_>, cfg: &CfgInfo<impl Allocator>) -> Option<LutPlan> {
    for h in 1..view.blocks() {
        let Some(lp) = match_counted_loop(view, cfg, to_u32(h)) else {
            continue;
        };
        let b_range = view.block(Block(lp.body));
        let b_term = b_range.end.saturating_sub(1);

        // Identify the sink: either a single unit-stride `i8` store (1-param loop) or an
        // `Add`/`Xor` reduction into `acc` (2-param loop).
        let (lut_load_inst, store_inst, sink) = if lp.param_count == 1 {
            // Must have exactly 1 `Op::Store` in `body`, storing `Ty::I8` at `PtrAdd(dst_base, iv)`.
            let mut found_store: Option<usize> = None;
            let mut bad = false;
            for inst in b_range.start..b_term {
                if view.op(inst) == Op::Store {
                    if found_store.is_some() {
                        bad = true;
                        break;
                    }
                    found_store = Some(inst);
                }
            }
            let Some(st) = found_store else { continue };
            if bad || Flags(at(view.flags, st)).contains(Flags::VOLATILE) {
                continue;
            }
            let sat = to_idx(at(view.data, st));
            let val_v = at(view.extra, sat);
            let dst_ptr = to_idx(at(view.extra, sat + 1));
            let dst_off = at(view.extra, sat + 2).cast_signed();
            if Ty(at(view.tys, to_idx(val_v))) != Ty::I8 || view.op(dst_ptr) != Op::PtrAdd {
                continue;
            }
            let dpat = to_idx(at(view.data, dst_ptr));
            let (dst_base, dst_idx) = (at(view.extra, dpat), at(view.extra, dpat + 1));
            if dst_idx != lp.iv_inst
                || !is_ready_invariant(&cfg.inst_block, &cfg.idom, dst_base, lp.preheader)
            {
                continue;
            }
            (to_idx(val_v), Some(st), LutSink::Store { dst_base, dst_off })
        } else {
            // Reduction loop: `acc_ty` must be `I8`, `I32`, or `I64`.
            if !matches!(lp.acc_ty, Ty::I8 | Ty::I32 | Ty::I64) {
                continue;
            }
            let upd = to_idx(lp.acc_next);
            if at(&cfg.inst_block, upd) != lp.body {
                continue;
            }
            let sink = match view.op(upd) {
                Op::Add => LutSink::ReduceAdd,
                Op::Xor => LutSink::ReduceXor,
                _ => continue,
            };
            let uat = to_idx(at(view.data, upd));
            let (ul, ur) = (at(view.extra, uat), at(view.extra, uat + 1));
            let elem_v = if ul == lp.acc_inst {
                ur
            } else if ur == lp.acc_inst {
                ul
            } else {
                continue;
            };
            let lut_inst = if lp.acc_ty == Ty::I8 {
                to_idx(elem_v)
            } else {
                let ei = to_idx(elem_v);
                if at(&cfg.inst_block, ei) != lp.body || view.op(ei) != Op::Zext {
                    continue;
                }
                to_idx(at(view.data, ei))
            };
            (lut_inst, None, sink)
        };

        // `lut_load_inst` must be a non-volatile `Ty::I8` load from `PtrAdd(lut_base, idx)`.
        if at(&cfg.inst_block, lut_load_inst) != lp.body
            || view.op(lut_load_inst) != Op::Load
            || Ty(at(view.tys, lut_load_inst)) != Ty::I8
            || Flags(at(view.flags, lut_load_inst)).contains(Flags::VOLATILE)
        {
            continue;
        }
        let llat = to_idx(at(view.data, lut_load_inst));
        let lut_ptr = to_idx(at(view.extra, llat));
        let lut_off = at(view.extra, llat + 1).cast_signed();
        if view.op(lut_ptr) != Op::PtrAdd {
            continue;
        }
        let lpat = to_idx(at(view.data, lut_ptr));
        let (lut_base, lut_idx) = (at(view.extra, lpat), at(view.extra, lpat + 1));
        if !is_ready_invariant(&cfg.inst_block, &cfg.idom, lut_base, lp.preheader) {
            continue;
        }

        // Trace `lut_idx` back to `(src_load_inst, mode)`.
        let Some((src_load_inst, mode)) = match_lut_index_expr(view, cfg, lut_idx, lp.body) else {
            continue;
        };
        if src_load_inst == lut_load_inst {
            continue;
        }
        let Some((src_base, src_off)) =
            match_unit_i8_load(view, cfg, src_load_inst, lp.body, lp.iv_inst, lp.preheader)
        else {
            continue;
        };

        // Purity check: no other loads, stores, calls, traps, or side-effecting ops in `body`.
        let body_ok = (b_range.start..b_term).all(|k| {
            k == src_load_inst
                || k == lut_load_inst
                || store_inst == Some(k)
                || matches!(
                    view.op(k),
                    Op::PtrAdd
                        | Op::And
                        | Op::Lshr
                        | Op::Zext
                        | Op::Add
                        | Op::Xor
                        | Op::Iconst
                        | Op::IconstWide
                )
        });
        if !body_ok {
            continue;
        }

        return Some(LutPlan {
            lp,
            src_base,
            src_off,
            lut_base,
            lut_off,
            mode,
            sink,
        });
    }
    None
}

fn apply_lut_plan<A: Allocator + Copy>(
    view: &View<'_>,
    plan: LutPlan,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let Some(v16i8) = Ty::vector(Scalar::I8, 16) else {
        return Err(SimplifyError::Refused { inst: 0, what: "vector type <16 x i8>" });
    };
    let Some(v4i32) = Ty::vector(Scalar::I32, 4) else {
        return Err(SimplifyError::Refused { inst: 0, what: "vector type <4 x i32>" });
    };

    let use_chunked_i64 =
        matches!(plan.sink, LutSink::ReduceAdd) && plan.lp.acc_ty == Ty::I64;
    let needs_vred = !matches!(plan.sink, LutSink::Store { .. });
    let extra_blocks: usize = if use_chunked_i64 {
        4 // vpre, vchunk, vloop, vred
    } else if needs_vred {
        3 // vpre, vloop, vred
    } else {
        2 // vpre, vloop
    };

    let blocks = view.blocks();
    let len = view.len();
    let insert_at = to_idx(plan.lp.preheader) + 1;
    let vpre_blk = to_u32(insert_at);
    let (vchunk_blk, vloop_blk, vred_blk) = if use_chunked_i64 {
        (to_u32(insert_at + 1), to_u32(insert_at + 2), to_u32(insert_at + 3))
    } else {
        (NONE, to_u32(insert_at + 1), to_u32(insert_at + 2))
    };

    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        blk_map.push(if b < insert_at { to_u32(b) } else { to_u32(b + extra_blocks) });
    }
    let orig_hdr_new = at(&blk_map, to_idx(plan.lp.header));

    let mut func = view.to_func_in(alloc);
    clear_func_code(&mut func);

    // Register the needed NEON intrinsic(s) in canonical table order.
    let tbl_id = if plan.mode.is_64() {
        ensure_intrinsic(&mut func, NEON_TBL64_INTRINSIC, &[v16i8, v16i8, v16i8, v16i8, v16i8], v16i8)
    } else {
        ensure_intrinsic(&mut func, NEON_TBL16_INTRINSIC, &[v16i8, v16i8], v16i8)
    };
    let udot_id = if matches!(plan.sink, LutSink::ReduceAdd) && plan.lp.acc_ty != Ty::I8 {
        Some(ensure_intrinsic(&mut func, NEON_UDOT_INTRINSIC, &[v4i32, v16i8, v16i8], v4i32))
    } else {
        None
    };

    let mut val_map = filled(len, NONE, &alloc);
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));

        if b_u32 != plan.lp.preheader {
            for inst in range {
                let mapped = copy_remapped(view, inst, &val_map, Some(&blk_map), &mut func, &mut fixups);
                set_at(&mut val_map, inst, mapped);
            }
            continue;
        }

        // Copy non-terminator instructions of `preheader`.
        let term = range.end.saturating_sub(1);
        for inst in range.start..term {
            let mapped = copy_remapped(view, inst, &val_map, Some(&blk_map), &mut func, &mut fixups);
            set_at(&mut val_map, inst, mapped);
        }

        let iv_init = at(&val_map, to_idx(plan.lp.iv_init));
        let bound = at(&val_map, to_idx(plan.lp.bound));
        let src_base = at(&val_map, to_idx(plan.src_base));
        let lut_base = at(&val_map, to_idx(plan.lut_base));
        let acc_init = if plan.lp.acc_pos.is_some() {
            at(&val_map, to_idx(plan.lp.acc_init))
        } else {
            NONE
        };

        let has_rem = push_icmp(&mut func, IntCond::Ult, Ty::I1, iv_init, bound);
        let rem = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, bound, iv_init);
        let k64 = push_iconst(&mut func, Ty::I64, 64);
        let enough = push_icmp(&mut func, IntCond::Uge, Ty::I1, rem, k64);
        let mut go_vec = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, enough);

        let src_base_adj = emit_offset_base(&mut func, src_base, plan.src_off);
        let lut_base_adj = emit_offset_base(&mut func, lut_base, plan.lut_off);
        let dst_base_adj = match plan.sink {
            LutSink::Store { dst_base, dst_off } => {
                let db = at(&val_map, to_idx(dst_base));
                let db_adj = emit_offset_base(&mut func, db, dst_off);

                // Runtime alias guard 1: `dst` vs `src` (in-place exact equality is safe, else disjoint).
                let dst_start = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, db_adj, iv_init);
                let src_start = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, src_base_adj, iv_init);
                let dst_addr = push_unary(&mut func, Op::PtrAddr, Ty::I64, dst_start);
                let src_addr = push_unary(&mut func, Op::PtrAddr, Ty::I64, src_start);
                let d_ds = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, dst_addr, src_addr);
                let d_sd = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, src_addr, dst_addr);
                let k0 = push_iconst(&mut func, Ty::I64, 0);
                let same_ptr = push_icmp(&mut func, IntCond::Eq, Ty::I1, d_ds, k0);
                let min_dist = push_bin(&mut func, Op::Umin, Flags::NONE, Ty::I64, d_ds, d_sd);
                let disj_src = push_icmp(&mut func, IntCond::Uge, Ty::I1, min_dist, rem);
                let ok_src = push_bin(&mut func, Op::Or, Flags::NONE, Ty::I1, same_ptr, disj_src);

                // Runtime alias guard 2: `dst[iv_init..bound]` must not overlap `lut[0..table_bytes]`.
                let lut_addr = push_unary(&mut func, Op::PtrAddr, Ty::I64, lut_base_adj);
                let d_dl = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, dst_addr, lut_addr);
                let d_ld = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, lut_addr, dst_addr);
                let k_tbl_bytes = push_iconst(&mut func, Ty::I64, plan.mode.table_bytes());
                let dst_after_lut = push_icmp(&mut func, IntCond::Uge, Ty::I1, d_dl, k_tbl_bytes);
                let lut_after_dst = push_icmp(&mut func, IntCond::Uge, Ty::I1, d_ld, rem);
                let ok_lut = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, dst_after_lut, lut_after_dst);

                let ok_alias = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, ok_src, ok_lut);
                go_vec = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, go_vec, ok_alias);
                db_adj
            }
            LutSink::ReduceAdd | LutSink::ReduceXor => NONE,
        };

        let (orig_args, orig_argc) = ordered_header_args(
            plan.lp.param_count,
            plan.lp.iv_pos,
            iv_init,
            plan.lp.acc_pos,
            acc_init,
        );
        let orig_slice = orig_args.get(..orig_argc).unwrap_or(&[]);
        push_br(&mut func, go_vec, vpre_blk, &[], orig_hdr_new, orig_slice);

        // --- `vpre`: hoist LUT register loads (1 or 4 x 16B) and index mask/shift splat ---
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(block_kind_byte(BlockKind::Normal));
        let kmask64 = push_iconst(&mut func, Ty::I64, !63u64);
        let vcnt = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, rem, kmask64);
        let vend = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_init, vcnt);

        let t0 = push_load(&mut func, v16i8, lut_base_adj, 0, 0);
        let (t1, t2, t3) = if plan.mode.is_64() {
            (
                push_load(&mut func, v16i8, lut_base_adj, 16, 0),
                push_load(&mut func, v16i8, lut_base_adj, 32, 0),
                push_load(&mut func, v16i8, lut_base_adj, 48, 0),
            )
        } else {
            (NONE, NONE, NONE)
        };

        let vidx_imm = match plan.mode {
            LutIndexMode::Mask16 => {
                let c = push_iconst(&mut func, Ty::I8, 15);
                push_unary(&mut func, Op::Splat, v16i8, c)
            }
            LutIndexMode::Mask64 => {
                let c = push_iconst(&mut func, Ty::I8, 63);
                push_unary(&mut func, Op::Splat, v16i8, c)
            }
            LutIndexMode::ShiftRight4 => {
                let c = push_iconst(&mut func, Ty::I8, 4);
                push_unary(&mut func, Op::Splat, v16i8, c)
            }
        };

        let (vzero_acc, vones_u8) = match plan.sink {
            LutSink::Store { .. } => {
                push_jump(&mut func, vloop_blk, &[iv_init]);
                (NONE, NONE)
            }
            LutSink::ReduceXor => {
                let z8 = push_iconst(&mut func, Ty::I8, 0);
                let vz = push_unary(&mut func, Op::Splat, v16i8, z8);
                push_jump(&mut func, vloop_blk, &[iv_init, vz, vz, vz, vz]);
                (vz, NONE)
            }
            LutSink::ReduceAdd if plan.lp.acc_ty == Ty::I8 => {
                let z8 = push_iconst(&mut func, Ty::I8, 0);
                let vz = push_unary(&mut func, Op::Splat, v16i8, z8);
                push_jump(&mut func, vloop_blk, &[iv_init, vz, vz, vz, vz]);
                (vz, NONE)
            }
            LutSink::ReduceAdd => {
                let z32 = push_iconst(&mut func, Ty::I32, 0);
                let vz = push_unary(&mut func, Op::Splat, v4i32, z32);
                let one8 = push_iconst(&mut func, Ty::I8, 1);
                let vones = push_unary(&mut func, Op::Splat, v16i8, one8);
                if use_chunked_i64 {
                    push_jump(&mut func, vchunk_blk, &[iv_init, acc_init]);
                } else {
                    push_jump(&mut func, vloop_blk, &[iv_init, vz, vz, vz, vz]);
                }
                (vz, vones)
            }
        };

        // --- optional `vchunk`: outer 16,384-element chunking for overflow-free `i64` sum ---
        let (inner_end, chunk_acc_param) = if use_chunked_i64 {
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv_c = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
            let acc_c = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
            let rem_c = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, vend, iv_c);
            let kchunk = push_iconst(&mut func, Ty::I64, I64_ACC_CHUNK_ELEMS);
            let step = push_bin(&mut func, Op::Umin, Flags::NONE, Ty::I64, rem_c, kchunk);
            let cend = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_c, step);
            push_jump(&mut func, vloop_blk, &[iv_c, vzero_acc, vzero_acc, vzero_acc, vzero_acc]);
            (cend, acc_c)
        } else {
            (vend, acc_init)
        };

        // --- `vloop`: 4x-unrolled 64B/iter NEON `TBL` kernel ---
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(block_kind_byte(BlockKind::Normal));
        let iv = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
        let acc_vec_ty = if udot_id.is_some() { v4i32 } else { v16i8 };
        let mut vaccs = [NONE; 4];
        if needs_vred {
            for u in 0..4usize {
                set_at(
                    &mut vaccs,
                    u,
                    push_inst(&mut func, Op::Param, Flags::NONE, acc_vec_ty, 0, 0),
                );
            }
        }

        let psrc = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, src_base_adj, iv);
        let pdst = if matches!(plan.sink, LutSink::Store { .. }) {
            push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, dst_base_adj, iv)
        } else {
            NONE
        };

        let mut next_vaccs = [NONE; 4];
        for u in 0..4usize {
            let byte_off = i32::try_from(u * 16).unwrap_or(0);
            let raw = push_load(&mut func, v16i8, psrc, byte_off, 0);
            let idx_v = match plan.mode {
                LutIndexMode::Mask16 | LutIndexMode::Mask64 => {
                    push_bin(&mut func, Op::And, Flags::NONE, v16i8, raw, vidx_imm)
                }
                LutIndexMode::ShiftRight4 => {
                    push_bin(&mut func, Op::Lshr, Flags::NONE, v16i8, raw, vidx_imm)
                }
            };
            let out_v = if plan.mode.is_64() {
                push_target_intrinsic(&mut func, tbl_id, v16i8, &[t0, t1, t2, t3, idx_v])
            } else {
                push_target_intrinsic(&mut func, tbl_id, v16i8, &[t0, idx_v])
            };

            match plan.sink {
                LutSink::Store { .. } => {
                    push_store(&mut func, out_v, pdst, byte_off, 0);
                }
                LutSink::ReduceXor => {
                    let prev = at(&vaccs, u);
                    set_at(&mut next_vaccs, u, push_bin(&mut func, Op::Xor, Flags::NONE, v16i8, prev, out_v));
                }
                LutSink::ReduceAdd => {
                    let prev = at(&vaccs, u);
                    let nv = match udot_id {
                        Some(uid) => push_target_intrinsic(&mut func, uid, v4i32, &[prev, out_v, vones_u8]),
                        None => push_bin(&mut func, Op::Add, Flags::NONE, v16i8, prev, out_v),
                    };
                    set_at(&mut next_vaccs, u, nv);
                }
            }
        }

        let iv_next = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv, k64);
        let keep = push_icmp(&mut func, IntCond::Ult, Ty::I1, iv_next, inner_end);

        if !needs_vred {
            push_br(&mut func, keep, vloop_blk, &[iv_next], orig_hdr_new, &[iv_next]);
        } else {
            let [nv0, nv1, nv2, nv3] = next_vaccs;
            let loop_args = [iv_next, nv0, nv1, nv2, nv3];
            push_br(&mut func, keep, vloop_blk, &loop_args, vred_blk, &loop_args);

            // --- `vred`: horizontal reduction of 4 independent vector accumulators ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv_r = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
            let r0 = push_inst(&mut func, Op::Param, Flags::NONE, acc_vec_ty, 0, 0);
            let r1 = push_inst(&mut func, Op::Param, Flags::NONE, acc_vec_ty, 0, 0);
            let r2 = push_inst(&mut func, Op::Param, Flags::NONE, acc_vec_ty, 0, 0);
            let r3 = push_inst(&mut func, Op::Param, Flags::NONE, acc_vec_ty, 0, 0);

            let (comb_op, red_op) = match plan.sink {
                LutSink::ReduceXor => (Op::Xor, Op::ReduceXor),
                LutSink::ReduceAdd | LutSink::Store { .. } => (Op::Add, Op::ReduceAdd),
            };
            let s01 = push_bin(&mut func, comb_op, Flags::NONE, acc_vec_ty, r0, r1);
            let s23 = push_bin(&mut func, comb_op, Flags::NONE, acc_vec_ty, r2, r3);
            let sall = push_bin(&mut func, comb_op, Flags::NONE, acc_vec_ty, s01, s23);
            let lane_ty = acc_vec_ty.lane();
            let hred = push_unary(&mut func, red_op, lane_ty, sall);
            let widened = if lane_ty == plan.lp.acc_ty {
                hred
            } else {
                push_unary(&mut func, Op::Zext, plan.lp.acc_ty, hred)
            };
            let acc_after = push_bin(&mut func, comb_op, Flags::NONE, plan.lp.acc_ty, chunk_acc_param, widened);

            let (tail_args, tail_argc) = ordered_header_args(
                plan.lp.param_count,
                plan.lp.iv_pos,
                iv_r,
                plan.lp.acc_pos,
                acc_after,
            );
            let tail_slice = tail_args.get(..tail_argc).unwrap_or(&[]);
            if use_chunked_i64 {
                let more = push_icmp(&mut func, IntCond::Ult, Ty::I1, iv_r, vend);
                push_br(&mut func, more, vchunk_blk, &[iv_r, acc_after], orig_hdr_new, tail_slice);
            } else {
                push_jump(&mut func, orig_hdr_new, tail_slice);
            }
        }
    }

    apply_val_fixups(&mut func, &val_map, &fixups);
    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(func)
}

/// Lowers byte-table lookup loops (`dst[i] = lut[src[i] & 15]`, `dst[i] = lut[src[i] & 63]`,
/// high-nibble `lut[src[i] >> 4]`, and LUT reductions) into 4x-unrolled (64 B/iter) NEON `TBL`
/// loops (`"blaze.neon.tbl16"` / `"blaze.neon.tbl64"`) with sound alias guards and scalar tail.
pub fn lower_byte_lut_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<SimdIdiomOutput<A>, SimplifyError> {
    if view.blocks() < 3 || view.head.ptr_bits != 64 {
        return Ok(SimdIdiomOutput {
            func: view.to_func_in(alloc),
            stats: SimdIdiomStats::default(),
        });
    }

    let mut current: Option<Func<A>> = None;
    let mut tbl_loops = 0u32;

    for _ in 0..MAX_LOOPS_PER_FUNC {
        let cur_view = match &current {
            Some(f) => f.view(),
            None => view,
        };
        let cfg = build_cfg_info(&cur_view, &alloc);
        let Some(plan) = find_lut_plan(&cur_view, &cfg) else {
            break;
        };
        let next = apply_lut_plan(&cur_view, plan, alloc)?;
        current = Some(next);
        tbl_loops = tbl_loops.saturating_add(1);
    }

    let func = match current {
        Some(f) => f,
        None => view.to_func_in(alloc),
    };
    Ok(SimdIdiomOutput {
        func,
        stats: SimdIdiomStats {
            tbl_loops,
            dotprod_loops: 0,
            sort_swaps_canonicalized: 0,
        },
    })
}

// ============================================================================
// Idiom 2: `i8`/`u8` Widening Dot-Product (`SDOT` / `UDOT`) Vectorization
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DotSignedness {
    Signed,
    Unsigned,
}

#[derive(Clone, Copy)]
struct DotProdPlan {
    lp: CountedLoop,
    base_a: u32,
    off_a: i32,
    base_b: u32,
    off_b: i32,
    signedness: DotSignedness,
}

/// Unwraps `Sext(i8_load)` or `Zext(i8_load)` (allowing 1 or 2 widening steps of the same kind).
fn unwrap_i8_ext(
    view: &View<'_>,
    cfg: &CfgInfo<impl Allocator>,
    val: u32,
    body: u32,
    want_op: Op,
) -> Option<usize> {
    let mut cur = to_idx(val);
    let mut saw_ext = false;
    for _ in 0..2 {
        if at(&cfg.inst_block, cur) == body && view.op(cur) == want_op {
            saw_ext = true;
            cur = to_idx(at(view.data, cur));
        } else {
            break;
        }
    }
    if !saw_ext || at(&cfg.inst_block, cur) != body || view.op(cur) != Op::Load {
        return None;
    }
    if Ty(at(view.tys, cur)) != Ty::I8 {
        return None;
    }
    Some(cur)
}

fn match_dot_product_term(
    view: &View<'_>,
    cfg: &CfgInfo<impl Allocator>,
    term: u32,
    body: u32,
    acc_ty: Ty,
) -> Option<(usize, usize, DotSignedness)> {
    let ti = to_idx(term);
    if at(&cfg.inst_block, ti) != body {
        return None;
    }

    // Form 1: `Mul(Sext(a), Sext(b))` or `Mul(Zext(a), Zext(b))` directly at `acc_ty`.
    // Form 2 (for `acc_ty == Ty::I64`): `Sext(Mul(Sext(a), Sext(b)))` or `Zext(Mul(Zext(a), Zext(b)))`
    // where the inner `Mul` is `i16` or `i32` (since `i8 * i8` fits in `i16`/`i32` without overflow,
    // and `u8 * u8` fits in `u16`/`u32` without overflow!).
    let (mul_idx, forced_ext) = match view.op(ti) {
        Op::Mul => (ti, None),
        Op::Sext if acc_ty == Ty::I64 => {
            let inner = to_idx(at(view.data, ti));
            let inner_ty = Ty(at(view.tys, inner));
            if at(&cfg.inst_block, inner) != body
                || view.op(inner) != Op::Mul
                || !matches!(inner_ty, Ty::I16 | Ty::I32)
            {
                return None;
            }
            (inner, Some(Op::Sext))
        }
        Op::Zext if acc_ty == Ty::I64 => {
            let inner = to_idx(at(view.data, ti));
            let inner_ty = Ty(at(view.tys, inner));
            if at(&cfg.inst_block, inner) != body
                || view.op(inner) != Op::Mul
                || !matches!(inner_ty, Ty::I16 | Ty::I32)
            {
                return None;
            }
            (inner, Some(Op::Zext))
        }
        _ => return None,
    };

    let mat = to_idx(at(view.data, mul_idx));
    let (ml, mr) = (at(view.extra, mat), at(view.extra, mat + 1));

    if forced_ext != Some(Op::Zext)
        && let Some(la) = unwrap_i8_ext(view, cfg, ml, body, Op::Sext)
        && let Some(lb) = unwrap_i8_ext(view, cfg, mr, body, Op::Sext)
    {
        return Some((la, lb, DotSignedness::Signed));
    }
    if forced_ext != Some(Op::Sext)
        && let Some(la) = unwrap_i8_ext(view, cfg, ml, body, Op::Zext)
        && let Some(lb) = unwrap_i8_ext(view, cfg, mr, body, Op::Zext)
    {
        return Some((la, lb, DotSignedness::Unsigned));
    }
    None
}

fn find_dotprod_plan(view: &View<'_>, cfg: &CfgInfo<impl Allocator>) -> Option<DotProdPlan> {
    for h in 1..view.blocks() {
        let Some(lp) = match_counted_loop(view, cfg, to_u32(h)) else {
            continue;
        };
        if lp.param_count != 2 || !matches!(lp.acc_ty, Ty::I32 | Ty::I64) {
            continue;
        }
        let upd = to_idx(lp.acc_next);
        if at(&cfg.inst_block, upd) != lp.body || view.op(upd) != Op::Add {
            continue;
        }
        let uat = to_idx(at(view.data, upd));
        let (ul, ur) = (at(view.extra, uat), at(view.extra, uat + 1));
        let prod_v = if ul == lp.acc_inst {
            ur
        } else if ur == lp.acc_inst {
            ul
        } else {
            continue;
        };

        let Some((load_a, load_b, signedness)) =
            match_dot_product_term(view, cfg, prod_v, lp.body, lp.acc_ty)
        else {
            continue;
        };

        let Some((base_a, off_a)) =
            match_unit_i8_load(view, cfg, load_a, lp.body, lp.iv_inst, lp.preheader)
        else {
            continue;
        };
        let Some((base_b, off_b)) =
            match_unit_i8_load(view, cfg, load_b, lp.body, lp.iv_inst, lp.preheader)
        else {
            continue;
        };

        let b_range = view.block(Block(lp.body));
        let b_term = b_range.end.saturating_sub(1);
        let body_ok = (b_range.start..b_term).all(|k| {
            k == load_a
                || k == load_b
                || matches!(
                    view.op(k),
                    Op::PtrAdd | Op::Sext | Op::Zext | Op::Mul | Op::Add | Op::Iconst | Op::IconstWide
                )
        });
        if !body_ok {
            continue;
        }

        return Some(DotProdPlan {
            lp,
            base_a,
            off_a,
            base_b,
            off_b,
            signedness,
        });
    }
    None
}

fn apply_dotprod_plan<A: Allocator + Copy>(
    view: &View<'_>,
    plan: DotProdPlan,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let Some(v16i8) = Ty::vector(Scalar::I8, 16) else {
        return Err(SimplifyError::Refused { inst: 0, what: "vector type <16 x i8>" });
    };
    let Some(v4i32) = Ty::vector(Scalar::I32, 4) else {
        return Err(SimplifyError::Refused { inst: 0, what: "vector type <4 x i32>" });
    };

    let use_chunked_i64 = plan.lp.acc_ty == Ty::I64;
    let extra_blocks: usize = if use_chunked_i64 {
        4 // vpre, vchunk, vloop, vred
    } else {
        3 // vpre, vloop, vred
    };

    let blocks = view.blocks();
    let len = view.len();
    let insert_at = to_idx(plan.lp.preheader) + 1;
    let vpre_blk = to_u32(insert_at);
    let (vchunk_blk, vloop_blk, vred_blk) = if use_chunked_i64 {
        (to_u32(insert_at + 1), to_u32(insert_at + 2), to_u32(insert_at + 3))
    } else {
        (NONE, to_u32(insert_at + 1), to_u32(insert_at + 2))
    };

    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        blk_map.push(if b < insert_at { to_u32(b) } else { to_u32(b + extra_blocks) });
    }
    let orig_hdr_new = at(&blk_map, to_idx(plan.lp.header));

    let mut func = view.to_func_in(alloc);
    clear_func_code(&mut func);

    let dot_name = match plan.signedness {
        DotSignedness::Signed => NEON_SDOT_INTRINSIC,
        DotSignedness::Unsigned => NEON_UDOT_INTRINSIC,
    };
    let dot_id = ensure_intrinsic(&mut func, dot_name, &[v4i32, v16i8, v16i8], v4i32);

    let mut val_map = filled(len, NONE, &alloc);
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));

        if b_u32 != plan.lp.preheader {
            for inst in range {
                let mapped = copy_remapped(view, inst, &val_map, Some(&blk_map), &mut func, &mut fixups);
                set_at(&mut val_map, inst, mapped);
            }
            continue;
        }

        let term = range.end.saturating_sub(1);
        for inst in range.start..term {
            let mapped = copy_remapped(view, inst, &val_map, Some(&blk_map), &mut func, &mut fixups);
            set_at(&mut val_map, inst, mapped);
        }

        let iv_init = at(&val_map, to_idx(plan.lp.iv_init));
        let bound = at(&val_map, to_idx(plan.lp.bound));
        let acc_init = at(&val_map, to_idx(plan.lp.acc_init));
        let base_a = at(&val_map, to_idx(plan.base_a));
        let base_b = at(&val_map, to_idx(plan.base_b));

        let has_rem = push_icmp(&mut func, IntCond::Ult, Ty::I1, iv_init, bound);
        let rem = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, bound, iv_init);
        let k64 = push_iconst(&mut func, Ty::I64, 64);
        let enough = push_icmp(&mut func, IntCond::Uge, Ty::I1, rem, k64);
        let go_vec = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, enough);

        let (orig_args, orig_argc) = ordered_header_args(
            plan.lp.param_count,
            plan.lp.iv_pos,
            iv_init,
            plan.lp.acc_pos,
            acc_init,
        );
        let orig_slice = orig_args.get(..orig_argc).unwrap_or(&[]);
        push_br(&mut func, go_vec, vpre_blk, &[], orig_hdr_new, orig_slice);

        // --- `vpre`: compute `vend` and initialize `<4 x i32>` zero accumulator ---
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(block_kind_byte(BlockKind::Normal));
        let kmask64 = push_iconst(&mut func, Ty::I64, !63u64);
        let vcnt = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, rem, kmask64);
        let vend = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_init, vcnt);
        let base_a_adj = emit_offset_base(&mut func, base_a, plan.off_a);
        let same_stream = plan.base_a == plan.base_b && plan.off_a == plan.off_b;
        let base_b_adj = if same_stream {
            base_a_adj
        } else {
            emit_offset_base(&mut func, base_b, plan.off_b)
        };
        let z32 = push_iconst(&mut func, Ty::I32, 0);
        let vzero = push_unary(&mut func, Op::Splat, v4i32, z32);
        if use_chunked_i64 {
            push_jump(&mut func, vchunk_blk, &[iv_init, acc_init]);
        } else {
            push_jump(&mut func, vloop_blk, &[iv_init, vzero, vzero, vzero, vzero]);
        }

        // --- optional `vchunk`: 16,384-element chunking prevents 32-bit lane overflow for `i64` ---
        let (inner_end, chunk_acc_param) = if use_chunked_i64 {
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let iv_c = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
            let acc_c = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
            let rem_c = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, vend, iv_c);
            let kchunk = push_iconst(&mut func, Ty::I64, I64_ACC_CHUNK_ELEMS);
            let step = push_bin(&mut func, Op::Umin, Flags::NONE, Ty::I64, rem_c, kchunk);
            let cend = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_c, step);
            push_jump(&mut func, vloop_blk, &[iv_c, vzero, vzero, vzero, vzero]);
            (cend, acc_c)
        } else {
            (vend, acc_init)
        };

        // --- `vloop`: 4x-unrolled 64 MACs/iter `sdot.4s` / `udot.4s` kernel ---
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(block_kind_byte(BlockKind::Normal));
        let iv = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
        let va0 = push_inst(&mut func, Op::Param, Flags::NONE, v4i32, 0, 0);
        let va1 = push_inst(&mut func, Op::Param, Flags::NONE, v4i32, 0, 0);
        let va2 = push_inst(&mut func, Op::Param, Flags::NONE, v4i32, 0, 0);
        let va3 = push_inst(&mut func, Op::Param, Flags::NONE, v4i32, 0, 0);
        let vaccs = [va0, va1, va2, va3];

        let pa = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, base_a_adj, iv);
        let pb = if same_stream {
            pa
        } else {
            push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, base_b_adj, iv)
        };

        // Load adjacent 16B vectors in pairs so backend fuses into `ldp q, q`.
        let mut loaded_a = [NONE; 4];
        let mut loaded_b = [NONE; 4];
        for u in 0..4usize {
            let byte_off = i32::try_from(u * 16).unwrap_or(0);
            let la = push_load(&mut func, v16i8, pa, byte_off, 0);
            set_at(&mut loaded_a, u, la);
            if same_stream {
                set_at(&mut loaded_b, u, la);
            }
        }
        if !same_stream {
            for u in 0..4usize {
                let byte_off = i32::try_from(u * 16).unwrap_or(0);
                set_at(&mut loaded_b, u, push_load(&mut func, v16i8, pb, byte_off, 0));
            }
        }

        let mut next_vaccs = [NONE; 4];
        for u in 0..4usize {
            let acc_u = at(&vaccs, u);
            let a_u = at(&loaded_a, u);
            let b_u = at(&loaded_b, u);
            set_at(
                &mut next_vaccs,
                u,
                push_target_intrinsic(&mut func, dot_id, v4i32, &[acc_u, a_u, b_u]),
            );
        }

        let iv_next = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv, k64);
        let keep = push_icmp(&mut func, IntCond::Ult, Ty::I1, iv_next, inner_end);
        let [nv0, nv1, nv2, nv3] = next_vaccs;
        let loop_args = [iv_next, nv0, nv1, nv2, nv3];
        push_br(&mut func, keep, vloop_blk, &loop_args, vred_blk, &loop_args);

        // --- `vred`: sum 4 independent `<4 x i32>` accumulators + horizontal `ReduceAdd` ---
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(block_kind_byte(BlockKind::Normal));
        let iv_r = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
        let r0 = push_inst(&mut func, Op::Param, Flags::NONE, v4i32, 0, 0);
        let r1 = push_inst(&mut func, Op::Param, Flags::NONE, v4i32, 0, 0);
        let r2 = push_inst(&mut func, Op::Param, Flags::NONE, v4i32, 0, 0);
        let r3 = push_inst(&mut func, Op::Param, Flags::NONE, v4i32, 0, 0);

        let s01 = push_bin(&mut func, Op::Add, Flags::NONE, v4i32, r0, r1);
        let s23 = push_bin(&mut func, Op::Add, Flags::NONE, v4i32, r2, r3);
        let sall = push_bin(&mut func, Op::Add, Flags::NONE, v4i32, s01, s23);
        let sum32 = push_unary(&mut func, Op::ReduceAdd, Ty::I32, sall);
        let sum_acc = if use_chunked_i64 {
            let ext_op = match plan.signedness {
                DotSignedness::Signed => Op::Sext,
                DotSignedness::Unsigned => Op::Zext,
            };
            push_unary(&mut func, ext_op, Ty::I64, sum32)
        } else {
            sum32
        };
        let acc_after = push_bin(&mut func, Op::Add, Flags::NONE, plan.lp.acc_ty, chunk_acc_param, sum_acc);

        let (tail_args, tail_argc) = ordered_header_args(
            plan.lp.param_count,
            plan.lp.iv_pos,
            iv_r,
            plan.lp.acc_pos,
            acc_after,
        );
        let tail_slice = tail_args.get(..tail_argc).unwrap_or(&[]);
        if use_chunked_i64 {
            let more = push_icmp(&mut func, IntCond::Ult, Ty::I1, iv_r, vend);
            push_br(&mut func, more, vchunk_blk, &[iv_r, acc_after], orig_hdr_new, tail_slice);
        } else {
            push_jump(&mut func, orig_hdr_new, tail_slice);
        }
    }

    apply_val_fixups(&mut func, &val_map, &fixups);
    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(func)
}

/// Lowers widening `i8`/`u8` dot-product loops into 4x-unrolled (64 MACs/iter) NEON `SDOT`/`UDOT`
/// loops (`"blaze.neon.sdot"` / `"blaze.neon.udot"`) with 4 independent `<4 x i32>` accumulators,
/// horizontal `Op::ReduceAdd`, overflow-free chunking for `i64`, and scalar tail.
pub fn lower_dotprod_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<SimdIdiomOutput<A>, SimplifyError> {
    if view.blocks() < 3 || view.head.ptr_bits != 64 {
        return Ok(SimdIdiomOutput {
            func: view.to_func_in(alloc),
            stats: SimdIdiomStats::default(),
        });
    }

    let mut current: Option<Func<A>> = None;
    let mut dotprod_loops = 0u32;

    for _ in 0..MAX_LOOPS_PER_FUNC {
        let cur_view = match &current {
            Some(f) => f.view(),
            None => view,
        };
        let cfg = build_cfg_info(&cur_view, &alloc);
        let Some(plan) = find_dotprod_plan(&cur_view, &cfg) else {
            break;
        };
        let next = apply_dotprod_plan(&cur_view, plan, alloc)?;
        current = Some(next);
        dotprod_loops = dotprod_loops.saturating_add(1);
    }

    let func = match current {
        Some(f) => f,
        None => view.to_func_in(alloc),
    };
    Ok(SimdIdiomOutput {
        func,
        stats: SimdIdiomStats {
            tbl_loops: 0,
            dotprod_loops,
            sort_swaps_canonicalized: 0,
        },
    })
}

// ============================================================================
// Idiom 3: Branchless Compare-Exchange Sorting Networks
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct OrderCond {
    lhs: u32,
    rhs: u32,
    signed: bool,
    /// True if `cond` holds when `lhs < rhs` (`Slt`/`Sle`/`Ult`/`Ule`).
    lt_when_true: bool,
}

fn decode_order_cond(view: &View<'_>, cond_val: u32) -> Option<OrderCond> {
    let mut cur = to_idx(cond_val);
    let mut flip = false;

    // Unwrap a single boolean negation (`Not(c)` or `Xor(c, 1)`).
    match view.op(cur) {
        Op::Not => {
            cur = to_idx(at(view.data, cur));
            flip = true;
        }
        Op::Xor => {
            let xat = to_idx(at(view.data, cur));
            let (l, r) = (at(view.extra, xat), at(view.extra, xat + 1));
            if const_u64(view, to_idx(r)) == Some(1) {
                cur = to_idx(l);
                flip = true;
            } else if const_u64(view, to_idx(l)) == Some(1) {
                cur = to_idx(r);
                flip = true;
            }
        }
        _ => {}
    }

    if view.op(cur) != Op::Icmp {
        return None;
    }
    let cat = to_idx(at(view.data, cur));
    let ic = IntCond::from_word(at(view.extra, cat))?;
    let lhs = at(view.extra, cat + 1);
    let rhs = at(view.extra, cat + 2);
    let op_ty = Ty(at(view.tys, to_idx(lhs)));
    if !op_ty.is_int() || op_ty.elem() == Scalar::I1 || op_ty.compound_row().is_some() {
        return None;
    }

    let (signed, mut lt_when_true) = match ic {
        IntCond::Slt | IntCond::Sle => (true, true),
        IntCond::Sgt | IntCond::Sge => (true, false),
        IntCond::Ult | IntCond::Ule => (false, true),
        IntCond::Ugt | IntCond::Uge => (false, false),
        IntCond::Eq | IntCond::Ne => return None,
    };
    if flip {
        lt_when_true = !lt_when_true;
    }
    Some(OrderCond { lhs, rhs, signed, lt_when_true })
}

#[inline]
fn minmax_op(ord: OrderCond, t_val: u32, f_val: u32) -> Option<Op> {
    let pick_min = if t_val == ord.lhs && f_val == ord.rhs {
        ord.lt_when_true
    } else if t_val == ord.rhs && f_val == ord.lhs {
        !ord.lt_when_true
    } else {
        return None;
    };
    Some(match (ord.signed, pick_min) {
        (true, true) => Op::Smin,
        (true, false) => Op::Smax,
        (false, true) => Op::Umin,
        (false, false) => Op::Umax,
    })
}

#[derive(Clone, Copy)]
struct SelectRewrite {
    op: Op,
    lhs: u32,
    rhs: u32,
}

#[derive(Clone, Copy)]
enum DiamondArg {
    PassThrough(u32),
    Min,
    Max,
}

struct DiamondRewrite<A: Allocator> {
    merge_blk: u32,
    lhs: u32,
    rhs: u32,
    elem_ty: Ty,
    min_op: Op,
    max_op: Op,
    need_min: bool,
    need_max: bool,
    args: Vec<DiamondArg, A>,
}

/// Resolves a branch target `(blk, args_start, args_len)` through at most one empty trampoline
/// block (0 params, 1 predecessor, containing only `Op::Jump`).
fn resolve_trampoline(
    view: &View<'_>,
    preds: &[Vec<u32, impl Allocator>],
    blk: u32,
    args_start: usize,
    args_len: usize,
) -> (u32, usize, usize) {
    let b_idx = to_idx(blk);
    if args_len == 0
        && preds.get(b_idx).map(Vec::len) == Some(1)
        && view.block_kind(Block(blk)) == BlockKind::Normal
        && view.params(Block(blk)).is_empty()
    {
        let range = view.block(Block(blk));
        if range.len() == 1 && view.op(range.start) == Op::Jump {
            let jat = to_idx(at(view.data, range.start));
            let dest = at(view.extra, jat);
            let n = to_idx(at(view.extra, jat + 1));
            return (dest, jat + 2, n);
        }
    }
    (blk, args_start, args_len)
}

fn match_compare_swap_diamond<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &CfgInfo<A>,
    blk: u32,
    alloc: A,
) -> Option<DiamondRewrite<A>> {
    let term = view.terminator(Block(blk));
    if view.op(term) != Op::Br {
        return None;
    }
    let bat = to_idx(at(view.data, term));
    let cond_v = at(view.extra, bat);
    let ord = decode_order_cond(view, cond_v)?;
    let elem_ty = Ty(at(view.tys, to_idx(ord.lhs)));
    if !elem_ty.is_scalar() {
        return None;
    }

    let t_blk = at(view.extra, bat + 1);
    let t_argc = to_idx(at(view.extra, bat + 2));
    let t_args_at = bat + 3;
    let f_blk = at(view.extra, t_args_at + t_argc);
    let f_argc = to_idx(at(view.extra, t_args_at + t_argc + 1));
    let f_args_at = t_args_at + t_argc + 2;

    let (t_dest, t_at, t_n) = resolve_trampoline(view, &cfg.preds, t_blk, t_args_at, t_argc);
    let (f_dest, f_at, f_n) = resolve_trampoline(view, &cfg.preds, f_blk, f_args_at, f_argc);
    if t_dest != f_dest || t_n != f_n || t_n == 0 {
        return None;
    }

    // Verify `lhs` and `rhs` dominate `blk` (always true for `Icmp` dominating `Br`).
    if !dominates(&cfg.idom, at(&cfg.inst_block, to_idx(ord.lhs)), blk)
        || !dominates(&cfg.idom, at(&cfg.inst_block, to_idx(ord.rhs)), blk)
    {
        return None;
    }

    let (min_op, max_op) = if ord.signed {
        (Op::Smin, Op::Smax)
    } else {
        (Op::Umin, Op::Umax)
    };

    let mut args: Vec<DiamondArg, A> = Vec::with_capacity_in(t_n, alloc);
    let mut need_min = false;
    let mut need_max = false;
    for k in 0..t_n {
        let tv = at(view.extra, t_at + k);
        let fv = at(view.extra, f_at + k);
        if tv == fv {
            if !dominates(&cfg.idom, at(&cfg.inst_block, to_idx(tv)), blk) {
                return None;
            }
            args.push(DiamondArg::PassThrough(tv));
        } else if let Some(op) = minmax_op(ord, tv, fv) {
            if op == min_op {
                need_min = true;
                args.push(DiamondArg::Min);
            } else {
                need_max = true;
                args.push(DiamondArg::Max);
            }
        } else {
            return None;
        }
    }

    if !need_min && !need_max {
        return None;
    }

    Some(DiamondRewrite {
        merge_blk: t_dest,
        lhs: ord.lhs,
        rhs: ord.rhs,
        elem_ty,
        min_op,
        max_op,
        need_min,
        need_max,
        args,
    })
}

/// Canonicalizes `Select`-based compare-swap pairs and conditional compare-and-swap CFG diamonds
/// into pure branchless `Smin`/`Smax` or `Umin`/`Umax` operations, then runs `dead` to collapse
/// now-trivial branches and coalesce straight-line sorting networks into a single basic block.
pub fn lower_sorting_networks<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<SimdIdiomOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks == 0 || len == 0 {
        return Ok(SimdIdiomOutput {
            func: view.to_func_in(alloc),
            stats: SimdIdiomStats::default(),
        });
    }

    let cfg = build_cfg_info(&view, &alloc);

    // 1. Detect `Select(Icmp(ord, a, b), a, b)` / `Select(Icmp(ord, a, b), b, a)` rewrites.
    let mut sel_rewrites: Vec<Option<SelectRewrite>, A> = filled(len, None, &alloc);
    let mut sel_pairs: Vec<(u32, u32, bool), A> = Vec::new_in(alloc);
    let mut sel_swaps = 0u32;

    for inst in 0..len {
        if view.op(inst) != Op::Select {
            continue;
        }
        let sat = to_idx(at(view.data, inst));
        let cond_v = at(view.extra, sat);
        let t_val = at(view.extra, sat + 1);
        let f_val = at(view.extra, sat + 2);
        let Some(ord) = decode_order_cond(&view, cond_v) else {
            continue;
        };
        let Some(op) = minmax_op(ord, t_val, f_val) else {
            continue;
        };
        let sel_ty = Ty(at(view.tys, inst));
        if sel_ty != Ty(at(view.tys, to_idx(ord.lhs))) {
            continue;
        }
        set_at(
            &mut sel_rewrites,
            inst,
            Some(SelectRewrite { op, lhs: ord.lhs, rhs: ord.rhs }),
        );

        // Pair `(min(a, b), max(a, b))` on the same unordered `(a, b)` as 1 compare-swap!
        let (k0, k1) = if ord.lhs <= ord.rhs {
            (ord.lhs, ord.rhs)
        } else {
            (ord.rhs, ord.lhs)
        };
        let is_min = matches!(op, Op::Smin | Op::Umin);
        let mut paired = false;
        for entry in sel_pairs.iter_mut() {
            if entry.0 == k0 && entry.1 == k1 && entry.2 != is_min {
                entry.0 = NONE;
                entry.1 = NONE;
                paired = true;
                break;
            }
        }
        if !paired {
            sel_pairs.push((k0, k1, is_min));
            sel_swaps = sel_swaps.saturating_add(1);
        }
    }

    // 2. Detect conditional compare-and-swap CFG diamonds (`if a > b { swap(a, b) }`).
    let mut diamond_rewrites: Vec<Option<DiamondRewrite<A>>, A> = Vec::with_capacity_in(blocks, alloc);
    let mut diamond_swaps = 0u32;
    for b in 0..blocks {
        let plan = match_compare_swap_diamond(&view, &cfg, to_u32(b), alloc);
        if plan.is_some() {
            diamond_swaps = diamond_swaps.saturating_add(1);
        }
        diamond_rewrites.push(plan);
    }

    let total_swaps = sel_swaps.saturating_add(diamond_swaps);
    if total_swaps == 0 {
        return Ok(SimdIdiomOutput {
            func: view.to_func_in(alloc),
            stats: SimdIdiomStats::default(),
        });
    }

    let mut func = view.to_func_in(alloc);
    clear_func_code(&mut func);

    let mut val_map = filled(len, NONE, &alloc);
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));
        let term = range.end.saturating_sub(1);

        for inst in range.start..term {
            let mapped = if let Some(sr) = sel_rewrites.get(inst).copied().flatten() {
                let ty = Ty(at(view.tys, inst));
                let l_mapped = at(&val_map, to_idx(sr.lhs));
                let r_mapped = at(&val_map, to_idx(sr.rhs));
                let idx = to_u32(func.extra.len());
                if l_mapped == NONE {
                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: sr.lhs });
                    func.extra.push(0);
                } else {
                    func.extra.push(l_mapped);
                }
                if r_mapped == NONE {
                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: sr.rhs });
                    func.extra.push(0);
                } else {
                    func.extra.push(r_mapped);
                }
                push_inst(&mut func, sr.op, Flags::NONE, ty, idx, 0)
            } else {
                copy_remapped(&view, inst, &val_map, None, &mut func, &mut fixups)
            };
            set_at(&mut val_map, inst, mapped);
        }

        if let Some(Some(dr)) = diamond_rewrites.get(b) {
            let l_mapped = at(&val_map, to_idx(dr.lhs));
            let r_mapped = at(&val_map, to_idx(dr.rhs));
            let lo_v = if dr.need_min {
                let idx = to_u32(func.extra.len());
                if l_mapped == NONE {
                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: dr.lhs });
                    func.extra.push(0);
                } else {
                    func.extra.push(l_mapped);
                }
                if r_mapped == NONE {
                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: dr.rhs });
                    func.extra.push(0);
                } else {
                    func.extra.push(r_mapped);
                }
                push_inst(&mut func, dr.min_op, Flags::NONE, dr.elem_ty, idx, 0)
            } else {
                NONE
            };
            let hi_v = if dr.need_max {
                let idx = to_u32(func.extra.len());
                if l_mapped == NONE {
                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: dr.lhs });
                    func.extra.push(0);
                } else {
                    func.extra.push(l_mapped);
                }
                if r_mapped == NONE {
                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: dr.rhs });
                    func.extra.push(0);
                } else {
                    func.extra.push(r_mapped);
                }
                push_inst(&mut func, dr.max_op, Flags::NONE, dr.elem_ty, idx, 0)
            } else {
                NONE
            };

            let j_idx = to_u32(func.extra.len());
            func.extra.push(dr.merge_blk);
            func.extra.push(to_u32(dr.args.len()));
            for &arg in dr.args.iter() {
                match arg {
                    DiamondArg::PassThrough(old_v) => {
                        let m = at(&val_map, to_idx(old_v));
                        if m == NONE {
                            fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old_v });
                            func.extra.push(0);
                        } else {
                            func.extra.push(m);
                        }
                    }
                    DiamondArg::Min => func.extra.push(lo_v),
                    DiamondArg::Max => func.extra.push(hi_v),
                }
            }
            let mapped = push_inst(&mut func, Op::Jump, Flags::NONE, Ty::NONE, j_idx, 0);
            set_at(&mut val_map, term, mapped);
        } else {
            let mapped = copy_remapped(&view, term, &val_map, None, &mut func, &mut fixups);
            set_at(&mut val_map, term, mapped);
        }
    }

    apply_val_fixups(&mut func, &val_map, &fixups);
    verify(func.view()).map_err(SimplifyError::Verify)?;
    let cleaned = dead(func.view(), alloc)?;

    Ok(SimdIdiomOutput {
        func: cleaned,
        stats: SimdIdiomStats {
            tbl_loops: 0,
            dotprod_loops: 0,
            sort_swaps_canonicalized: total_swaps,
        },
    })
}

/// Emits a single branchless compare-exchange `(min(a, b), max(a, b))` into `b`.
#[inline]
fn emit_cas<A: Allocator + Copy, S: Sink>(
    b: &mut Builder<A, S>,
    a: Value,
    c: Value,
    signed: bool,
) -> (Value, Value) {
    let (min_op, max_op) = if signed {
        (Op::Smin, Op::Smax)
    } else {
        (Op::Umin, Op::Umax)
    };
    let lo = b.binary(min_op, Flags::NONE, a, c);
    let hi = b.binary(max_op, Flags::NONE, a, c);
    (lo, hi)
}

/// Emits the optimal 5-comparator (depth-3) branchless sorting network on 4 integer scalar or
/// vector SSA values into `b`.
pub fn emit_sort4_values<A: Allocator + Copy, S: Sink>(
    b: &mut Builder<A, S>,
    vals: [Value; 4],
    signed: bool,
) -> [Value; 4] {
    let [v0, v1, v2, v3] = vals;
    // Layer 1: (0, 1), (2, 3)
    let (a0, a1) = emit_cas(b, v0, v1, signed);
    let (a2, a3) = emit_cas(b, v2, v3, signed);
    // Layer 2: (0, 2), (1, 3)
    let (b0, b2) = emit_cas(b, a0, a2, signed);
    let (b1, b3) = emit_cas(b, a1, a3, signed);
    // Layer 3: (1, 2)
    let (c1, c2) = emit_cas(b, b1, b2, signed);
    [b0, c1, c2, b3]
}

/// Emits the optimal 19-comparator (depth-6) Batcher odd-even merge branchless sorting network on
/// 8 integer scalar or vector SSA values into `b`.
pub fn emit_sort8_values<A: Allocator + Copy, S: Sink>(
    b: &mut Builder<A, S>,
    vals: [Value; 8],
    signed: bool,
) -> [Value; 8] {
    let [v0, v1, v2, v3, v4, v5, v6, v7] = vals;
    // Layers 1-3: sort [0..4] and [4..8] in parallel (10 comparators, depth 3).
    let [a0, a1, a2, a3] = emit_sort4_values(b, [v0, v1, v2, v3], signed);
    let [a4, a5, a6, a7] = emit_sort4_values(b, [v4, v5, v6, v7], signed);
    // Layer 4: merge evens and odds across halves (4 comparators).
    let (b0, b4) = emit_cas(b, a0, a4, signed);
    let (b1, b5) = emit_cas(b, a1, a5, signed);
    let (b2, b6) = emit_cas(b, a2, a6, signed);
    let (b3, b7) = emit_cas(b, a3, a7, signed);
    // Layer 5: finish sorting evens {0,2,4,6} and odds {1,3,5,7} (2 comparators).
    let (c2, c4) = emit_cas(b, b2, b4, signed);
    let (c3, c5) = emit_cas(b, b3, b5, signed);
    // Layer 6: resolve adjacent odd-even inversions (1,2), (3,4), (5,6) (3 comparators).
    let (d1, d2) = emit_cas(b, b1, c2, signed);
    let (d3, d4) = emit_cas(b, c3, c4, signed);
    let (d5, d6) = emit_cas(b, c5, b6, signed);
    [b0, d1, d2, d3, d4, d5, d6, b7]
}

fn check_sort_elem_ty(elem_ty: Ty) -> Result<(), SimplifyError> {
    if !elem_ty.is_valid()
        || elem_ty.is_none()
        || !elem_ty.is_int()
        || elem_ty.elem() == Scalar::I1
        || elem_ty.compound_row().is_some()
    {
        return Err(SimplifyError::Refused {
            inst: 0,
            what: "a non-integer element type for branchless sorting network",
        });
    }
    Ok(())
}

/// Builds a verified standalone branchless 4-element sorting network LIR function
/// `fn(x0, x1, x2, x3) -> (y0, y1, y2, y3)` of integer scalar or SIMD vector type `elem_ty`
/// using 5 compare-exchange stages (`Smin`/`Smax` or `Umin`/`Umax`, depth 3, zero branches).
pub fn emit_sort4_network<A: Allocator + Copy>(
    elem_ty: Ty,
    signed: bool,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    check_sort_elem_ty(elem_ty)?;
    let mut b = Builder::<A, Full>::new_in(64, alloc);
    let p = Param::of(elem_ty);
    let sig = b.sig(Conv::C, Variadic::No, &[p, p, p, p], &[p, p, p, p]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let v0 = b.param(elem_ty);
    let v1 = b.param(elem_ty);
    let v2 = b.param(elem_ty);
    let v3 = b.param(elem_ty);
    let sorted = emit_sort4_values(&mut b, [v0, v1, v2, v3], signed);
    b.ret(&sorted);
    b.finish_checked().map_err(SimplifyError::Build)
}

/// Builds a verified standalone branchless 8-element sorting network LIR function
/// `fn(x0..x7) -> (y0..y7)` of integer scalar or SIMD vector type `elem_ty`
/// using 19 compare-exchange stages (Batcher odd-even merge, depth 6, zero branches).
pub fn emit_sort8_network<A: Allocator + Copy>(
    elem_ty: Ty,
    signed: bool,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    check_sort_elem_ty(elem_ty)?;
    let mut b = Builder::<A, Full>::new_in(64, alloc);
    let p = Param::of(elem_ty);
    let params = [p; 8];
    let sig = b.sig(Conv::C, Variadic::No, &params, &params);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let v0 = b.param(elem_ty);
    let v1 = b.param(elem_ty);
    let v2 = b.param(elem_ty);
    let v3 = b.param(elem_ty);
    let v4 = b.param(elem_ty);
    let v5 = b.param(elem_ty);
    let v6 = b.param(elem_ty);
    let v7 = b.param(elem_ty);
    let sorted = emit_sort8_values(&mut b, [v0, v1, v2, v3, v4, v5, v6, v7], signed);
    b.ret(&sorted);
    b.finish_checked().map_err(SimplifyError::Build)
}

// ============================================================================
// Top-Level Entrypoint
// ============================================================================

/// Runs all enabled SIMD data-parallel idiom lowerings (`lower_sorting_networks`,
/// `lower_byte_lut_loops`, `lower_dotprod_loops`) in deterministic order and returns the verified
/// transformed function plus telemetry.
pub fn lower_simd_idioms<A: Allocator + Copy>(
    view: View<'_>,
    opts: SimdIdiomOpts,
    alloc: A,
) -> Result<SimdIdiomOutput<A>, SimplifyError> {
    let mut current: Option<Func<A>> = None;
    let mut stats = SimdIdiomStats::default();

    if opts.enable_sort_net {
        let cur_view = match &current {
            Some(f) => f.view(),
            None => view,
        };
        let out = lower_sorting_networks(cur_view, alloc)?;
        if out.stats.sort_swaps_canonicalized > 0 {
            stats.sort_swaps_canonicalized = out.stats.sort_swaps_canonicalized;
            current = Some(out.func);
        }
    }

    if opts.enable_tbl {
        let cur_view = match &current {
            Some(f) => f.view(),
            None => view,
        };
        let out = lower_byte_lut_loops(cur_view, alloc)?;
        if out.stats.tbl_loops > 0 {
            stats.tbl_loops = out.stats.tbl_loops;
            current = Some(out.func);
        }
    }

    if opts.enable_dotprod {
        let cur_view = match &current {
            Some(f) => f.view(),
            None => view,
        };
        let out = lower_dotprod_loops(cur_view, alloc)?;
        if out.stats.dotprod_loops > 0 {
            stats.dotprod_loops = out.stats.dotprod_loops;
            current = Some(out.func);
        }
    }

    let func = match current {
        Some(f) => f,
        None => view.to_func_in(alloc),
    };
    Ok(SimdIdiomOutput { func, stats })
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::MemAttrs,
        eval::{self, Outcome, Val},
    };

    fn has_intrinsic(view: &View<'_>, name: &str) -> bool {
        let want = name.as_bytes();
        view.intrinsics.iter().any(|rec| {
            let start = rec.name_first as usize;
            let end = start + rec.name_len as usize;
            view.names.get(start..end) == Some(want)
        })
    }

    fn count_intrinsic_calls(view: &View<'_>, name: &str) -> usize {
        let want = name.as_bytes();
        (0..view.len())
            .filter(|&i| {
                if view.op(i) != Op::TargetIntrinsic {
                    return false;
                }
                let id = view.extra[view.data[i] as usize] as usize;
                let Some(rec) = view.intrinsics.get(id) else {
                    return false;
                };
                let start = rec.name_first as usize;
                let end = start + rec.name_len as usize;
                view.names.get(start..end) == Some(want)
            })
            .count()
    }

    fn build_byte_lut_map_loop(mask: u128) -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[],
        );
        b.set_signature(sig);
        let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let lut = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[z]));

        b.begin(hdr);
        let i = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let psrc = b.ptr_add(Flags::NONE, src, i);
        let byte = b.load(Ty::I8, Flags::NONE, psrc, 0, MemAttrs::default());
        let kmask = b.iconst(Ty::I8, mask);
        let masked = b.binary(Op::And, Flags::NONE, byte, kmask);
        let idx64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, masked);
        let plut = b.ptr_add(Flags::NONE, lut, idx64);
        let out = b.load(Ty::I8, Flags::NONE, plut, 0, MemAttrs::default());
        let pdst = b.ptr_add(Flags::NONE, dst, i);
        b.store(Flags::NONE, out, pdst, 0, MemAttrs::default());
        let one = b.iconst(Ty::I64, 1);
        let i_next = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((hdr, &[i_next]));

        b.begin(exit);
        b.ret(&[]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn lut16_map_loop_lowers_to_4x_neon_tbl16() {
        let func = build_byte_lut_map_loop(15);
        let out = lower_simd_idioms(func.view(), SimdIdiomOpts::apple_silicon(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.tbl_loops, 1);
        assert!(has_intrinsic(&out.func.view(), NEON_TBL16_INTRINSIC));
        assert_eq!(count_intrinsic_calls(&out.func.view(), NEON_TBL16_INTRINSIC), 4);
    }

    #[test]
    fn lut64_map_loop_lowers_to_4x_neon_tbl64() {
        let func = build_byte_lut_map_loop(63);
        let out = lower_simd_idioms(func.view(), SimdIdiomOpts::apple_silicon(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.tbl_loops, 1);
        assert!(has_intrinsic(&out.func.view(), NEON_TBL64_INTRINSIC));
        assert_eq!(count_intrinsic_calls(&out.func.view(), NEON_TBL64_INTRINSIC), 4);
    }

    #[test]
    fn lut16_reduction_loop_lowers_to_neon_tbl16_and_verifies() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let src = b.param(Ty::PTR);
        let lut = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[z, z]));

        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let psrc = b.ptr_add(Flags::NONE, src, i);
        let byte = b.load(Ty::I8, Flags::NONE, psrc, 0, MemAttrs::default());
        let k15 = b.iconst(Ty::I8, 15);
        let idx = b.binary(Op::And, Flags::NONE, byte, k15);
        let idx64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, idx);
        let plut = b.ptr_add(Flags::NONE, lut, idx64);
        let out_b = b.load(Ty::I8, Flags::NONE, plut, 0, MemAttrs::default());
        let out64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, out_b);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, out64);
        let one = b.iconst(Ty::I64, 1);
        let i_next = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((hdr, &[i_next, acc_next]));

        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let out = lower_simd_idioms(func.view(), SimdIdiomOpts::apple_silicon(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.tbl_loops, 1);
        assert_eq!(count_intrinsic_calls(&out.func.view(), NEON_TBL16_INTRINSIC), 4);
        assert_eq!(count_intrinsic_calls(&out.func.view(), NEON_UDOT_INTRINSIC), 4);
    }

    fn build_dotprod_loop(acc_ty: Ty, signed: bool) -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(acc_ty)],
        );
        b.set_signature(sig);
        let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let z_i = b.iconst(Ty::I64, 0);
        let z_acc = b.iconst(acc_ty, 0);
        b.jump((hdr, &[z_i, z_acc]));

        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(acc_ty);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let ptr_a = b.ptr_add(Flags::NONE, pa, i);
        let ptr_b = b.ptr_add(Flags::NONE, pb, i);
        let va = b.load(Ty::I8, Flags::NONE, ptr_a, 0, MemAttrs::default());
        let vb = b.load(Ty::I8, Flags::NONE, ptr_b, 0, MemAttrs::default());
        let ext_op = if signed { Op::Sext } else { Op::Zext };
        let ea = b.convert(ext_op, Flags::NONE, acc_ty, va);
        let eb = b.convert(ext_op, Flags::NONE, acc_ty, vb);
        let prod = b.binary(Op::Mul, Flags::NONE, ea, eb);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, prod);
        let one = b.iconst(Ty::I64, 1);
        let i_next = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((hdr, &[i_next, acc_next]));

        b.begin(exit);
        b.ret(&[acc]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn signed_i8_dotprod_lowers_to_4x_neon_sdot() {
        let func = build_dotprod_loop(Ty::I32, true);
        let out = lower_simd_idioms(func.view(), SimdIdiomOpts::apple_silicon(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.dotprod_loops, 1);
        assert!(has_intrinsic(&out.func.view(), NEON_SDOT_INTRINSIC));
        assert_eq!(count_intrinsic_calls(&out.func.view(), NEON_SDOT_INTRINSIC), 4);
        assert!((0..out.func.view().len()).any(|k| out.func.view().op(k) == Op::ReduceAdd));
    }

    #[test]
    fn unsigned_u8_dotprod_lowers_to_4x_neon_udot() {
        let func = build_dotprod_loop(Ty::I32, false);
        let out = lower_simd_idioms(func.view(), SimdIdiomOpts::apple_silicon(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.dotprod_loops, 1);
        assert!(has_intrinsic(&out.func.view(), NEON_UDOT_INTRINSIC));
        assert_eq!(count_intrinsic_calls(&out.func.view(), NEON_UDOT_INTRINSIC), 4);
        assert!((0..out.func.view().len()).any(|k| out.func.view().op(k) == Op::ReduceAdd));
    }

    #[test]
    fn signed_i8_to_i64_dotprod_lowers_to_chunked_neon_sdot() {
        let func = build_dotprod_loop(Ty::I64, true);
        let out = lower_simd_idioms(func.view(), SimdIdiomOpts::apple_silicon(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.dotprod_loops, 1);
        assert_eq!(count_intrinsic_calls(&out.func.view(), NEON_SDOT_INTRINSIC), 4);
        assert!((0..out.func.view().len()).any(|k| out.func.view().op(k) == Op::ReduceAdd));
    }

    fn all_24_perms_4() -> [[usize; 4]; 24] {
        let mut out = [[0usize; 4]; 24];
        let mut n = 0usize;
        for a in 0..4 {
            for b in 0..4 {
                if b == a {
                    continue;
                }
                for c in 0..4 {
                    if c == a || c == b {
                        continue;
                    }
                    let d = 6 - a - b - c;
                    out[n] = [a, b, c, d];
                    n += 1;
                }
            }
        }
        out
    }

    #[test]
    fn select_sort4_canonicalizes_to_smin_smax_and_matches_eval_all_24_perms() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let p = Param::of(Ty::I32);
        let sig = b.sig(Conv::C, Variadic::No, &[p, p, p, p], &[p, p, p, p]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let mut v = [b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32)];
        for (i, j) in [(0, 1), (2, 3), (0, 2), (1, 3), (1, 2)] {
            let cmp = b.icmp(IntCond::Slt, v[i], v[j]);
            let lo = b.select(Flags::NONE, cmp, v[i], v[j]);
            let hi = b.select(Flags::NONE, cmp, v[j], v[i]);
            v[i] = lo;
            v[j] = hi;
        }
        b.ret(&v);
        let orig = b.finish_checked().unwrap();

        let out = lower_simd_idioms(orig.view(), SimdIdiomOpts::apple_silicon(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.sort_swaps_canonicalized, 5);
        assert_eq!(
            (0..out.func.view().len()).filter(|&k| out.func.view().op(k) == Op::Select).count(),
            0
        );
        assert_eq!(
            (0..out.func.view().len()).filter(|&k| out.func.view().op(k) == Op::Smin).count(),
            5
        );
        assert_eq!(
            (0..out.func.view().len()).filter(|&k| out.func.view().op(k) == Op::Smax).count(),
            5
        );

        let vals = [-100i32, -1i32, 0i32, 42i32];
        let want: [Val; 4] = vals.map(|x| Val::Int(u128::from(x.cast_unsigned())));
        for perm in all_24_perms_4() {
            let args = perm.map(|idx| Val::Int(u128::from(vals[idx].cast_unsigned())));
            let r_orig = eval::run_in(orig.view(), &args, 10_000, TestHeap).unwrap();
            let r_opt = eval::run_in(out.func.view(), &args, 10_000, TestHeap).unwrap();
            assert_eq!(r_orig, r_opt);
            match r_opt {
                Outcome::Returned(res) => assert_eq!(res.as_slice(), &want),
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }

    #[test]
    fn branch_diamond_sort4_canonicalizes_to_branchless_smin_smax_all_24_perms() {
        // 5 sequential `if a > b { swap(a, b) }` diamonds with explicit swap trampoline blocks.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let p = Param::of(Ty::I32);
        let sig = b.sig(Conv::C, Variadic::No, &[p, p, p, p], &[p, p, p, p]);
        b.set_signature(sig);

        let stages = [(0usize, 1usize), (2, 3), (0, 2), (1, 3), (1, 2)];
        let blks: [Block; 6] = [(); 6].map(|_| b.create_block(BlockKind::Normal));
        let swaps: [Block; 5] = [(); 5].map(|_| b.create_block(BlockKind::Normal));

        b.begin(blks[0]);
        let mut cur = [b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32)];
        for (s, &(i, j)) in stages.iter().enumerate() {
            if s > 0 {
                b.begin(blks[s]);
                cur = [b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32)];
            }
            let gt = b.icmp(IntCond::Sgt, cur[i], cur[j]);
            b.br(Flags::NONE, gt, (swaps[s], &[]), (blks[s + 1], &cur));

            b.begin(swaps[s]);
            let mut swapped = cur;
            swapped.swap(i, j);
            b.jump((blks[s + 1], &swapped));
        }
        b.begin(blks[5]);
        let res = [b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32)];
        b.ret(&res);
        let orig = b.finish_checked().unwrap();

        let out = lower_simd_idioms(orig.view(), SimdIdiomOpts::apple_silicon(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.sort_swaps_canonicalized, 5);
        assert_eq!(out.func.view().blocks(), 1, "all 5 diamonds should coalesce into 1 branchless block");
        assert_eq!(
            (0..out.func.view().len()).filter(|&k| out.func.view().op(k) == Op::Br).count(),
            0
        );
        assert_eq!(
            (0..out.func.view().len()).filter(|&k| out.func.view().op(k) == Op::Smin).count(),
            5
        );
        assert_eq!(
            (0..out.func.view().len()).filter(|&k| out.func.view().op(k) == Op::Smax).count(),
            5
        );

        let vals = [-500i32, -7i32, 13i32, 999i32];
        let want: [Val; 4] = vals.map(|x| Val::Int(u128::from(x.cast_unsigned())));
        for perm in all_24_perms_4() {
            let args = perm.map(|idx| Val::Int(u128::from(vals[idx].cast_unsigned())));
            let r_orig = eval::run_in(orig.view(), &args, 10_000, TestHeap).unwrap();
            let r_opt = eval::run_in(out.func.view(), &args, 10_000, TestHeap).unwrap();
            assert_eq!(r_orig, r_opt);
            match r_opt {
                Outcome::Returned(r) => assert_eq!(r.as_slice(), &want),
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }

    #[test]
    fn emit_sort4_and_sort8_networks_verify_and_sort_exhaustively() {
        let f4 = emit_sort4_network(Ty::I32, true, TestHeap).unwrap();
        verify(f4.view()).unwrap();
        let vals4 = [-20i32, -1i32, 7i32, 100i32];
        let want4: [Val; 4] = vals4.map(|x| Val::Int(u128::from(x.cast_unsigned())));
        for perm in all_24_perms_4() {
            let args = perm.map(|idx| Val::Int(u128::from(vals4[idx].cast_unsigned())));
            match eval::run_in(f4.view(), &args, 10_000, TestHeap).unwrap() {
                Outcome::Returned(r) => assert_eq!(r.as_slice(), &want4),
                other => panic!("unexpected outcome: {other:?}"),
            }
        }

        // Zero-one principle: testing all 2^8 = 256 binary inputs proves `emit_sort8_network`
        // sorts all 8! = 40,320 permutations of arbitrary integers.
        let f8 = emit_sort8_network(Ty::I32, false, TestHeap).unwrap();
        verify(f8.view()).unwrap();
        for bits in 0u32..256 {
            let mut args = [Val::Int(0); 8];
            let mut ones = 0usize;
            for k in 0..8 {
                let bit = (bits >> k) & 1;
                args[k] = Val::Int(u128::from(bit));
                ones += bit as usize;
            }
            let mut want = [Val::Int(0); 8];
            for k in (8 - ones)..8 {
                want[k] = Val::Int(1);
            }
            match eval::run_in(f8.view(), &args, 10_000, TestHeap).unwrap() {
                Outcome::Returned(r) => assert_eq!(r.as_slice(), &want),
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }
}
