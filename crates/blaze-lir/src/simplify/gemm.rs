//! Auto-SME2 & 4x16 NEON Register-Blocked GEMM Optimizer ([`optimize_gemm_loops`]).
//!
//! Recognizes triple-nested matrix multiply-accumulate loops (`i-k-j` broadcast-A/unit-stride-B/C
//! and `i-j-k` dot-product orders, both `HeaderBody` and rotated `SingleBlock` shapes, over
//! `f32`, `f64`, or `i32`, square or rectangular, `C += A*B` or `C = A*B`) and lowers them via:
//!
//! - **Path A — Hardware Auto-SME2 Intrinsic** (`opts.enable_sme2`, `f32`/`f64`, contiguous
//!   row-major strides `lda == K`, `ldb == N`, `ldc == N`): emits a guarded
//!   `Op::TargetIntrinsic` call to `"blaze.sme2.gemm_f32"` / `"blaze.sme2.gemm_f64"` with
//!   signature `(c: PTR, a: PTR, b: PTR, m: I64, n: I64, k: I64, flags: I64) -> ()` where
//!   `flags & 1` selects accumulate (`1`) vs overwrite (`0`), guarded by dimension thresholds
//!   (`m >= min_sme2_dim && n >= min_sme2_dim && k >= 4`), overflow bounds, and wraparound-safe
//!   non-aliasing checks (skipped when `ParamFactsRec::NOALIAS` proves disjointness).
//! - **Path B — Universal 4x16 NEON Register-Blocked Micro-Kernel** (`opts.enable_neon_4x16`):
//!   rewrites the triple nest into a 4-row x 4-vector (`16` 128-bit NEON vector accumulators:
//!   `4x16` `f32`/`i32` or `4x8` `f64`) register-blocked micro-kernel whose inner `k` loop uses
//!   zero `Shl`/`Mul` index math (only running pointers `pa0..pa3` stepping by `+elem_bytes` and
//!   `pb` stepping by `+ldb_bytes`), loads 4 contiguous `B` vectors (`+0,+16,+32,+48` -> two
//!   `ldp q, q`), broadcasts each scalar `A[(i+r)*lda + k]` once via `Op::Splat`, and executes
//!   16 independent vector FMAs per iteration (4x less `B` memory bandwidth), followed by exact
//!   `j`-column and `i`-row cleanup tails and an untouched original-loop fallback for tiny or
//!   aliased invocations.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
};
use crate::{
    Block, BlockKind, Flags, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    func::{Conv, Ext, IntrinsicId, IntrinsicRec, ParamFactsRec, ParamKind, ParamRec, SigId, SigRec},
    op::{Field, Layout},
    verify,
};

/// Target intrinsic name emitted for hardware SME2 `f32` GEMM:
/// `(c: ptr, a: ptr, b: ptr, m: i64, n: i64, k: i64, flags: i64) -> ()`.
pub const SME2_GEMM_F32_INTRINSIC: &str = "blaze.sme2.gemm_f32";

/// Target intrinsic name emitted for hardware SME2 `f64` GEMM:
/// `(c: ptr, a: ptr, b: ptr, m: i64, n: i64, k: i64, flags: i64) -> ()`.
pub const SME2_GEMM_F64_INTRINSIC: &str = "blaze.sme2.gemm_f64";

/// Target intrinsic name emitted for hardware SME2 signed `i8 -> i32` (`SMOPA`) GEMM:
/// `(c: ptr, a: ptr, b: ptr, m: i64, n: i64, k: i64, flags: i64) -> ()`.
pub const SME2_GEMM_I8_I32_INTRINSIC: &str = "blaze.sme2.gemm_i8_i32";

/// Target intrinsic name emitted for hardware SME2 unsigned `u8 -> u32` (`UMOPA`) GEMM:
/// `(c: ptr, a: ptr, b: ptr, m: i64, n: i64, k: i64, flags: i64) -> ()`.
pub const SME2_GEMM_U8_U32_INTRINSIC: &str = "blaze.sme2.gemm_u8_u32";

/// Maximum number of disjoint GEMM nests lowered per function in a single pass.
const MAX_GEMMS_PER_FUNC: usize = 8;

/// Maximum element dimension / stride permitted on the fast path so `dim * ld * 8 <= 2^59 < 2^63`
/// never overflows 64-bit byte-extent or signed `PtrAdd` arithmetic.
const MAX_SAFE_DIM: u64 = 1u64 << 28;

/// Configuration for [`optimize_gemm_loops`] (Rule 83: single options struct, no `with_*` methods).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GemmOpts {
    /// Emit guarded hardware SME2 intrinsic (`"blaze.sme2.gemm_f32"` / `"blaze.sme2.gemm_f64"`)
    /// when `elem_ty` is `f32`/`f64` and row strides match contiguous `(K, N, N)`.
    pub enable_sme2: bool,
    /// Emit the universal 4x16 (4 rows x 4 128-bit vectors = 16 NEON accumulators) register-blocked
    /// micro-kernel for non-SME2 targets, `i32` GEMMs, custom strides, or sub-SME2 dimensions.
    pub enable_neon_4x16: bool,
    /// Minimum `M` and `N` dimension required to enter the SME2 streaming micro-kernel.
    pub min_sme2_dim: u64,
}

impl GemmOpts {
    /// Universal 4x16 NEON register-blocked micro-kernel (SME2 disabled).
    pub const fn neon() -> Self {
        Self {
            enable_sme2: false,
            enable_neon_4x16: true,
            min_sme2_dim: 16,
        }
    }

    /// Hardware Auto-SME2 dispatch (`m >= 16 && n >= 16 && k >= 4`) with 4x16 NEON fallback.
    pub const fn sme2() -> Self {
        Self {
            enable_sme2: true,
            enable_neon_4x16: true,
            min_sme2_dim: 16,
        }
    }
}

impl Default for GemmOpts {
    fn default() -> Self {
        Self::neon()
    }
}

/// Telemetry returned by [`optimize_gemm_loops`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct GemmStats {
    /// Number of triple-nested GEMM kernels recognized in the function.
    pub gemms_detected: u32,
    /// Number of GEMM kernels lowered to a guarded hardware SME2 intrinsic call.
    pub sme2_lowered: u32,
    /// Number of GEMM kernels lowered to the 4x16 NEON register-blocked micro-kernel.
    pub neon_blocked: u32,
}

/// Output of [`optimize_gemm_loops`].
#[derive(Debug)]
pub struct GemmOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: GemmStats,
}

// ---------------------------------------------------------------------------
// Low-level ratchet-clean indexing, conversion, and inspection helpers
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
    let [b0, b1, b2, b3, ..] = v.to_le_bytes();
    u32::from_le_bytes([b0, b1, b2, b3])
}

#[inline]
fn trunc_u64(v: u128) -> u64 {
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = v.to_le_bytes();
    u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
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
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
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
const fn conv_byte(c: Conv) -> u8 {
    match c {
        Conv::C => 0,
        Conv::SysV64 => 1,
        Conv::Win64 => 2,
        Conv::Aapcs => 3,
        Conv::Wasm => 4,
        Conv::PreserveMost => 5,
        Conv::BlazeResult => 6,
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
            let start = to_idx(view.data.get(inst).copied()?);
            let end = start.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(start..end)?, width)
        }
        _ => return None,
    };
    Some(trunc_u64(v))
}

#[inline]
fn is_zero_elem_const(view: &View<'_>, inst: usize, elem_ty: Ty) -> bool {
    if Ty(at(view.tys, inst)) != elem_ty {
        return false;
    }
    match view.op(inst) {
        Op::Iconst => elem_ty == Ty::I32 && at(view.data, inst) == 0,
        Op::Fconst => elem_ty == Ty::F32 && at(view.data, inst) == 0,
        Op::FconstWide => {
            if elem_ty != Ty::F64 {
                return false;
            }
            let pos = to_idx(at(view.data, inst));
            at(view.extra, pos) == 0 && at(view.extra, pos + 1) == 0
        }
        _ => false,
    }
}

#[inline]
fn strip_int_ext(view: &View<'_>, mut v: u32) -> u32 {
    for _ in 0..8 {
        let vi = to_idx(v);
        if matches!(view.op(vi), Op::Zext | Op::Sext) {
            v = at(view.data, vi);
        } else {
            break;
        }
    }
    v
}

#[inline]
fn same_val_or_const(view: &View<'_>, a: u32, b: u32) -> bool {
    if a == b {
        return true;
    }
    let ai = to_idx(a);
    let bi = to_idx(b);
    match (const_u64(view, ai), const_u64(view, bi)) {
        (Some(ca), Some(cb)) => ca == cb,
        _ => strip_int_ext(view, a) == strip_int_ext(view, b),
    }
}

/// Pure, non-trapping, side-effect-free operation permitted inside a recognized GEMM loop nest.
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
            | Op::Freeze
            | Op::PtrAdd
            | Op::SlotAddr
    )
}

fn for_each_use(view: &View<'_>, inst: usize, mut f: impl FnMut(u32)) {
    each(view, inst, |item| match item {
        Item::Value(v) => f(v),
        Item::Edge(edge) => {
            if let Some((start, count)) = edge.args {
                for k in 0..count {
                    f(at(view.extra, start + k));
                }
            }
        }
    });
}

/// Returns `true` when `a` and `b` trace through `Op::PtrAdd` chains to provably non-aliasing
/// root objects: either distinct local `Op::SlotAddr`s, or distinct entry-block `Op::Param`s
/// where at least one carries `ParamFactsRec::NOALIAS`.
fn provably_noalias_bases(view: &View<'_>, mut a: u32, mut b: u32) -> bool {
    for _ in 0..8 {
        let ai = to_idx(a);
        if view.op(ai) == Op::PtrAdd {
            a = at(view.extra, to_idx(at(view.data, ai)));
        } else {
            break;
        }
    }
    for _ in 0..8 {
        let bi = to_idx(b);
        if view.op(bi) == Op::PtrAdd {
            b = at(view.extra, to_idx(at(view.data, bi)));
        } else {
            break;
        }
    }
    if a == b {
        return false;
    }
    let ai = to_idx(a);
    let bi = to_idx(b);
    if view.op(ai) == Op::SlotAddr && view.op(bi) == Op::SlotAddr {
        return at(view.data, ai) != at(view.data, bi);
    }
    let entry_params = view.params(Block(0));
    if view.op(ai) == Op::Param
        && view.op(bi) == Op::Param
        && ai >= entry_params.start
        && ai < entry_params.end
        && bi >= entry_params.start
        && bi < entry_params.end
    {
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

fn copy_u32_vec<A: Allocator + Copy>(src: &[u32], alloc: A) -> Vec<u32, A> {
    let mut dst = Vec::with_capacity_in(src.len(), alloc);
    for &v in src {
        dst.push(v);
    }
    dst
}

fn copy_bool_vec<A: Allocator + Copy>(src: &[bool], alloc: A) -> Vec<bool, A> {
    let mut dst = Vec::with_capacity_in(src.len(), alloc);
    for &v in src {
        dst.push(v);
    }
    dst
}

// ---------------------------------------------------------------------------
// Low-level LIR emission helpers
// ---------------------------------------------------------------------------

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

fn begin_block<A: Allocator>(func: &mut Func<A>, kind: BlockKind) -> u32 {
    let b = to_u32(func.block_starts.len());
    func.block_starts.push(to_u32(func.ops.len()));
    func.block_kinds.push(block_kind_byte(kind));
    b
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
        let pos = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, pos, 0)
    }
}

fn push_zero_scalar<A: Allocator>(func: &mut Func<A>, elem_ty: Ty) -> u32 {
    if elem_ty == Ty::F32 {
        push_inst(func, Op::Fconst, Flags::NONE, Ty::F32, 0, 0)
    } else if elem_ty == Ty::F64 {
        let pos = to_u32(func.extra.len());
        func.extra.push(0);
        func.extra.push(0);
        push_inst(func, Op::FconstWide, Flags::NONE, Ty::F64, pos, 0)
    } else {
        push_iconst(func, elem_ty, 0)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32) -> u32 {
    let pos = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, pos, 0)
}

fn push_tern<A: Allocator>(
    func: &mut Func<A>,
    op: Op,
    flags: Flags,
    ty: Ty,
    a: u32,
    b: u32,
    c: u32,
) -> u32 {
    let pos = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    func.extra.push(c);
    push_inst(func, op, flags, ty, pos, 0)
}

fn push_icmp<A: Allocator>(func: &mut Func<A>, cond: IntCond, a: u32, b: u32) -> u32 {
    let pos = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Icmp, Flags::NONE, Ty::I1, pos, 0)
}

fn push_load<A: Allocator>(func: &mut Func<A>, ty: Ty, ptr: u32, off: i32) -> u32 {
    let pos = to_u32(func.extra.len());
    func.extra.push(ptr);
    func.extra.push(off.cast_unsigned());
    push_inst(func, Op::Load, Flags::NONE, ty, pos, 0)
}

fn push_store<A: Allocator>(func: &mut Func<A>, val: u32, ptr: u32, off: i32) -> u32 {
    let pos = to_u32(func.extra.len());
    func.extra.push(val);
    func.extra.push(ptr);
    func.extra.push(off.cast_unsigned());
    push_inst(func, Op::Store, Flags::NONE, Ty::NONE, pos, 0)
}

fn push_jump<A: Allocator>(func: &mut Func<A>, target: u32, args: &[u32]) -> u32 {
    let pos = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len()));
    for &a in args {
        func.extra.push(a);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, pos, 0)
}

fn push_br<A: Allocator>(
    func: &mut Func<A>,
    cond: u32,
    t_blk: u32,
    t_args: &[u32],
    e_blk: u32,
    e_args: &[u32],
) -> u32 {
    let pos = to_u32(func.extra.len());
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
    push_inst(func, Op::Br, Flags::NONE, Ty::NONE, pos, 0)
}

fn push_target_intrinsic<A: Allocator>(func: &mut Func<A>, intrinsic: IntrinsicId, args: &[u32]) -> u32 {
    let pos = to_u32(func.extra.len());
    func.extra.push(intrinsic.0);
    func.extra.push(to_u32(args.len()));
    for &a in args {
        func.extra.push(a);
    }
    push_inst(func, Op::TargetIntrinsic, Flags::NONE, Ty::NONE, pos, 0)
}

/// Ensures `"blaze.sme2.gemm_f32"` or `"blaze.sme2.gemm_f64"` with signature
/// `(c: ptr, a: ptr, b: ptr, m: i64, n: i64, k: i64, flags: i64) -> ()` exists in
/// `func.intrinsics`, preserving canonical CSR tiling of `sigs`/`sig_params` and
/// `intrinsics`/`names`.
fn ensure_sme2_intrinsic<A: Allocator>(func: &mut Func<A>, name: &str) -> IntrinsicId {
    let want = name.as_bytes();
    let expected_tys: [u16; 7] = [
        Ty::PTR.0,
        Ty::PTR.0,
        Ty::PTR.0,
        Ty::I64.0,
        Ty::I64.0,
        Ty::I64.0,
        Ty::I64.0,
    ];
    let c_conv = conv_byte(Conv::C);
    let no_ext = ext_byte(Ext::None);
    let val_kind = param_kind_byte(ParamKind::Value);

    let sig_matches = |s: SigRec, params: &[ParamRec]| -> bool {
        if s.conv != c_conv || s.bits != 0 || s.params != 7 || s.rets != 0 {
            return false;
        }
        let start = to_idx(s.first);
        for (k, &want_ty) in expected_tys.iter().enumerate() {
            let Some(p) = params.get(start + k) else {
                return false;
            };
            if p.facts != 0 || p.ty != want_ty || p.ext != no_ext || p.kind != val_kind {
                return false;
            }
        }
        true
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

    let existing_sig = func
        .sigs
        .iter()
        .enumerate()
        .find_map(|(r, &s)| sig_matches(s, &func.sig_params).then_some(to_u32(r)));
    let sig_row = match existing_sig {
        Some(row) => row,
        None => {
            let first = to_u32(func.sig_params.len());
            for &ty in &expected_tys {
                func.sig_params.push(ParamRec {
                    facts: 0,
                    ty,
                    ext: no_ext,
                    kind: val_kind,
                });
            }
            let row = to_u32(func.sigs.len());
            func.sigs.push(SigRec {
                first,
                params: 7,
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

#[derive(Clone, Copy)]
enum ValFixup {
    Data { inst: usize, old_val: u32 },
    Extra { pos: usize, old_val: u32 },
}

fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
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
                    Field::Blocks => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(at(view.extra, p));
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
                            func.extra.push(at(view.extra, p));
                            let an_u32 = at(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                let old = at(view.extra, p + k);
                                let mapped = at(val_map, to_idx(old));
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
    func.ops.push(op_raw);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

/// Copies `outer_preheader`'s terminator (`Op::Jump` or `Op::Br`), redirecting the edge targeting
/// `old_header` to `(dispatch_blk, [])` while keeping any other arm unchanged.
fn copy_redirected_preheader_term<A: Allocator>(
    view: &View<'_>,
    term: usize,
    old_header: u32,
    dispatch_blk: u32,
    val_map: &[u32],
    func: &mut Func<A>,
    fixups: &mut Vec<ValFixup, A>,
) {
    let op = view.op(term);
    if op == Op::Jump {
        push_jump(func, dispatch_blk, &[]);
        return;
    }
    if op == Op::Br {
        let src = to_idx(at(view.data, term));
        let cond_old = at(view.extra, src);
        let t_blk = at(view.extra, src + 1);
        let t_argc = to_idx(at(view.extra, src + 2));
        let e_at = src + 3 + t_argc;
        let e_blk = at(view.extra, e_at);
        let e_argc = to_idx(at(view.extra, e_at + 1));

        let dst = to_u32(func.extra.len());
        let cond_mapped = at(val_map, to_idx(cond_old));
        if cond_mapped == NONE {
            fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: cond_old });
            func.extra.push(0);
        } else {
            func.extra.push(cond_mapped);
        }

        if t_blk == old_header {
            func.extra.push(dispatch_blk);
            func.extra.push(0);
        } else {
            func.extra.push(t_blk);
            func.extra.push(to_u32(t_argc));
            for k in 0..t_argc {
                let old = at(view.extra, src + 3 + k);
                let mapped = at(val_map, to_idx(old));
                if mapped == NONE {
                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                    func.extra.push(0);
                } else {
                    func.extra.push(mapped);
                }
            }
        }

        if e_blk == old_header {
            func.extra.push(dispatch_blk);
            func.extra.push(0);
        } else {
            func.extra.push(e_blk);
            func.extra.push(to_u32(e_argc));
            for k in 0..e_argc {
                let old = at(view.extra, e_at + 2 + k);
                let mapped = at(val_map, to_idx(old));
                if mapped == NONE {
                    fixups.push(ValFixup::Extra { pos: func.extra.len(), old_val: old });
                    func.extra.push(0);
                } else {
                    func.extra.push(mapped);
                }
            }
        }

        push_inst(func, Op::Br, Flags(at(view.flags, term)), Ty::NONE, dst, 0);
        return;
    }
    let _ = copy_remapped(view, term, val_map, func, fixups);
}

// ---------------------------------------------------------------------------
// CFG, Dominators, & Counted Natural Loop Discovery
// ---------------------------------------------------------------------------

struct CfgInfo<A: Allocator> {
    inst_block: Vec<u32, A>,
    preds: Vec<Vec<u32, A>, A>,
    idom: Vec<u32, A>,
}

impl<A: Allocator + Copy> CfgInfo<A> {
    fn build(view: &View<'_>, alloc: A) -> Self {
        let blocks = view.blocks();
        let len = view.len();
        let mut inst_block = filled(len, 0u32, &alloc);
        let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
        let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
        for _ in 0..blocks {
            preds.push(Vec::new_in(alloc));
            succs.push(Vec::new_in(alloc));
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
                    if let Some(st) = succs.get_mut(b)
                        && st.scan_for(&edge.block).is_none()
                    {
                        st.push(edge.block);
                    }
                    if let Some(pt) = preds.get_mut(tb)
                        && pt.scan_for(&b32).is_none()
                    {
                        pt.push(b32);
                    }
                }
            });
        }

        // Reverse postorder via explicit iterative stack.
        let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        let mut seen = filled(blocks, false, &alloc);
        let mut stack: Vec<(u32, usize), A> = Vec::with_capacity_in(blocks, alloc);
        if blocks > 0 {
            set_at(&mut seen, 0, true);
            stack.push((0, 0));
        }
        let max_dfs = blocks.saturating_mul(8).saturating_add(16);
        for _ in 0..max_dfs {
            let Some(&(b, next_idx)) = stack.last() else {
                break;
            };
            let maybe_succ = succs.get(to_idx(b)).and_then(|s| s.get(next_idx).copied());
            if let Some(s) = maybe_succ {
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                if !at(&seen, to_idx(s)) {
                    set_at(&mut seen, to_idx(s), true);
                    stack.push((s, 0));
                }
            } else {
                post.push(b);
                let _ = stack.pop();
            }
        }

        let mut rpo: Vec<u32, A> = Vec::with_capacity_in(post.len(), alloc);
        let mut order = filled(blocks, NONE, &alloc);
        for (i, &b) in post.iter().rev().enumerate() {
            rpo.push(b);
            set_at(&mut order, to_idx(b), to_u32(i));
        }

        // Cooper-Harvey-Kennedy iterative dominators.
        let mut idom = filled(blocks, NONE, &alloc);
        if blocks > 0 {
            set_at(&mut idom, 0, 0);
        }
        let max_rounds = blocks.saturating_mul(2).saturating_add(2);
        for _ in 0..max_rounds {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new_idom = NONE;
                if let Some(p_list) = preds.get(to_idx(b)) {
                    for &p in p_list {
                        if at(&idom, to_idx(p)) == NONE {
                            continue;
                        }
                        new_idom = if new_idom == NONE {
                            p
                        } else {
                            dom_intersect(&idom, &order, p, new_idom)
                        };
                    }
                }
                if new_idom != at(&idom, to_idx(b)) {
                    set_at(&mut idom, to_idx(b), new_idom);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        Self { inst_block, preds, idom }
    }

    fn dominates(&self, a: u32, mut b: u32) -> bool {
        if at(&self.idom, to_idx(a)) == NONE || at(&self.idom, to_idx(b)) == NONE {
            return false;
        }
        for _ in 0..self.idom.len() {
            if a == b {
                return true;
            }
            if b == 0 {
                return false;
            }
            b = at(&self.idom, to_idx(b));
        }
        false
    }
}

fn dom_intersect(idom: &[u32], order: &[u32], mut a: u32, mut b: u32) -> u32 {
    let bound = idom.len().saturating_mul(2).saturating_add(2);
    for _ in 0..bound {
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

struct CountedLoop<A: Allocator> {
    header: u32,
    latch: u32,
    preheader: u32,
    body_entry: u32,
    exit: u32,
    is_rotated: bool,
    iv_pos: usize,
    iv_ty: Ty,
    bound: u32,
    is_signed: bool,
    pre_args: Vec<u32, A>,
    back_args: Vec<u32, A>,
    exit_args: Vec<u32, A>,
    skip_args: Vec<u32, A>,
    in_loop: Vec<bool, A>,
    block_count: usize,
}

/// Unwraps up to 6 layers of boolean wrappers (`Op::Freeze`, `Op::Not`, `Op::Xor(_, 1)`,
/// `Op::Trunc(Op::Zext(i1))`, or `Op::Icmp(Eq|Ne, i1|Zext(i1), 0|1)`) around an `i1` condition.
fn unwrap_bool_cond(view: &View<'_>, mut cond_val: u32) -> (u32, bool) {
    let mut flipped = false;
    for _ in 0..6 {
        let ci = to_idx(cond_val);
        if ci >= view.len() {
            break;
        }
        match view.op(ci) {
            Op::Freeze => {
                cond_val = at(view.data, ci);
            }
            Op::Not => {
                cond_val = at(view.data, ci);
                flipped = !flipped;
            }
            Op::Trunc => {
                let src = at(view.data, ci);
                let si = to_idx(src);
                if si < view.len() && view.op(si) == Op::Zext {
                    let inner = at(view.data, si);
                    if Ty(at(view.tys, to_idx(inner))) == Ty::I1 {
                        cond_val = inner;
                        continue;
                    }
                }
                break;
            }
            Op::Xor => {
                let pos = to_idx(at(view.data, ci));
                let (l, r) = (at(view.extra, pos), at(view.extra, pos + 1));
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
                let cat = to_idx(at(view.data, ci));
                let Some(ic) = IntCond::from_word(at(view.extra, cat)) else {
                    break;
                };
                if ic != IntCond::Eq && ic != IntCond::Ne {
                    break;
                }
                let (lhs, rhs) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
                let (val, c) = if let Some(k) = const_u64(view, to_idx(rhs)).filter(|&k| k <= 1) {
                    (lhs, k)
                } else if let Some(k) = const_u64(view, to_idx(lhs)).filter(|&k| k <= 1) {
                    (rhs, k)
                } else {
                    break;
                };
                let vi = to_idx(val);
                let inner_i1 = if Ty(at(view.tys, vi)) == Ty::I1 {
                    val
                } else if view.op(vi) == Op::Zext {
                    let src = at(view.data, vi);
                    if Ty(at(view.tys, to_idx(src))) == Ty::I1 {
                        src
                    } else {
                        break;
                    }
                } else {
                    break;
                };
                let is_inverted = (c == 0 && ic == IntCond::Eq) || (c == 1 && ic == IntCond::Ne);
                cond_val = inner_i1;
                if is_inverted {
                    flipped = !flipped;
                }
            }
            _ => break,
        }
    }
    (cond_val, flipped)
}

#[inline]
fn is_step_one(view: &View<'_>, inst: u32, iv: u32) -> bool {
    let i = to_idx(inst);
    let (a, b) = match view.op(i) {
        Op::Add => {
            let pos = to_idx(at(view.data, i));
            (at(view.extra, pos), at(view.extra, pos + 1))
        }
        Op::Project => {
            let pos = to_idx(at(view.data, i));
            let pair = at(view.extra, pos);
            let field = at(view.extra, pos + 1);
            let pi = to_idx(pair);
            if field == 0 && pi < view.len() && view.op(pi) == Op::UaddOverflow {
                let apos = to_idx(at(view.data, pi));
                (at(view.extra, apos), at(view.extra, apos + 1))
            } else {
                return false;
            }
        }
        _ => return false,
    };
    (a == iv && const_u64(view, to_idx(b)) == Some(1))
        || (b == iv && const_u64(view, to_idx(a)) == Some(1))
}

/// Parses a branch `cond` testing `iv < bound` (true -> `stay_blk`, false -> `exit_blk`) or its
/// swapped/inverted equivalents (including `iv != bound` / `!(iv == bound)` for `0..bound`).
/// Returns `(bound, is_signed)`.
fn parse_lt_cond(view: &View<'_>, cond_val: u32, iv_val: u32, stay_is_then: bool) -> Option<(u32, bool)> {
    let (inner_cond, flipped) = unwrap_bool_cond(view, cond_val);
    let stay = stay_is_then ^ flipped;
    let ci = to_idx(inner_cond);
    if view.op(ci) != Op::Icmp {
        return None;
    }
    let cat = to_idx(at(view.data, ci));
    let cond = IntCond::from_word(at(view.extra, cat))?;
    let (lhs, rhs) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
    if stay {
        if lhs == iv_val && matches!(cond, IntCond::Ult | IntCond::Ne) {
            Some((rhs, false))
        } else if lhs == iv_val && cond == IntCond::Slt {
            Some((rhs, true))
        } else if rhs == iv_val && matches!(cond, IntCond::Ugt | IntCond::Ne) {
            Some((lhs, false))
        } else if rhs == iv_val && cond == IntCond::Sgt {
            Some((lhs, true))
        } else {
            None
        }
    } else if lhs == iv_val && matches!(cond, IntCond::Uge | IntCond::Eq) {
        Some((rhs, false))
    } else if lhs == iv_val && cond == IntCond::Sge {
        Some((rhs, true))
    } else if rhs == iv_val && matches!(cond, IntCond::Ule | IntCond::Eq) {
        Some((lhs, false))
    } else if rhs == iv_val && cond == IntCond::Sle {
        Some((lhs, true))
    } else {
        None
    }
}

/// Checks whether a rotated preheader branch guards `0 < bound` (entering `header` iff `bound > 0`).
fn preheader_guards_positive(
    view: &View<'_>,
    cond_val: u32,
    bound: u32,
    is_signed: bool,
    enter_is_then: bool,
) -> bool {
    let (inner_cond, flipped) = unwrap_bool_cond(view, cond_val);
    let enter = enter_is_then ^ flipped;
    let ci = to_idx(inner_cond);
    if view.op(ci) != Op::Icmp {
        return false;
    }
    let cat = to_idx(at(view.data, ci));
    let Some(cond) = IntCond::from_word(at(view.extra, cat)) else {
        return false;
    };
    let (lhs, rhs) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
    let lhs_zero = const_u64(view, to_idx(lhs)) == Some(0);
    let rhs_zero = const_u64(view, to_idx(rhs)) == Some(0);
    let lhs_bound = same_val_or_const(view, lhs, bound);
    let rhs_bound = same_val_or_const(view, rhs, bound);

    if enter {
        if is_signed {
            (lhs_zero && rhs_bound && cond == IntCond::Slt)
                || (lhs_bound && rhs_zero && cond == IntCond::Sgt)
        } else {
            (lhs_zero && rhs_bound && matches!(cond, IntCond::Ult | IntCond::Ne))
                || (lhs_bound && rhs_zero && matches!(cond, IntCond::Ugt | IntCond::Ne))
        }
    } else if is_signed {
        (lhs_zero && rhs_bound && cond == IntCond::Sge)
            || (lhs_bound && rhs_zero && cond == IntCond::Sle)
    } else {
        (lhs_zero && rhs_bound && matches!(cond, IntCond::Uge | IntCond::Eq))
            || (lhs_bound && rhs_zero && matches!(cond, IntCond::Ule | IntCond::Eq))
    }
}

struct BrInfo<A: Allocator> {
    cond: u32,
    t_blk: u32,
    t_args: Vec<u32, A>,
    e_blk: u32,
    e_args: Vec<u32, A>,
}

fn read_br<A: Allocator + Copy>(view: &View<'_>, term: usize, alloc: A) -> Option<BrInfo<A>> {
    if view.op(term) != Op::Br {
        return None;
    }
    let pos = to_idx(at(view.data, term));
    let cond = at(view.extra, pos);
    let t_blk = at(view.extra, pos + 1);
    let t_argc = to_idx(at(view.extra, pos + 2));
    let mut t_args = Vec::with_capacity_in(t_argc, alloc);
    for k in 0..t_argc {
        t_args.push(at(view.extra, pos + 3 + k));
    }
    let e_at = pos + 3 + t_argc;
    let e_blk = at(view.extra, e_at);
    let e_argc = to_idx(at(view.extra, e_at + 1));
    let mut e_args = Vec::with_capacity_in(e_argc, alloc);
    for k in 0..e_argc {
        e_args.push(at(view.extra, e_at + 2 + k));
    }
    Some(BrInfo { cond, t_blk, t_args, e_blk, e_args })
}

fn read_jump<A: Allocator + Copy>(view: &View<'_>, term: usize, alloc: A) -> Option<(u32, Vec<u32, A>)> {
    if view.op(term) != Op::Jump {
        return None;
    }
    let pos = to_idx(at(view.data, term));
    let target = at(view.extra, pos);
    let argc = to_idx(at(view.extra, pos + 1));
    let mut args = Vec::with_capacity_in(argc, alloc);
    for k in 0..argc {
        args.push(at(view.extra, pos + 2 + k));
    }
    Some((target, args))
}

/// Reads the back-edge argument list of a pre-tested loop from `latch` to `header`: either
/// `Op::Jump(header, args)` or an `Op::Br` arm targeting `header` whose OTHER arm stays inside
/// `in_loop` (e.g. an inner loop header exiting directly back to an outer loop header after
/// `Range::next` + `thread_jumps` collapses empty latch blocks).
fn read_pretested_back_edge_to_header<A: Allocator + Copy>(
    view: &View<'_>,
    latch: u32,
    header: u32,
    expected_argc: usize,
    in_loop: &[bool],
    alloc: A,
) -> Option<Vec<u32, A>> {
    if latch == header {
        return None;
    }
    let term = view.terminator(Block(latch));
    if let Some((tgt, args)) = read_jump(view, term, alloc) {
        if tgt == header && args.len() == expected_argc {
            return Some(args);
        }
        return None;
    }
    if let Some(br) = read_br(view, term, alloc) {
        if br.t_blk == header
            && br.e_blk != header
            && at(in_loop, to_idx(br.e_blk))
            && br.t_args.len() == expected_argc
        {
            return Some(br.t_args);
        }
        if br.e_blk == header
            && br.t_blk != header
            && at(in_loop, to_idx(br.t_blk))
            && br.e_args.len() == expected_argc
        {
            return Some(br.e_args);
        }
    }
    None
}

/// Discovers all single-latch, single-preheader counted natural loops `[0, bound)` (step `+1`)
/// in `view`, supporting both `HeaderBody` and rotated (`SingleBlock` or multi-block) shapes.
fn discover_counted_loops<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &CfgInfo<A>,
    alloc: A,
) -> (Vec<CountedLoop<A>, A>, Vec<(u32, u32), A>) {
    let blocks = view.blocks();
    let mut all_natural_loops: Vec<(u32, u32), A> = Vec::new_in(alloc);
    let mut counted: Vec<CountedLoop<A>, A> = Vec::new_in(alloc);

    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let Some(p_list) = cfg.preds.get(h) else {
            continue;
        };
        let mut back_latches: Vec<u32, A> = Vec::new_in(alloc);
        let mut outside_preds: Vec<u32, A> = Vec::new_in(alloc);
        for &p in p_list {
            if cfg.dominates(header, p) {
                back_latches.push(p);
                all_natural_loops.push((header, p));
            } else {
                outside_preds.push(p);
            }
        }
        if back_latches.len() != 1 || outside_preds.len() != 1 {
            continue;
        }
        let latch = at(&back_latches, 0);
        let preheader = at(&outside_preds, 0);
        if !cfg.dominates(preheader, header) {
            continue;
        }

        // Collect natural loop blocks via iterative backward BFS from `latch` to `header`.
        let mut in_loop = filled(blocks, false, &alloc);
        let mut work: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        set_at(&mut in_loop, h, true);
        let mut block_count = 1usize;
        if latch != header {
            set_at(&mut in_loop, to_idx(latch), true);
            block_count += 1;
            work.push(latch);
        }
        let max_bfs = blocks.saturating_mul(4).saturating_add(8);
        for _ in 0..max_bfs {
            let Some(cur) = work.pop() else {
                break;
            };
            if let Some(cp) = cfg.preds.get(to_idx(cur)) {
                for &pred in cp {
                    if !at(&in_loop, to_idx(pred)) {
                        set_at(&mut in_loop, to_idx(pred), true);
                        block_count += 1;
                        work.push(pred);
                    }
                }
            }
        }

        let h_params = view.params(Block(header));
        if h_params.is_empty() {
            continue;
        }

        // Read preheader edge into `header`.
        let pre_term = view.terminator(Block(preheader));
        let (pre_args, pre_br_skip) = if let Some((tgt, args)) = read_jump(view, pre_term, alloc) {
            if tgt != header || args.len() != h_params.len() {
                continue;
            }
            (args, None)
        } else if let Some(br) = read_br(view, pre_term, alloc) {
            if br.t_blk == header && br.e_blk != header && br.t_args.len() == h_params.len() {
                (br.t_args, Some((br.cond, true, br.e_blk, br.e_args)))
            } else if br.e_blk == header && br.t_blk != header && br.e_args.len() == h_params.len() {
                (br.e_args, Some((br.cond, false, br.t_blk, br.t_args)))
            } else {
                continue;
            }
        } else {
            continue;
        };

        // Determine loop shape: `HeaderBody` (`latch` jumps to `header` or is a collapsed inner
        // `Br` whose other arm stays inside `in_loop`) or `Rotated` (`latch` ends in an exit `Br`).
        let latch_term = view.terminator(Block(latch));
        if let Some(back_args) =
            read_pretested_back_edge_to_header(view, latch, header, h_params.len(), &in_loop, alloc)
        {
            // Pre-tested loop (`HeaderBody` / multi-block with header test).
            let h_term = view.terminator(Block(header));
            let Some(h_br) = read_br(view, h_term, alloc) else {
                continue;
            };
            let t_in = at(&in_loop, to_idx(h_br.t_blk));
            let e_in = at(&in_loop, to_idx(h_br.e_blk));
            let (stay_is_then, body_entry, exit, exit_args) = if t_in && !e_in {
                (true, h_br.t_blk, h_br.e_blk, h_br.e_args)
            } else if e_in && !t_in {
                (false, h_br.e_blk, h_br.t_blk, h_br.t_args)
            } else {
                continue;
            };
            // For pre-tested `HeaderBody` loops, the loop tests `0 < bound` itself in `header`,
            // so `preheader` may end in any `Br` (e.g. a preceding sibling loop's exit branch or
            // outer loop's header branch after jump-threading collapses forwarding preheaders).
            let skip_args = match pre_br_skip {
                Some((_, _, skip_blk, sa)) if skip_blk == exit => sa,
                _ => Vec::new_in(alloc),
            };

            // Identify primary IV `0..bound` stepping by `+1`.
            let mut found = None;
            for pos in 0..h_params.len() {
                let p_idx = h_params.start + pos;
                let p_inst = to_u32(p_idx);
                let p_ty = Ty(at(view.tys, p_idx));
                if p_ty != Ty::I64 && p_ty != Ty::I32 {
                    continue;
                }
                let init_v = at(&pre_args, pos);
                if const_u64(view, to_idx(init_v)) != Some(0) {
                    continue;
                }
                let back_v = at(&back_args, pos);
                if !is_step_one(view, back_v, p_inst) {
                    continue;
                }
                if let Some((bound, is_signed)) = parse_lt_cond(view, h_br.cond, p_inst, stay_is_then)
                    && !at(&in_loop, to_idx(at(&cfg.inst_block, to_idx(bound))))
                {
                    found = Some((pos, p_ty, bound, is_signed));
                    break;
                }
            }
            let Some((iv_pos, iv_ty, bound, is_signed)) = found else {
                continue;
            };
            counted.push(CountedLoop {
                header,
                latch,
                preheader,
                body_entry,
                exit,
                is_rotated: false,
                iv_pos,
                iv_ty,
                bound,
                is_signed,
                pre_args,
                back_args,
                exit_args,
                skip_args,
                in_loop,
                block_count,
            });
        } else if let Some(l_br) = read_br(view, latch_term, alloc) {
            // Post-tested / rotated loop (`SingleBlock` or multi-block with latch test).
            let (stay_is_then, back_args, exit, exit_args) =
                if l_br.t_blk == header && !at(&in_loop, to_idx(l_br.e_blk)) && l_br.t_args.len() == h_params.len() {
                    (true, l_br.t_args, l_br.e_blk, l_br.e_args)
                } else if l_br.e_blk == header
                    && !at(&in_loop, to_idx(l_br.t_blk))
                    && l_br.e_args.len() == h_params.len()
                {
                    (false, l_br.e_args, l_br.t_blk, l_br.t_args)
                } else {
                    continue;
                };

            let mut found = None;
            for pos in 0..h_params.len() {
                let p_idx = h_params.start + pos;
                let p_inst = to_u32(p_idx);
                let p_ty = Ty(at(view.tys, p_idx));
                if p_ty != Ty::I64 && p_ty != Ty::I32 {
                    continue;
                }
                let init_v = at(&pre_args, pos);
                if const_u64(view, to_idx(init_v)) != Some(0) {
                    continue;
                }
                let back_v = at(&back_args, pos);
                if !is_step_one(view, back_v, p_inst) {
                    continue;
                }
                if let Some((bound, is_signed)) = parse_lt_cond(view, l_br.cond, back_v, stay_is_then)
                    && !at(&in_loop, to_idx(at(&cfg.inst_block, to_idx(bound))))
                {
                    found = Some((pos, p_ty, bound, is_signed));
                    break;
                }
            }
            let Some((iv_pos, iv_ty, bound, is_signed)) = found else {
                continue;
            };

            // Verify preheader either guards `0 < bound` jumping to `exit`, or `bound` is a positive constant.
            let skip_args = match pre_br_skip {
                Some((guard_cond, enter_is_then, skip_blk, sa)) => {
                    if skip_blk != exit || !preheader_guards_positive(view, guard_cond, bound, is_signed, enter_is_then)
                    {
                        continue;
                    }
                    sa
                }
                None => {
                    let Some(c) = const_u64(view, to_idx(bound)) else {
                        continue;
                    };
                    if c == 0 || (is_signed && c.cast_signed() <= 0) {
                        continue;
                    }
                    Vec::new_in(alloc)
                }
            };

            counted.push(CountedLoop {
                header,
                latch,
                preheader,
                body_entry: header,
                exit,
                is_rotated: true,
                iv_pos,
                iv_ty,
                bound,
                is_signed,
                pre_args,
                back_args,
                exit_args,
                skip_args,
                in_loop,
                block_count,
            });
        }
    }

    (counted, all_natural_loops)
}

// ---------------------------------------------------------------------------
// Iterative Affine Address & Index Analyzer (ZERO recursion)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StrideTerm {
    Zero,
    Const(u64),
    Dyn { val: u32, scale: u64 },
    Unknown,
}

impl StrideTerm {
    fn scale_const(self, k: u64) -> Self {
        if k == 0 {
            return Self::Zero;
        }
        match self {
            Self::Zero => Self::Zero,
            Self::Const(c) => c.checked_mul(k).map_or(Self::Unknown, Self::Const),
            Self::Dyn { val, scale } => scale.checked_mul(k).map_or(Self::Unknown, |s| Self::Dyn { val, scale: s }),
            Self::Unknown => Self::Unknown,
        }
    }

    fn mul_term(self, other: Self) -> Self {
        match (self, other) {
            (Self::Zero, _) | (_, Self::Zero) => Self::Zero,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Const(a), Self::Const(b)) => a.checked_mul(b).map_or(Self::Unknown, Self::Const),
            (Self::Const(c), Self::Dyn { val, scale }) | (Self::Dyn { val, scale }, Self::Const(c)) => {
                scale.checked_mul(c).map_or(Self::Unknown, |s| Self::Dyn { val, scale: s })
            }
            (Self::Dyn { .. }, Self::Dyn { .. }) => Self::Unknown,
        }
    }

    fn add_term(self, other: Self, view: &View<'_>) -> Self {
        match (self, other) {
            (Self::Zero, x) | (x, Self::Zero) => x,
            (Self::Const(a), Self::Const(b)) => a.checked_add(b).map_or(Self::Unknown, Self::Const),
            (Self::Dyn { val: v1, scale: s1 }, Self::Dyn { val: v2, scale: s2 })
                if same_val_or_const(view, v1, v2) =>
            {
                s1.checked_add(s2).map_or(Self::Unknown, |s| Self::Dyn { val: v1, scale: s })
            }
            _ => Self::Unknown,
        }
    }

    fn div_exact(self, d: u64) -> Option<DimStride> {
        if d == 0 {
            return None;
        }
        match self {
            Self::Zero => Some(DimStride::Zero),
            Self::Const(c) if c % d == 0 => Some(DimStride::Const(c / d)),
            Self::Dyn { val, scale } if scale % d == 0 => {
                let q = scale / d;
                if q == 1 { Some(DimStride::Value(val)) } else { None }
            }
            _ => None,
        }
    }
}

/// Element stride along one loop axis (after dividing byte stride by `elem_bytes`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DimStride {
    Zero,
    Const(u64),
    Value(u32),
}

/// A matrix dimension or leading-dimension stride in elements.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DimVal {
    Const(u64),
    Value(u32),
}

impl DimVal {
    fn from_stride(s: DimStride) -> Option<Self> {
        match s {
            DimStride::Zero => None,
            DimStride::Const(c) if c > 0 => Some(Self::Const(c)),
            DimStride::Value(v) => Some(Self::Value(v)),
            DimStride::Const(_) => None,
        }
    }

    fn matches_bound(self, view: &View<'_>, bound: u32) -> bool {
        match self {
            Self::Const(c) => const_u64(view, to_idx(bound)) == Some(c),
            Self::Value(v) => same_val_or_const(view, v, bound),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct AffineInt {
    coeff: [StrideTerm; 3],
    const_off: i64,
    /// When `self` is purely a loop-invariant outside value (`coeff == [Zero; 3]`), its `StrideTerm`.
    inv_term: StrideTerm,
    valid: bool,
}

impl AffineInt {
    const fn invalid() -> Self {
        Self {
            coeff: [StrideTerm::Unknown; 3],
            const_off: 0,
            inv_term: StrideTerm::Unknown,
            valid: false,
        }
    }

    const fn zero() -> Self {
        Self {
            coeff: [StrideTerm::Zero; 3],
            const_off: 0,
            inv_term: StrideTerm::Zero,
            valid: true,
        }
    }

    const fn from_const(c: i64) -> Self {
        let inv = if c == 0 {
            StrideTerm::Zero
        } else if c > 0 {
            StrideTerm::Const(c.cast_unsigned())
        } else {
            StrideTerm::Unknown
        };
        Self {
            coeff: [StrideTerm::Zero; 3],
            const_off: c,
            inv_term: inv,
            valid: true,
        }
    }

    const fn from_iv(axis: usize, stride: StrideTerm) -> Self {
        let coeff = match axis {
            0 => [stride, StrideTerm::Zero, StrideTerm::Zero],
            1 => [StrideTerm::Zero, stride, StrideTerm::Zero],
            2 => [StrideTerm::Zero, StrideTerm::Zero, stride],
            _ => [StrideTerm::Unknown; 3],
        };
        Self {
            coeff,
            const_off: 0,
            inv_term: StrideTerm::Zero,
            valid: axis < 3 && !matches!(stride, StrideTerm::Unknown),
        }
    }

    const fn is_pure_inv(&self) -> bool {
        let [c0, c1, c2] = self.coeff;
        self.valid
            && matches!(c0, StrideTerm::Zero)
            && matches!(c1, StrideTerm::Zero)
            && matches!(c2, StrideTerm::Zero)
    }

    fn scale_const(self, k: u64) -> Self {
        if !self.valid {
            return Self::invalid();
        }
        let [s0, s1, s2] = self.coeff;
        let c0 = s0.scale_const(k);
        let c1 = s1.scale_const(k);
        let c2 = s2.scale_const(k);
        if matches!(c0, StrideTerm::Unknown)
            || matches!(c1, StrideTerm::Unknown)
            || matches!(c2, StrideTerm::Unknown)
        {
            return Self::invalid();
        }
        let Some(k_i64) = i64::try_from(k).ok() else {
            return Self::invalid();
        };
        let Some(new_off) = self.const_off.checked_mul(k_i64) else {
            return Self::invalid();
        };
        let new_inv = if self.is_pure_inv() { self.inv_term.scale_const(k) } else { StrideTerm::Zero };
        Self {
            coeff: [c0, c1, c2],
            const_off: new_off,
            inv_term: new_inv,
            valid: true,
        }
    }

    fn scale_term(self, term: StrideTerm) -> Self {
        if !self.valid || matches!(term, StrideTerm::Unknown) {
            return Self::invalid();
        }
        if let StrideTerm::Const(k) = term {
            return self.scale_const(k);
        }
        if matches!(term, StrideTerm::Zero) {
            return Self::zero();
        }
        if self.const_off != 0 || !matches!(self.inv_term, StrideTerm::Zero) {
            return Self::invalid();
        }
        let [s0, s1, s2] = self.coeff;
        let c0 = s0.mul_term(term);
        let c1 = s1.mul_term(term);
        let c2 = s2.mul_term(term);
        if matches!(c0, StrideTerm::Unknown)
            || matches!(c1, StrideTerm::Unknown)
            || matches!(c2, StrideTerm::Unknown)
        {
            return Self::invalid();
        }
        Self {
            coeff: [c0, c1, c2],
            const_off: 0,
            inv_term: StrideTerm::Zero,
            valid: true,
        }
    }

    fn add(self, other: Self, view: &View<'_>) -> Self {
        if !self.valid || !other.valid {
            return Self::invalid();
        }
        // Reject adding an un-absorbed dynamic invariant offset (would shift base pointer dynamically).
        if matches!(self.inv_term, StrideTerm::Dyn { .. }) || matches!(other.inv_term, StrideTerm::Dyn { .. }) {
            return Self::invalid();
        }
        let [a0, a1, a2] = self.coeff;
        let [b0, b1, b2] = other.coeff;
        let c0 = a0.add_term(b0, view);
        let c1 = a1.add_term(b1, view);
        let c2 = a2.add_term(b2, view);
        if matches!(c0, StrideTerm::Unknown)
            || matches!(c1, StrideTerm::Unknown)
            || matches!(c2, StrideTerm::Unknown)
        {
            return Self::invalid();
        }
        let Some(const_off) = self.const_off.checked_add(other.const_off) else {
            return Self::invalid();
        };
        let inv_term = if matches!(c0, StrideTerm::Zero)
            && matches!(c1, StrideTerm::Zero)
            && matches!(c2, StrideTerm::Zero)
        {
            if const_off == 0 {
                StrideTerm::Zero
            } else if const_off > 0 {
                StrideTerm::Const(const_off.cast_unsigned())
            } else {
                StrideTerm::Unknown
            }
        } else {
            StrideTerm::Zero
        };
        Self {
            coeff: [c0, c1, c2],
            const_off,
            inv_term,
            valid: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct AffinePtr {
    base: u32,
    off: AffineInt,
}

struct NestRef<'a, A: Allocator> {
    loops: [&'a CountedLoop<A>; 3],
    in_outer: &'a [bool],
    inst_block: &'a [u32],
}

/// Evaluates an expression outside `L0` as a constant or `val * scale` `StrideTerm`.
fn eval_outside_stride(view: &View<'_>, mut val: u32, in_outer: &[bool], inst_block: &[u32]) -> StrideTerm {
    let mut scale = 1u64;
    for _ in 0..16 {
        let vi = to_idx(val);
        if at(in_outer, to_idx(at(inst_block, vi))) {
            return StrideTerm::Unknown;
        }
        if let Some(c) = const_u64(view, vi) {
            return scale.checked_mul(c).map_or(StrideTerm::Unknown, StrideTerm::Const);
        }
        match view.op(vi) {
            Op::Zext | Op::Sext => {
                val = at(view.data, vi);
            }
            Op::Shl => {
                let pos = to_idx(at(view.data, vi));
                let (a, b) = (at(view.extra, pos), at(view.extra, pos + 1));
                let Some(sh) = const_u64(view, to_idx(b)).filter(|&s| s <= 16) else {
                    return StrideTerm::Dyn { val, scale };
                };
                let Some(next_scale) = scale.checked_shl(trunc_u32(sh)) else {
                    return StrideTerm::Unknown;
                };
                scale = next_scale;
                val = a;
            }
            Op::Mul => {
                let pos = to_idx(at(view.data, vi));
                let (a, b) = (at(view.extra, pos), at(view.extra, pos + 1));
                if let Some(c) = const_u64(view, to_idx(b)).filter(|&c| c > 0) {
                    let Some(next_scale) = scale.checked_mul(c) else {
                        return StrideTerm::Unknown;
                    };
                    scale = next_scale;
                    val = a;
                } else if let Some(c) = const_u64(view, to_idx(a)).filter(|&c| c > 0) {
                    let Some(next_scale) = scale.checked_mul(c) else {
                        return StrideTerm::Unknown;
                    };
                    scale = next_scale;
                    val = b;
                } else {
                    return StrideTerm::Dyn { val, scale };
                }
            }
            _ => return StrideTerm::Dyn { val, scale },
        }
    }
    StrideTerm::Unknown
}

/// Checks if `val` is a loop-carried auxiliary linear integer IV on loop `axis` (`init == 0`,
/// `back == Add(param, inv_step)`), returning its step `StrideTerm`.
fn match_aux_int_iv<A: Allocator>(view: &View<'_>, val: u32, nest: &NestRef<'_, A>) -> Option<(usize, StrideTerm)> {
    for (axis, lp) in nest.loops.iter().enumerate() {
        let hp = view.params(Block(lp.header));
        let vi = to_idx(val);
        if vi < hp.start || vi >= hp.end {
            continue;
        }
        let pos = vi - hp.start;
        if pos == lp.iv_pos {
            return Some((axis, StrideTerm::Const(1)));
        }
        let init_v = at(&lp.pre_args, pos);
        if const_u64(view, to_idx(init_v)) != Some(0) {
            continue;
        }
        let back_v = at(&lp.back_args, pos);
        let bi = to_idx(back_v);
        if view.op(bi) != Op::Add {
            continue;
        }
        let bp = to_idx(at(view.data, bi));
        let (a, b) = (at(view.extra, bp), at(view.extra, bp + 1));
        let step_v = if a == val {
            b
        } else if b == val {
            a
        } else {
            continue;
        };
        let step = eval_outside_stride(view, step_v, nest.in_outer, nest.inst_block);
        if !matches!(step, StrideTerm::Unknown) {
            return Some((axis, step));
        }
    }
    None
}

#[derive(Clone, Copy)]
enum AffFrame {
    Empty,
    ShlWait { shift_val: u32 },
    AddWaitLhs { rhs: u32, depth: u32 },
    AddWaitRhs { lhs: AffineInt },
    MulWaitLhs { rhs: u32, depth: u32 },
    MulWaitRhs { lhs: AffineInt },
}

/// Evaluates an integer SSA expression iteratively as an affine function of `(iv0, iv1, iv2)`.
fn eval_affine_int<A: Allocator>(view: &View<'_>, root: u32, nest: &NestRef<'_, A>) -> AffineInt {
    let mut frames = [AffFrame::Empty; 24];
    let mut sp = 0usize;
    let mut cur = root;
    let mut depth = 0u32;

    for _ in 0..256 {
        let mut res_opt: Option<AffineInt> = None;
        for _ in 0..32 {
            if depth > 16 {
                res_opt = Some(AffineInt::invalid());
                break;
            }
            let vi = to_idx(cur);
            if let Some(c) = const_u64(view, vi) {
                res_opt = Some(i64::try_from(c).map_or(AffineInt::invalid(), AffineInt::from_const));
                break;
            }
            if !at(nest.in_outer, to_idx(at(nest.inst_block, vi))) {
                let term = eval_outside_stride(view, cur, nest.in_outer, nest.inst_block);
                res_opt = Some(if matches!(term, StrideTerm::Unknown) {
                    AffineInt::invalid()
                } else {
                    AffineInt {
                        coeff: [StrideTerm::Zero; 3],
                        const_off: 0,
                        inv_term: term,
                        valid: true,
                    }
                });
                break;
            }
            if let Some((axis, step)) = match_aux_int_iv(view, cur, nest) {
                res_opt = Some(AffineInt::from_iv(axis, step));
                break;
            }
            match view.op(vi) {
                Op::Zext | Op::Sext => {
                    cur = at(view.data, vi);
                    depth += 1;
                }
                Op::Shl => {
                    let pos = to_idx(at(view.data, vi));
                    let (a, b) = (at(view.extra, pos), at(view.extra, pos + 1));
                    if sp >= frames.len() {
                        res_opt = Some(AffineInt::invalid());
                        break;
                    }
                    set_at(&mut frames, sp, AffFrame::ShlWait { shift_val: b });
                    sp += 1;
                    cur = a;
                    depth += 1;
                }
                Op::Add => {
                    let pos = to_idx(at(view.data, vi));
                    let (a, b) = (at(view.extra, pos), at(view.extra, pos + 1));
                    if sp >= frames.len() {
                        res_opt = Some(AffineInt::invalid());
                        break;
                    }
                    set_at(&mut frames, sp, AffFrame::AddWaitLhs { rhs: b, depth: depth + 1 });
                    sp += 1;
                    cur = a;
                    depth += 1;
                }
                Op::Mul => {
                    let pos = to_idx(at(view.data, vi));
                    let (a, b) = (at(view.extra, pos), at(view.extra, pos + 1));
                    if sp >= frames.len() {
                        res_opt = Some(AffineInt::invalid());
                        break;
                    }
                    set_at(&mut frames, sp, AffFrame::MulWaitLhs { rhs: b, depth: depth + 1 });
                    sp += 1;
                    cur = a;
                    depth += 1;
                }
                _ => {
                    res_opt = Some(AffineInt::invalid());
                    break;
                }
            }
        }

        let Some(mut res) = res_opt else {
            return AffineInt::invalid();
        };
        let mut descend_rhs = false;
        while sp > 0 {
            sp -= 1;
            let frame = frames.get(sp).copied().unwrap_or(AffFrame::Empty);
            match frame {
                AffFrame::Empty => return AffineInt::invalid(),
                AffFrame::ShlWait { shift_val } => {
                    res = if let Some(sh) = const_u64(view, to_idx(shift_val)).filter(|&s| s <= 16) {
                        res.scale_const(1u64 << sh)
                    } else {
                        AffineInt::invalid()
                    };
                }
                AffFrame::AddWaitLhs { rhs, depth: rd } => {
                    if !res.valid {
                        return AffineInt::invalid();
                    }
                    set_at(&mut frames, sp, AffFrame::AddWaitRhs { lhs: res });
                    sp += 1;
                    cur = rhs;
                    depth = rd;
                    descend_rhs = true;
                    break;
                }
                AffFrame::AddWaitRhs { lhs } => {
                    res = lhs.add(res, view);
                }
                AffFrame::MulWaitLhs { rhs, depth: rd } => {
                    if !res.valid {
                        return AffineInt::invalid();
                    }
                    set_at(&mut frames, sp, AffFrame::MulWaitRhs { lhs: res });
                    sp += 1;
                    cur = rhs;
                    depth = rd;
                    descend_rhs = true;
                    break;
                }
                AffFrame::MulWaitRhs { lhs } => {
                    res = if lhs.is_pure_inv() {
                        res.scale_term(lhs.inv_term)
                    } else if res.is_pure_inv() {
                        lhs.scale_term(res.inv_term)
                    } else {
                        AffineInt::invalid()
                    };
                }
            }
        }
        if !descend_rhs {
            return res;
        }
    }
    AffineInt::invalid()
}

/// Evaluates a pointer expression `ptr + mem_off` iteratively into `(base_outside_L0, byte_affine)`.
fn eval_affine_ptr<A: Allocator>(
    view: &View<'_>,
    mut ptr: u32,
    mem_off: i32,
    nest: &NestRef<'_, A>,
) -> Option<AffinePtr> {
    let mut acc = AffineInt::from_const(i64::from(mem_off));
    for _ in 0..16 {
        let pi = to_idx(ptr);
        if !at(nest.in_outer, to_idx(at(nest.inst_block, pi))) {
            return if acc.valid && Ty(at(view.tys, pi)) == Ty::PTR {
                Some(AffinePtr { base: ptr, off: acc })
            } else {
                None
            };
        }
        if view.op(pi) == Op::PtrAdd {
            let pos = to_idx(at(view.data, pi));
            let base = at(view.extra, pos);
            let by = at(view.extra, pos + 1);
            let step = eval_affine_int(view, by, nest);
            acc = acc.add(step, view);
            if !acc.valid {
                return None;
            }
            ptr = base;
            continue;
        }
        // Check strength-reduced pointer IV on `L0/L1/L2`: `p_next = PtrAdd(p, inv_step)`.
        let mut matched_piv = None;
        for (axis, lp) in nest.loops.iter().enumerate() {
            let hp = view.params(Block(lp.header));
            if pi < hp.start || pi >= hp.end {
                continue;
            }
            let pos = pi - hp.start;
            let back_v = at(&lp.back_args, pos);
            let bi = to_idx(back_v);
            if view.op(bi) != Op::PtrAdd {
                continue;
            }
            let bp = to_idx(at(view.data, bi));
            let (bp_base, bp_by) = (at(view.extra, bp), at(view.extra, bp + 1));
            if bp_base != ptr {
                continue;
            }
            let step = eval_outside_stride(view, bp_by, nest.in_outer, nest.inst_block);
            if !matches!(step, StrideTerm::Unknown) {
                matched_piv = Some((axis, step, at(&lp.pre_args, pos)));
                break;
            }
        }
        if let Some((axis, step, init_ptr)) = matched_piv {
            acc = acc.add(AffineInt::from_iv(axis, step), view);
            if !acc.valid {
                return None;
            }
            ptr = init_ptr;
            continue;
        }
        return None;
    }
    None
}

// ---------------------------------------------------------------------------
// Triple-Nested GEMM Recognition (`detect_gemm_nest`)
// ---------------------------------------------------------------------------

/// Recognized element/accumulator precision kind for a GEMM loop nest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GemmElem {
    F32,
    F64,
    I32,
    /// Widening signed `i8 * i8 -> i32` GEMM (`SMOPA`).
    I8ToI32,
    /// Widening unsigned `u8 * u8 -> u32` GEMM (`UMOPA`).
    U8ToU32,
}

impl GemmElem {
    #[inline]
    const fn acc_ty(self) -> Ty {
        match self {
            Self::F32 => Ty::F32,
            Self::F64 => Ty::F64,
            Self::I32 | Self::I8ToI32 | Self::U8ToU32 => Ty::I32,
        }
    }

    #[inline]
    const fn acc_bytes(self) -> u64 {
        match self {
            Self::F64 => 8,
            Self::F32 | Self::I32 | Self::I8ToI32 | Self::U8ToU32 => 4,
        }
    }

    #[inline]
    const fn acc_shift(self) -> u32 {
        match self {
            Self::F64 => 3,
            Self::F32 | Self::I32 | Self::I8ToI32 | Self::U8ToU32 => 2,
        }
    }

    #[inline]
    const fn input_bytes(self) -> u64 {
        match self {
            Self::F64 => 8,
            Self::F32 | Self::I32 => 4,
            Self::I8ToI32 | Self::U8ToU32 => 1,
        }
    }

    #[inline]
    const fn input_shift(self) -> u32 {
        match self {
            Self::F64 => 3,
            Self::F32 | Self::I32 => 2,
            Self::I8ToI32 | Self::U8ToU32 => 0,
        }
    }

    #[inline]
    const fn sme2_intrinsic(self) -> Option<&'static str> {
        match self {
            Self::F32 => Some(SME2_GEMM_F32_INTRINSIC),
            Self::F64 => Some(SME2_GEMM_F64_INTRINSIC),
            Self::I8ToI32 => Some(SME2_GEMM_I8_I32_INTRINSIC),
            Self::U8ToU32 => Some(SME2_GEMM_U8_U32_INTRINSIC),
            Self::I32 => None,
        }
    }

    #[inline]
    const fn supports_neon_4x16(self) -> bool {
        matches!(self, Self::F32 | Self::F64 | Self::I32)
    }
}

struct GemmKernel<A: Allocator> {
    elem: GemmElem,
    a_ptr: u32,
    a_const_off: i64,
    b_ptr: u32,
    b_const_off: i64,
    c_ptr: u32,
    c_const_off: i64,
    m_dim: u32,
    m_ty: Ty,
    m_signed: bool,
    n_dim: u32,
    n_ty: Ty,
    n_signed: bool,
    k_dim: u32,
    k_ty: Ty,
    k_signed: bool,
    lda: DimVal,
    ldb: DimVal,
    ldc: DimVal,
    is_accumulate_into_c: bool,
    fma_flags: Flags,
    use_fused_fma: bool,
    outer_preheader: u32,
    outer_header: u32,
    outer_exit: u32,
    outer_pre_args: Vec<u32, A>,
    outer_exit_args: Vec<u32, A>,
    outer_blocks: Vec<bool, A>,
}

/// Checks if `sub` is a strict subset of `sup`.
fn is_strict_subset(sub: &[bool], sup: &[bool]) -> bool {
    let mut strict = false;
    for i in 0..sub.len() {
        let a = at(sub, i);
        let b = at(sup, i);
        if a && !b {
            return false;
        }
        if b && !a {
            strict = true;
        }
    }
    strict
}

/// Checks if `a` and `b` have no overlapping blocks.
fn is_disjoint_sets(a: &[bool], b: &[bool]) -> bool {
    for i in 0..a.len() {
        if at(a, i) && at(b, i) {
            return false;
        }
    }
    true
}

/// Checks whether two element strides are provably equal in `view`.
fn same_dim_stride(view: &View<'_>, a: DimStride, b: DimStride) -> bool {
    match (a, b) {
        (DimStride::Zero, DimStride::Zero) => true,
        (DimStride::Const(x), DimStride::Const(y)) => x == y,
        (DimStride::Value(x), DimStride::Value(y)) => same_val_or_const(view, x, y),
        (DimStride::Const(c), DimStride::Value(v)) | (DimStride::Value(v), DimStride::Const(c)) => {
            const_u64(view, to_idx(v)) == Some(c)
        }
        _ => false,
    }
}

/// Checks that every block inside `outer` has straight-line control flow except for the loop
/// headers/latches/rotated-preheaders of `L0`, `L1`, `L2` (and optional preceding sibling
/// row-zero-init loop `l_zero`), and that no edge exits `L0` except to `L0.exit` with
/// outside-defined arguments matching `L0.exit_args`.
fn verify_nest_cfg<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &CfgInfo<A>,
    l0: &CountedLoop<A>,
    l1: &CountedLoop<A>,
    l2: &CountedLoop<A>,
    l_zero: Option<&CountedLoop<A>>,
    all_natural_loops: &[(u32, u32)],
) -> bool {
    let blocks = view.blocks();
    // 1. Strict nesting: L2 ⊂ L1 ⊂ L0, and optional sibling `l_zero ⊂ L0` disjoint from `L1`.
    if !is_strict_subset(&l2.in_loop, &l1.in_loop) || !is_strict_subset(&l1.in_loop, &l0.in_loop) {
        return false;
    }
    if let Some(lz) = l_zero
        && (!is_strict_subset(&lz.in_loop, &l0.in_loop) || !is_disjoint_sets(&lz.in_loop, &l1.in_loop))
    {
        return false;
    }
    // 2. No other natural loops inside L0.
    for &(h, l) in all_natural_loops {
        if at(&l0.in_loop, to_idx(h)) {
            let is_known = (h == l0.header && l == l0.latch)
                || (h == l1.header && l == l1.latch)
                || (h == l2.header && l == l2.latch)
                || l_zero.is_some_and(|lz| h == lz.header && l == lz.latch);
            if !is_known {
                return false;
            }
        }
    }
    // 3. L1 preheader & exit live inside L0 \ (L1 ∪ l_zero); L2 preheader & exit live inside L1 \ L2;
    //    optional `l_zero` preheader & exit live inside L0 \ (l_zero ∪ L1).
    if !at(&l0.in_loop, to_idx(l1.preheader))
        || at(&l1.in_loop, to_idx(l1.preheader))
        || !at(&l0.in_loop, to_idx(l1.exit))
        || at(&l1.in_loop, to_idx(l1.exit))
    {
        return false;
    }
    if let Some(lz) = l_zero {
        let direct_lz_to_l1 = lz.exit == l1.header
            && (l1.preheader == lz.header || (lz.is_rotated && l1.preheader == lz.latch));
        if (!direct_lz_to_l1 && at(&lz.in_loop, to_idx(l1.preheader)))
            || at(&lz.in_loop, to_idx(l1.exit))
            || !at(&l0.in_loop, to_idx(lz.preheader))
            || at(&lz.in_loop, to_idx(lz.preheader))
            || at(&l1.in_loop, to_idx(lz.preheader))
            || !at(&l0.in_loop, to_idx(lz.exit))
            || at(&lz.in_loop, to_idx(lz.exit))
            || (!direct_lz_to_l1 && at(&l1.in_loop, to_idx(lz.exit)))
        {
            return false;
        }
    }
    if !at(&l1.in_loop, to_idx(l2.preheader))
        || at(&l2.in_loop, to_idx(l2.preheader))
        || !at(&l1.in_loop, to_idx(l2.exit))
        || at(&l2.in_loop, to_idx(l2.exit))
    {
        return false;
    }
    // 4. Dominance along the nest spine (with optional `l_zero` strictly preceding `l1`).
    if !cfg.dominates(l0.header, l1.header)
        || !cfg.dominates(l1.header, l2.header)
        || !cfg.dominates(l1.exit, l0.latch)
        || !cfg.dominates(l2.exit, l1.latch)
    {
        return false;
    }
    if let Some(lz) = l_zero {
        let direct_lz_to_l1 = lz.exit == l1.header
            && (l1.preheader == lz.header || (lz.is_rotated && l1.preheader == lz.latch));
        let precedes_l1 = if direct_lz_to_l1 {
            cfg.dominates(lz.header, l1.header)
        } else {
            cfg.dominates(lz.exit, l1.preheader)
        };
        if !cfg.dominates(l0.header, lz.header) || !precedes_l1 {
            return false;
        }
    }
    // 5. Innermost loop L2 (and optional `l_zero`) has exact canonical shape (1 block if rotated, 2 if HeaderBody).
    if l2.is_rotated {
        if l2.block_count != 1 || l2.header != l2.latch {
            return false;
        }
    } else if l2.block_count != 2 || l2.body_entry != l2.latch || !view.params(Block(l2.body_entry)).is_empty() {
        return false;
    }
    if let Some(lz) = l_zero {
        if lz.is_rotated {
            if lz.block_count != 1 || lz.header != lz.latch {
                return false;
            }
        } else if lz.block_count != 2
            || lz.body_entry != lz.latch
            || !view.params(Block(lz.body_entry)).is_empty()
        {
            return false;
        }
    }

    // 6. Verify every block inside L0 only uses approved terminators and never jumps outside L0
    //    except to `l0.exit`.
    for b in 0..blocks {
        if !at(&l0.in_loop, b) {
            continue;
        }
        let b32 = to_u32(b);
        if view.block_kind(Block(b32)) != BlockKind::Normal {
            return false;
        }
        let term = view.terminator(Block(b32));
        let op = view.op(term);
        let allowed_br = (b32 == l0.header && !l0.is_rotated)
            || (b32 == l0.latch && l0.is_rotated)
            || (b32 == l1.preheader && l1.is_rotated)
            || (b32 == l1.header && !l1.is_rotated)
            || (b32 == l1.latch && l1.is_rotated)
            || (b32 == l2.preheader && l2.is_rotated)
            || (b32 == l2.header && !l2.is_rotated)
            || (b32 == l2.latch && l2.is_rotated)
            || l_zero.is_some_and(|lz| {
                (b32 == lz.preheader && lz.is_rotated)
                    || (b32 == lz.header && !lz.is_rotated)
                    || (b32 == lz.latch && lz.is_rotated)
            });
        if op == Op::Br {
            if !allowed_br {
                return false;
            }
        } else if op != Op::Jump {
            return false;
        }

        // Check every outgoing edge from `b`.
        let mut exit_ok = true;
        each(view, term, |item| {
            if let Item::Edge(edge) = item
                && !at(&l0.in_loop, to_idx(edge.block))
                && edge.block != l0.exit
            {
                exit_ok = false;
            }
        });
        if !exit_ok {
            return false;
        }
    }

    // 7. No SSA value defined inside L0 may be used outside L0 (ensures L0's only observable
    //    effect is writing matrix C in memory).
    for &arg in &l0.exit_args {
        if at(&l0.in_loop, to_idx(at(&cfg.inst_block, to_idx(arg)))) {
            return false;
        }
    }
    for b in 0..blocks {
        if at(&l0.in_loop, b) {
            continue;
        }
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            let mut bad = false;
            for_each_use(view, inst, |v| {
                if at(&l0.in_loop, to_idx(at(&cfg.inst_block, to_idx(v)))) {
                    bad = true;
                }
            });
            if bad {
                return false;
            }
        }
    }

    true
}

/// Extracts `(mul_lhs, mul_rhs, acc_in, fma_flags, use_fused_fma)` from a multiply-accumulate
/// instruction supporting `Op::Fma`, `Op::Fmuladd`, `Op::Fadd(Op::Fmul(a, b), acc)` (with
/// `use_fused_fma = fl.contains(REASSOC | CONTRACT)` so strict IEEE 754 `fmul + fadd` stays
/// bit-exact via separate `Fmul` + `Fadd` in 4x16 NEON and never contracts via SME2 `FMOPA`),
/// and `Op::Add(Op::Mul(a, b), acc)` (for `Ty::I32`).
fn match_mac(view: &View<'_>, mac: u32, elem_ty: Ty) -> Option<(u32, u32, u32, Flags, bool)> {
    let mi = to_idx(mac);
    if Ty(at(view.tys, mi)) != elem_ty {
        return None;
    }
    let op = view.op(mi);
    let fl = view.flags(mi);
    if elem_ty.is_float() {
        if matches!(op, Op::Fma | Op::Fmuladd) {
            let pos = to_idx(at(view.data, mi));
            let (a, b, c) = (at(view.extra, pos), at(view.extra, pos + 1), at(view.extra, pos + 2));
            return Some((a, b, c, fl, true));
        }
        if op == Op::Fadd {
            let fused = fl.contains(Flags::REASSOC) || fl.contains(Flags::CONTRACT);
            let pos = to_idx(at(view.data, mi));
            let (x, y) = (at(view.extra, pos), at(view.extra, pos + 1));
            let xi = to_idx(x);
            let yi = to_idx(y);
            if view.op(xi) == Op::Fmul {
                let mp = to_idx(at(view.data, xi));
                return Some((at(view.extra, mp), at(view.extra, mp + 1), y, fl, fused));
            }
            if view.op(yi) == Op::Fmul {
                let mp = to_idx(at(view.data, yi));
                return Some((at(view.extra, mp), at(view.extra, mp + 1), x, fl, fused));
            }
        }
        None
    } else if elem_ty == Ty::I32 && op == Op::Add {
        let pos = to_idx(at(view.data, mi));
        let (x, y) = (at(view.extra, pos), at(view.extra, pos + 1));
        let xi = to_idx(x);
        let yi = to_idx(y);
        if view.op(xi) == Op::Mul {
            let mp = to_idx(at(view.data, xi));
            return Some((at(view.extra, mp), at(view.extra, mp + 1), y, Flags::NONE, true));
        }
        if view.op(yi) == Op::Mul {
            let mp = to_idx(at(view.data, yi));
            return Some((at(view.extra, mp), at(view.extra, mp + 1), x, Flags::NONE, true));
        }
        None
    } else {
        None
    }
}

/// Checks whether `(a_str, b_str, c_str)` match canonical GEMM role strides across loop axes
/// `(i_ax, k_ax, j_ax)`:
/// - `C[i * ldc + j]`: `c[i_ax] = ldc`, `c[k_ax] = 0`, `c[j_ax] = 1`
/// - `A[i * lda + k]`: `a[i_ax] = lda`, `a[k_ax] = 1`, `a[j_ax] = 0`
/// - `B[k * ldb + j]`: `b[i_ax] = 0`, `b[k_ax] = ldb`, `b[j_ax] = 1`
fn match_gemm_roles(
    a_str: [DimStride; 3],
    b_str: [DimStride; 3],
    c_str: [DimStride; 3],
) -> Option<(usize, usize, usize, DimVal, DimVal, DimVal)> {
    // Supported loop orders (prioritizing i-k-j and i-j-k, plus all remaining permutations).
    const ORDERS: [(usize, usize, usize); 6] =
        [(0, 1, 2), (0, 2, 1), (1, 0, 2), (1, 2, 0), (2, 0, 1), (2, 1, 0)];
    for (i_ax, k_ax, j_ax) in ORDERS {
        let c_i = c_str.get(i_ax).copied().unwrap_or(DimStride::Zero);
        let c_k = c_str.get(k_ax).copied().unwrap_or(DimStride::Zero);
        let c_j = c_str.get(j_ax).copied().unwrap_or(DimStride::Zero);
        let a_i = a_str.get(i_ax).copied().unwrap_or(DimStride::Zero);
        let a_k = a_str.get(k_ax).copied().unwrap_or(DimStride::Zero);
        let a_j = a_str.get(j_ax).copied().unwrap_or(DimStride::Zero);
        let b_i = b_str.get(i_ax).copied().unwrap_or(DimStride::Zero);
        let b_k = b_str.get(k_ax).copied().unwrap_or(DimStride::Zero);
        let b_j = b_str.get(j_ax).copied().unwrap_or(DimStride::Zero);

        if c_k == DimStride::Zero
            && c_j == DimStride::Const(1)
            && a_j == DimStride::Zero
            && a_k == DimStride::Const(1)
            && b_i == DimStride::Zero
            && b_j == DimStride::Const(1)
            && let (Some(lda), Some(ldb), Some(ldc)) = (
                DimVal::from_stride(a_i),
                DimVal::from_stride(b_k),
                DimVal::from_stride(c_i),
            )
        {
            return Some((i_ax, k_ax, j_ax, lda, ldb, ldc));
        }
    }
    None
}

fn affine_to_elem_strides(aff: AffinePtr, elem_bytes: u64) -> Option<[DimStride; 3]> {
    if !aff.off.valid || matches!(aff.off.inv_term, StrideTerm::Dyn { .. } | StrideTerm::Unknown) {
        return None;
    }
    let [c0, c1, c2] = aff.off.coeff;
    let s0 = c0.div_exact(elem_bytes)?;
    let s1 = c1.div_exact(elem_bytes)?;
    let s2 = c2.div_exact(elem_bytes)?;
    Some([s0, s1, s2])
}

/// Extracts `(elem_kind, lhs_load, rhs_load)` from the two multiplicands of `match_mac`,
/// supporting uniform `f32`/`f64`/`i32` loads as well as widening `Sext(load i8)` (`i8 -> i32`)
/// and `Zext(load i8)` (`u8 -> u32`) inputs.
fn extract_gemm_input_loads(
    view: &View<'_>,
    lhs_val: u32,
    rhs_val: u32,
    acc_ty: Ty,
    in_outer: &[bool],
    inst_block: &[u32],
) -> Option<(GemmElem, u32, u32)> {
    let li = to_idx(lhs_val);
    let ri = to_idx(rhs_val);
    if !at(in_outer, to_idx(at(inst_block, li))) || !at(in_outer, to_idx(at(inst_block, ri))) {
        return None;
    }
    let lop = view.op(li);
    let rop = view.op(ri);
    if acc_ty == Ty::F32 {
        if lhs_val != rhs_val
            && lop == Op::Load
            && rop == Op::Load
            && Ty(at(view.tys, li)) == Ty::F32
            && Ty(at(view.tys, ri)) == Ty::F32
        {
            return Some((GemmElem::F32, lhs_val, rhs_val));
        }
        return None;
    }
    if acc_ty == Ty::F64 {
        if lhs_val != rhs_val
            && lop == Op::Load
            && rop == Op::Load
            && Ty(at(view.tys, li)) == Ty::F64
            && Ty(at(view.tys, ri)) == Ty::F64
        {
            return Some((GemmElem::F64, lhs_val, rhs_val));
        }
        return None;
    }
    if acc_ty == Ty::I32 {
        if lhs_val != rhs_val
            && lop == Op::Load
            && rop == Op::Load
            && Ty(at(view.tys, li)) == Ty::I32
            && Ty(at(view.tys, ri)) == Ty::I32
        {
            return Some((GemmElem::I32, lhs_val, rhs_val));
        }
        if (lop == Op::Sext && rop == Op::Sext) || (lop == Op::Zext && rop == Op::Zext) {
            let l_src = at(view.data, li);
            let r_src = at(view.data, ri);
            let lsi = to_idx(l_src);
            let rsi = to_idx(r_src);
            if l_src != r_src
                && at(in_outer, to_idx(at(inst_block, lsi)))
                && at(in_outer, to_idx(at(inst_block, rsi)))
                && view.op(lsi) == Op::Load
                && view.op(rsi) == Op::Load
                && Ty(at(view.tys, lsi)) == Ty::I8
                && Ty(at(view.tys, rsi)) == Ty::I8
            {
                let elem = if lop == Op::Sext { GemmElem::I8ToI32 } else { GemmElem::U8ToU32 };
                return Some((elem, l_src, r_src));
            }
        }
    }
    None
}

/// Attempts to recognize a triple-nested GEMM kernel rooted at `outer_idx` among `counted` loops
/// (including an optional preceding sibling row-zero-init loop `for j in 0..N { C[i*ldc+j] = 0 }`
/// inside `outer`, which turns the nest into an overwrite GEMM `C = A * B` with `flags = 0`).
fn detect_gemm_nest<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &CfgInfo<A>,
    counted: &[CountedLoop<A>],
    all_natural_loops: &[(u32, u32)],
    outer_idx: usize,
    alloc: A,
) -> Option<GemmKernel<A>> {
    let l0 = counted.get(outer_idx)?;

    // 1. Find the unique middle loop `l1` inside `l0` (contains a child counted loop).
    let mut mid_idx: Option<usize> = None;
    for (idx, cand) in counted.iter().enumerate() {
        if idx == outer_idx || !is_strict_subset(&cand.in_loop, &l0.in_loop) {
            continue;
        }
        let mut contains_child = false;
        for (j, other) in counted.iter().enumerate() {
            if j != idx && is_strict_subset(&other.in_loop, &cand.in_loop) {
                contains_child = true;
                break;
            }
        }
        if contains_child {
            if mid_idx.is_some() {
                return None;
            }
            mid_idx = Some(idx);
        }
    }
    let mid_i = mid_idx?;
    let l1 = counted.get(mid_i)?;

    // 2. Partition remaining counted loops strictly inside `l0` into the unique `l2 ⊂ l1` and at
    //    most one sibling leaf loop `l_zero ⊂ l0` disjoint from `l1`.
    let mut inner_idx: Option<usize> = None;
    let mut zero_idx: Option<usize> = None;
    for (idx, cand) in counted.iter().enumerate() {
        if idx == outer_idx || idx == mid_i || !is_strict_subset(&cand.in_loop, &l0.in_loop) {
            continue;
        }
        if is_strict_subset(&cand.in_loop, &l1.in_loop) {
            if inner_idx.is_some() {
                return None;
            }
            inner_idx = Some(idx);
        } else if is_disjoint_sets(&cand.in_loop, &l1.in_loop) {
            if zero_idx.is_some() {
                return None;
            }
            zero_idx = Some(idx);
        } else {
            return None;
        }
    }
    let l2 = counted.get(inner_idx?)?;
    let l_zero = match zero_idx {
        Some(zi) => Some(counted.get(zi)?),
        None => None,
    };

    if !verify_nest_cfg(view, cfg, l0, l1, l2, l_zero, all_natural_loops) {
        return None;
    }

    // All loop bounds must dominate `l0.preheader` and lie outside `l0`.
    for &bval in &[l0.bound, l1.bound, l2.bound] {
        let bb = at(&cfg.inst_block, to_idx(bval));
        if at(&l0.in_loop, to_idx(bb)) || !cfg.dominates(bb, l0.preheader) {
            return None;
        }
    }
    if let Some(lz) = l_zero {
        let bb = at(&cfg.inst_block, to_idx(lz.bound));
        if at(&l0.in_loop, to_idx(bb)) || !cfg.dominates(bb, l0.preheader) {
            return None;
        }
    }

    // Scan every instruction in `l0`: collect all loads & stores and reject any other side effect.
    let mut loads: Vec<u32, A> = Vec::new_in(alloc);
    let mut stores: Vec<u32, A> = Vec::new_in(alloc);
    for b in 0..view.blocks() {
        if !at(&l0.in_loop, b) {
            continue;
        }
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            let op = view.op(inst);
            match op {
                Op::Param | Op::Jump | Op::Br => {}
                Op::Load => {
                    if view.flags(inst).contains(Flags::VOLATILE) {
                        return None;
                    }
                    loads.push(to_u32(inst));
                }
                Op::Store => {
                    if view.flags(inst).contains(Flags::VOLATILE) {
                        return None;
                    }
                    stores.push(to_u32(inst));
                }
                _ => {
                    if !is_pure_op(op) {
                        return None;
                    }
                }
            }
        }
    }
    let (store_c, zero_store_opt) = if let Some(lz) = l_zero {
        if stores.len() != 2 || !(loads.len() == 2 || loads.len() == 3) {
            return None;
        }
        for &ld in &loads {
            if at(&lz.in_loop, to_idx(at(&cfg.inst_block, to_idx(ld)))) {
                return None;
            }
        }
        let s0 = at(&stores, 0);
        let s1 = at(&stores, 1);
        let s0_in_z = at(&lz.in_loop, to_idx(at(&cfg.inst_block, to_idx(s0))));
        let s1_in_z = at(&lz.in_loop, to_idx(at(&cfg.inst_block, to_idx(s1))));
        if s0_in_z && !s1_in_z {
            (s1, Some(s0))
        } else if s1_in_z && !s0_in_z {
            (s0, Some(s1))
        } else {
            return None;
        }
    } else {
        if stores.len() != 1 || !(loads.len() == 2 || loads.len() == 3) {
            return None;
        }
        (at(&stores, 0), None)
    };

    let st_i = to_idx(store_c);
    let st_pos = to_idx(at(view.data, st_i));
    let store_val = at(view.extra, st_pos);
    let store_ptr = at(view.extra, st_pos + 1);
    let store_off = at(view.extra, st_pos + 2).cast_signed();
    let acc_ty = Ty(at(view.tys, to_idx(store_val)));
    let acc_bytes: u64 = if acc_ty == Ty::F32 || acc_ty == Ty::I32 {
        4
    } else if acc_ty == Ty::F64 {
        8
    } else {
        return None;
    };

    let nest = NestRef {
        loops: [l0, l1, l2],
        in_outer: &l0.in_loop,
        inst_block: &cfg.inst_block,
    };

    let c_aff = eval_affine_ptr(view, store_ptr, store_off, &nest)?;
    let store_blk = at(&cfg.inst_block, st_i);

    // Match the MAC pattern: either Case A (`store_c` inside `L2`, RMW `C += A*B`), or Case B
    // (`store_c` inside `L1 \ L2` after `L2`, register-accumulated dot-product in `L2`).
    let (lhs_val, rhs_val, mut is_accumulate_into_c, load_c_opt, fma_flags, use_fused_fma) =
        if at(&l2.in_loop, to_idx(store_blk)) {
            // Case A: RMW inside L2 (`i-k-j` or unhoisted `i-j-k`).
            let (a_v, b_v, acc_in, fl, fused) = match_mac(view, store_val, acc_ty)?;
            if view.op(to_idx(acc_in)) != Op::Load {
                return None;
            }
            (a_v, b_v, true, Some(acc_in), fl, fused)
        } else if at(&l1.in_loop, to_idx(store_blk)) && cfg.dominates(l2.exit, store_blk) {
            // Case B: Register-accumulated dot product in `L2`, stored to `C` in `L1` after `L2`.
            let l2_params = view.params(Block(l2.header));
            let exit_params = view.params(Block(l2.exit));
            let sv_i = to_idx(store_val);

            // Identify the `L2.header` accumulator param position `acc_pos`.
            let (acc_pos, skip_init_opt) = if sv_i >= exit_params.start && sv_i < exit_params.end {
                let epos = sv_i - exit_params.start;
                let ex_v = at(&l2.exit_args, epos);
                let skip_v = if l2.is_rotated && !l2.skip_args.is_empty() {
                    Some(at(&l2.skip_args, epos))
                } else {
                    None
                };
                let mut found_pos = None;
                for p in 0..l2_params.len() {
                    if p == l2.iv_pos {
                        continue;
                    }
                    let p_inst = to_u32(l2_params.start + p);
                    let back_v = at(&l2.back_args, p);
                    if (!l2.is_rotated && ex_v == p_inst) || (l2.is_rotated && ex_v == back_v) {
                        found_pos = Some(p);
                        break;
                    }
                }
                (found_pos?, skip_v)
            } else {
                let mut found_pos = None;
                for p in 0..l2_params.len() {
                    if p == l2.iv_pos {
                        continue;
                    }
                    let p_inst = to_u32(l2_params.start + p);
                    let back_v = at(&l2.back_args, p);
                    if (!l2.is_rotated && store_val == p_inst) || (l2.is_rotated && store_val == back_v) {
                        found_pos = Some(p);
                        break;
                    }
                }
                (found_pos?, None)
            };

            let acc_param = to_u32(l2_params.start + acc_pos);
            let acc_init = at(&l2.pre_args, acc_pos);
            if let Some(sk) = skip_init_opt
                && !same_val_or_const(view, sk, acc_init)
                && !(is_zero_elem_const(view, to_idx(sk), acc_ty)
                    && is_zero_elem_const(view, to_idx(acc_init), acc_ty))
            {
                return None;
            }
            let acc_back = at(&l2.back_args, acc_pos);
            let (a_v, b_v, acc_in, fl, fused) = match_mac(view, acc_back, acc_ty)?;
            if acc_in != acc_param {
                return None;
            }
            if is_zero_elem_const(view, to_idx(acc_init), acc_ty) {
                (a_v, b_v, false, None, fl, fused)
            } else if view.op(to_idx(acc_init)) == Op::Load {
                (a_v, b_v, true, Some(acc_init), fl, fused)
            } else {
                return None;
            }
        } else {
            return None;
        };

    // Resolve the two input matrix loads (uniform `f32`/`f64`/`i32` or widening `Sext`/`Zext` from `i8`).
    let (elem, lhs_load, rhs_load) =
        extract_gemm_input_loads(view, lhs_val, rhs_val, acc_ty, &l0.in_loop, &cfg.inst_block)?;

    // Verify every collected load is accounted for (exact 2 inputs `lhs_load`, `rhs_load` + optional `load_c`).
    let expected_loads = if load_c_opt.is_some() { 3 } else { 2 };
    if loads.len() != expected_loads {
        return None;
    }
    for &ld in &loads {
        if ld != lhs_load && ld != rhs_load && Some(ld) != load_c_opt {
            return None;
        }
    }

    // If accumulating into `C`, verify `load_c`'s affine address equals `store_c`'s affine address!
    if let Some(load_c) = load_c_opt {
        let lci = to_idx(load_c);
        if Ty(at(view.tys, lci)) != acc_ty {
            return None;
        }
        let lcp = to_idx(at(view.data, lci));
        let lc_aff = eval_affine_ptr(
            view,
            at(view.extra, lcp),
            at(view.extra, lcp + 1).cast_signed(),
            &nest,
        )?;
        if lc_aff != c_aff {
            return None;
        }
    }

    // Evaluate affine addresses of the two multiplicand loads (byte stride = `elem.input_bytes()`).
    let eval_load_aff = |ld: u32| -> Option<AffinePtr> {
        let li = to_idx(ld);
        let lp = to_idx(at(view.data, li));
        eval_affine_ptr(view, at(view.extra, lp), at(view.extra, lp + 1).cast_signed(), &nest)
    };
    let lhs_aff = eval_load_aff(lhs_load)?;
    let rhs_aff = eval_load_aff(rhs_load)?;

    let lhs_str = affine_to_elem_strides(lhs_aff, elem.input_bytes())?;
    let rhs_str = affine_to_elem_strides(rhs_aff, elem.input_bytes())?;
    let c_str = affine_to_elem_strides(c_aff, acc_bytes)?;

    // Match either `(lhs = A, rhs = B)` or `(lhs = B, rhs = A)` (multiplication is commutative).
    let (a_aff, b_aff, i_ax, k_ax, j_ax, lda, ldb, ldc) =
        if let Some((i_ax, k_ax, j_ax, lda, ldb, ldc)) = match_gemm_roles(lhs_str, rhs_str, c_str) {
            (lhs_aff, rhs_aff, i_ax, k_ax, j_ax, lda, ldb, ldc)
        } else if let Some((i_ax, k_ax, j_ax, lda, ldb, ldc)) = match_gemm_roles(rhs_str, lhs_str, c_str) {
            (rhs_aff, lhs_aff, i_ax, k_ax, j_ax, lda, ldb, ldc)
        } else {
            return None;
        };

    // Statically skip trivially self-aliasing `C == A` or `C == B` kernels.
    if c_aff.base == a_aff.base || c_aff.base == b_aff.base {
        return None;
    }

    // Verify all external bases and dynamic stride values dominate `l0.preheader`.
    for &ptr_v in &[a_aff.base, b_aff.base, c_aff.base] {
        let pb = at(&cfg.inst_block, to_idx(ptr_v));
        if at(&l0.in_loop, to_idx(pb)) || !cfg.dominates(pb, l0.preheader) {
            return None;
        }
    }
    for &dv in &[lda, ldb, ldc] {
        if let DimVal::Value(v) = dv {
            let vb = at(&cfg.inst_block, to_idx(v));
            if at(&l0.in_loop, to_idx(vb)) || !cfg.dominates(vb, l0.preheader) {
                return None;
            }
        }
    }

    let loop_at = |ax: usize| -> &CountedLoop<A> {
        if ax == 0 {
            l0
        } else if ax == 1 {
            l1
        } else {
            l2
        }
    };
    let li = loop_at(i_ax);
    let lj = loop_at(j_ax);
    let lk = loop_at(k_ax);

    // If a preceding sibling zero-init loop `l_zero` was found inside `l0`, verify it zeroes the
    // exact same `C` row `C[i * ldc + 0..N]` and upgrade the entire nest to overwrite mode (`flags = 0`)!
    if let (Some(lz), Some(zero_store)) = (l_zero, zero_store_opt) {
        if i_ax != 0 {
            return None;
        }
        let zsi = to_idx(zero_store);
        let zsp = to_idx(at(view.data, zsi));
        let z_val = at(view.extra, zsp);
        let z_ptr = at(view.extra, zsp + 1);
        let z_off = at(view.extra, zsp + 2).cast_signed();
        if !is_zero_elem_const(view, to_idx(z_val), acc_ty) {
            return None;
        }
        let zero_nest = NestRef {
            loops: [l0, lz, lz],
            in_outer: &l0.in_loop,
            inst_block: &cfg.inst_block,
        };
        let z_aff = eval_affine_ptr(view, z_ptr, z_off, &zero_nest)?;
        if z_aff.base != c_aff.base || z_aff.off.const_off != c_aff.off.const_off {
            return None;
        }
        let [zs0, zs1, zs2] = affine_to_elem_strides(z_aff, acc_bytes)?;
        let [cs0, _, _] = c_str;
        if !same_dim_stride(view, zs0, cs0)
            || zs1 != DimStride::Const(1)
            || zs2 != DimStride::Zero
            || !same_val_or_const(view, lz.bound, lj.bound)
        {
            return None;
        }
        is_accumulate_into_c = false;
    }

    Some(GemmKernel {
        elem,
        a_ptr: a_aff.base,
        a_const_off: a_aff.off.const_off,
        b_ptr: b_aff.base,
        b_const_off: b_aff.off.const_off,
        c_ptr: c_aff.base,
        c_const_off: c_aff.off.const_off,
        m_dim: li.bound,
        m_ty: li.iv_ty,
        m_signed: li.is_signed,
        n_dim: lj.bound,
        n_ty: lj.iv_ty,
        n_signed: lj.is_signed,
        k_dim: lk.bound,
        k_ty: lk.iv_ty,
        k_signed: lk.is_signed,
        lda,
        ldb,
        ldc,
        is_accumulate_into_c,
        fma_flags,
        use_fused_fma,
        outer_preheader: l0.preheader,
        outer_header: l0.header,
        outer_exit: l0.exit,
        outer_pre_args: copy_u32_vec(&l0.pre_args, alloc),
        outer_exit_args: copy_u32_vec(&l0.exit_args, alloc),
        outer_blocks: copy_bool_vec(&l0.in_loop, alloc),
    })
}

// ---------------------------------------------------------------------------
// Lowering: Path A (Auto-SME2 Intrinsic) + Path B (4x16 NEON Micro-Kernel)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct LowerPlan {
    emit_sme2: bool,
    emit_neon: bool,
    check_ca: bool,
    check_cb: bool,
    dispatch_blk: u32,
}

#[inline]
const fn planned_blocks(emit_sme2: bool, emit_neon: bool, needs_alias: bool) -> u32 {
    let alias_b = if needs_alias { 1 } else { 0 };
    if emit_sme2 && emit_neon {
        // dim_guard + [alias_guard] + sme2_check + sme2_call + neon_guard + 16 NEON blocks
        1 + alias_b + 2 + 1 + 16
    } else if emit_sme2 {
        // dim_guard + [alias_guard] + sme2_call
        1 + alias_b + 1
    } else if emit_neon {
        // dim_guard + [alias_guard] + 16 NEON blocks
        1 + alias_b + 16
    } else {
        0
    }
}

fn emit_i64_from_val<A: Allocator>(func: &mut Func<A>, val_remapped: u32, ty: Ty) -> u32 {
    if ty == Ty::I64 {
        val_remapped
    } else {
        push_inst(func, Op::Zext, Flags::NONE, Ty::I64, val_remapped, 0)
    }
}

fn emit_i64_from_dim<A: Allocator>(func: &mut Func<A>, view: &View<'_>, dim: DimVal, val_map: &[u32]) -> u32 {
    match dim {
        DimVal::Const(c) => push_iconst(func, Ty::I64, c),
        DimVal::Value(v) => {
            let ty = Ty(at(view.tys, to_idx(v)));
            emit_i64_from_val(func, at(val_map, to_idx(v)), ty)
        }
    }
}

fn emit_ptr_with_const_off<A: Allocator>(func: &mut Func<A>, base_remapped: u32, const_off: i64) -> u32 {
    if const_off == 0 {
        base_remapped
    } else {
        let off_v = push_iconst(func, Ty::I64, const_off.cast_unsigned());
        push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, base_remapped, off_v)
    }
}

/// Emits one scalar or vector multiply-accumulate `a * b + acc`, preserving strict IEEE 754
/// separate `Fmul` + `Fadd` rounding when `use_fused_fma` is false.
fn emit_mac<A: Allocator>(
    func: &mut Func<A>,
    ty: Ty,
    fma_flags: Flags,
    use_fused_fma: bool,
    a: u32,
    b: u32,
    acc: u32,
) -> u32 {
    if ty.is_float() {
        if use_fused_fma {
            push_tern(func, Op::Fma, fma_flags, ty, a, b, acc)
        } else {
            let prod = push_bin(func, Op::Fmul, fma_flags, ty, a, b);
            push_bin(func, Op::Fadd, fma_flags, ty, acc, prod)
        }
    } else {
        let prod = push_bin(func, Op::Mul, Flags::NONE, ty, a, b);
        push_bin(func, Op::Add, Flags::NONE, ty, acc, prod)
    }
}

/// Emits the runtime guard block(s), optional Path A (hardware SME2 intrinsic), and optional
/// Path B (universal 4x16 NEON register-blocked micro-kernel) for a single `GemmKernel`.
fn emit_gemm_lowered<A: Allocator + Copy>(
    view: &View<'_>,
    kernel: &GemmKernel<A>,
    opts: GemmOpts,
    plan: LowerPlan,
    val_map: &[u32],
    func: &mut Func<A>,
    alloc: A,
) -> Result<(), SimplifyError> {
    let elem_ty = kernel.elem.acc_ty();
    let elem_bytes = kernel.elem.acc_bytes();
    let elem_shift = kernel.elem.acc_shift();
    let (lanes, vec_ty): (u64, Ty) = if elem_ty == Ty::F32 {
        (4, Ty::vector(crate::Scalar::F32, 4).unwrap_or(Ty::F32))
    } else if elem_ty == Ty::F64 {
        (2, Ty::vector(crate::Scalar::F64, 2).unwrap_or(Ty::F64))
    } else {
        (4, Ty::vector(crate::Scalar::I32, 4).unwrap_or(Ty::I32))
    };
    let n_step = 4 * lanes; // 16 for f32/i32, 8 for f64 (always 64 bytes = 4 x 128-bit vectors)
    let min_sme2 = opts.min_sme2_dim.max(1);
    let needs_alias = plan.check_ca || plan.check_cb;

    let mut pre_args_new: Vec<u32, A> = Vec::with_capacity_in(kernel.outer_pre_args.len(), alloc);
    for &a in &kernel.outer_pre_args {
        pre_args_new.push(at(val_map, to_idx(a)));
    }
    let mut exit_args_new: Vec<u32, A> = Vec::with_capacity_in(kernel.outer_exit_args.len(), alloc);
    for &a in &kernel.outer_exit_args {
        exit_args_new.push(at(val_map, to_idx(a)));
    }

    // Reserve exact sequential block IDs starting at `plan.dispatch_blk`.
    let mut next_blk = plan.dispatch_blk;
    let _dim_guard_blk = next_blk;
    next_blk += 1;

    let alias_guard_blk = if needs_alias {
        let b = next_blk;
        next_blk += 1;
        b
    } else {
        NONE
    };

    let (sme2_check_blk, sme2_call_blk) = if plan.emit_sme2 && plan.emit_neon {
        let chk = next_blk;
        let call = next_blk + 1;
        next_blk += 2;
        (chk, call)
    } else if plan.emit_sme2 {
        let call = next_blk;
        next_blk += 1;
        (NONE, call)
    } else {
        (NONE, NONE)
    };

    let neon_guard_blk = if plan.emit_sme2 && plan.emit_neon {
        let g = next_blk;
        next_blk += 1;
        g
    } else {
        NONE
    };

    let (
        neon_pre_blk,
        i_main_head_blk,
        j_main_head_blk,
        k_main_loop_blk,
        j_main_store_blk,
        i_main_jtail_chk_blk,
        j_tail_head_blk,
        k_jtail_loop_blk,
        j_tail_store_blk,
        i_main_latch_blk,
        i_tail_chk_blk,
        i_tail_head_blk,
        j_itail_head_blk,
        k_itail_loop_blk,
        j_itail_store_blk,
        i_tail_latch_blk,
    ) = if plan.emit_neon {
        let base = next_blk;
        next_blk += 16;
        (
            base,
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9,
            base + 10,
            base + 11,
            base + 12,
            base + 13,
            base + 14,
            base + 15,
        )
    } else {
        (
            NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE,
        )
    };

    let after_alias_target = if plan.emit_sme2 && plan.emit_neon {
        sme2_check_blk
    } else if plan.emit_sme2 {
        sme2_call_blk
    } else {
        neon_pre_blk
    };
    let after_dim_target = if needs_alias { alias_guard_blk } else { after_alias_target };

    // --- Block: `dim_guard_blk` (cheap minimum-dimension + signed + overflow guard) ---
    let _ = begin_block(func, BlockKind::Normal);

    let a_base = emit_ptr_with_const_off(func, at(val_map, to_idx(kernel.a_ptr)), kernel.a_const_off);
    let b_base = emit_ptr_with_const_off(func, at(val_map, to_idx(kernel.b_ptr)), kernel.b_const_off);
    let c_base = emit_ptr_with_const_off(func, at(val_map, to_idx(kernel.c_ptr)), kernel.c_const_off);

    let m_raw = at(val_map, to_idx(kernel.m_dim));
    let n_raw = at(val_map, to_idx(kernel.n_dim));
    let k_raw = at(val_map, to_idx(kernel.k_dim));
    let m64 = emit_i64_from_val(func, m_raw, kernel.m_ty);
    let n64 = emit_i64_from_val(func, n_raw, kernel.n_ty);
    let k64 = emit_i64_from_val(func, k_raw, kernel.k_ty);
    let lda64 = emit_i64_from_dim(func, view, kernel.lda, val_map);
    let ldb64 = emit_i64_from_dim(func, view, kernel.ldb, val_map);
    let ldc64 = emit_i64_from_dim(func, view, kernel.ldc, val_map);

    let (min_fast_m, min_fast_n, min_fast_k) = if plan.emit_sme2 && plan.emit_neon {
        (4u64.min(min_sme2), n_step.min(min_sme2), 1u64)
    } else if plan.emit_sme2 {
        (min_sme2, min_sme2, 4u64)
    } else {
        (4u64, n_step, 1u64)
    };

    // 1. Minimum fast-path dimension check + overflow cap + signed positivity.
    let k_mfm = push_iconst(func, Ty::I64, min_fast_m);
    let k_mfn = push_iconst(func, Ty::I64, min_fast_n);
    let k_mfk = push_iconst(func, Ty::I64, min_fast_k);
    let ok_m0 = push_icmp(func, IntCond::Uge, m64, k_mfm);
    let ok_n0 = push_icmp(func, IntCond::Uge, n64, k_mfn);
    let ok_k0 = push_icmp(func, IntCond::Uge, k64, k_mfk);
    let ok_mn0 = push_bin(func, Op::And, Flags::NONE, Ty::I1, ok_m0, ok_n0);
    let mut safe = push_bin(func, Op::And, Flags::NONE, Ty::I1, ok_mn0, ok_k0);

    let or1 = push_bin(func, Op::Or, Flags::NONE, Ty::I64, m64, n64);
    let or2 = push_bin(func, Op::Or, Flags::NONE, Ty::I64, or1, k64);
    let or3 = push_bin(func, Op::Or, Flags::NONE, Ty::I64, or2, lda64);
    let or4 = push_bin(func, Op::Or, Flags::NONE, Ty::I64, or3, ldb64);
    let or_all = push_bin(func, Op::Or, Flags::NONE, Ty::I64, or4, ldc64);
    let k_max_safe = push_iconst(func, Ty::I64, MAX_SAFE_DIM);
    let ok_cap = push_icmp(func, IntCond::Ule, or_all, k_max_safe);
    safe = push_bin(func, Op::And, Flags::NONE, Ty::I1, safe, ok_cap);

    if kernel.m_signed {
        let z = push_iconst(func, kernel.m_ty, 0);
        let ok = push_icmp(func, IntCond::Sgt, m_raw, z);
        safe = push_bin(func, Op::And, Flags::NONE, Ty::I1, safe, ok);
    }
    if kernel.n_signed {
        let z = push_iconst(func, kernel.n_ty, 0);
        let ok = push_icmp(func, IntCond::Sgt, n_raw, z);
        safe = push_bin(func, Op::And, Flags::NONE, Ty::I1, safe, ok);
    }
    if kernel.k_signed {
        let z = push_iconst(func, kernel.k_ty, 0);
        let ok = push_icmp(func, IntCond::Sgt, k_raw, z);
        safe = push_bin(func, Op::And, Flags::NONE, Ty::I1, safe, ok);
    }

    push_br(func, safe, after_dim_target, &[], kernel.outer_header, &pre_args_new);

    // --- Optional Block: `alias_guard_blk` (wraparound-safe `C` vs `A` and `C` vs `B` checks) ---
    if needs_alias {
        let _ = begin_block(func, BlockKind::Normal);
        let c_shift = kernel.elem.acc_shift();
        let in_shift = kernel.elem.input_shift();
        let c_sh_v = push_iconst(func, Ty::I64, u64::from(c_shift));
        let in_sh_v = if in_shift == 0 {
            NONE
        } else if in_shift == c_shift {
            c_sh_v
        } else {
            push_iconst(func, Ty::I64, u64::from(in_shift))
        };
        let c_elems = push_bin(func, Op::Mul, Flags::NONE, Ty::I64, m64, ldc64);
        let bytes_c = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, c_elems, c_sh_v);
        let c_addr = push_inst(func, Op::PtrAddr, Flags::NONE, Ty::I64, c_base, 0);

        let mut alias_ok = NONE;
        if plan.check_ca {
            let a_elems = push_bin(func, Op::Mul, Flags::NONE, Ty::I64, m64, lda64);
            let bytes_a = if in_shift == 0 {
                a_elems
            } else {
                push_bin(func, Op::Shl, Flags::NONE, Ty::I64, a_elems, in_sh_v)
            };
            let a_addr = push_inst(func, Op::PtrAddr, Flags::NONE, Ty::I64, a_base, 0);
            let d_ca = push_bin(func, Op::Sub, Flags::NONE, Ty::I64, c_addr, a_addr);
            let d_ac = push_inst(func, Op::Neg, Flags::NONE, Ty::I64, d_ca, 0);
            let dist_ca = push_bin(func, Op::Umin, Flags::NONE, Ty::I64, d_ca, d_ac);
            let c_lt_a = push_icmp(func, IntCond::Ult, c_addr, a_addr);
            let need_ca = push_tern(func, Op::Select, Flags::NONE, Ty::I64, c_lt_a, bytes_c, bytes_a);
            alias_ok = push_icmp(func, IntCond::Uge, dist_ca, need_ca);
        }
        if plan.check_cb {
            let b_elems = push_bin(func, Op::Mul, Flags::NONE, Ty::I64, k64, ldb64);
            let bytes_b = if in_shift == 0 {
                b_elems
            } else {
                push_bin(func, Op::Shl, Flags::NONE, Ty::I64, b_elems, in_sh_v)
            };
            let b_addr = push_inst(func, Op::PtrAddr, Flags::NONE, Ty::I64, b_base, 0);
            let d_cb = push_bin(func, Op::Sub, Flags::NONE, Ty::I64, c_addr, b_addr);
            let d_bc = push_inst(func, Op::Neg, Flags::NONE, Ty::I64, d_cb, 0);
            let dist_cb = push_bin(func, Op::Umin, Flags::NONE, Ty::I64, d_cb, d_bc);
            let c_lt_b = push_icmp(func, IntCond::Ult, c_addr, b_addr);
            let need_cb = push_tern(func, Op::Select, Flags::NONE, Ty::I64, c_lt_b, bytes_c, bytes_b);
            let ok_cb = push_icmp(func, IntCond::Uge, dist_cb, need_cb);
            alias_ok = if alias_ok == NONE {
                ok_cb
            } else {
                push_bin(func, Op::And, Flags::NONE, Ty::I1, alias_ok, ok_cb)
            };
        }

        push_br(func, alias_ok, after_alias_target, &[], kernel.outer_header, &pre_args_new);
    }

    // --- Path A: Hardware Auto-SME2 Intrinsic (`[sme2_check_blk] + sme2_call_blk`) ---
    if plan.emit_sme2 {
        if plan.emit_neon {
            let _ = begin_block(func, BlockKind::Normal);
            let k_min_mn = push_iconst(func, Ty::I64, min_sme2);
            let k_min_k = push_iconst(func, Ty::I64, 4);
            let ok_m = push_icmp(func, IntCond::Uge, m64, k_min_mn);
            let ok_n = push_icmp(func, IntCond::Uge, n64, k_min_mn);
            let ok_k = push_icmp(func, IntCond::Uge, k64, k_min_k);
            let ok_mn = push_bin(func, Op::And, Flags::NONE, Ty::I1, ok_m, ok_n);
            let ok_sme2 = push_bin(func, Op::And, Flags::NONE, Ty::I1, ok_mn, ok_k);
            push_br(func, ok_sme2, sme2_call_blk, &[], neon_guard_blk, &[]);
        }

        let _ = begin_block(func, BlockKind::Normal);
        let intr_name = kernel.elem.sme2_intrinsic().unwrap_or(SME2_GEMM_F32_INTRINSIC);
        let intr_id = ensure_sme2_intrinsic(func, intr_name);
        let flags_val = push_iconst(func, Ty::I64, u64::from(kernel.is_accumulate_into_c));
        let _ = push_target_intrinsic(func, intr_id, &[c_base, a_base, b_base, m64, n64, k64, flags_val]);
        push_jump(func, kernel.outer_exit, &exit_args_new);
    }

    // --- Path B: Universal 4x16 NEON Register-Blocked Micro-Kernel ---
    if plan.emit_neon {
        // 0. Optional `neon_guard_blk` (when SME2 path precedes NEON): verify `m >= 4 && n >= n_step && k >= 1`.
        if plan.emit_sme2 {
            let _ = begin_block(func, BlockKind::Normal);
            let k_4_g = push_iconst(func, Ty::I64, 4);
            let k_nstep_g = push_iconst(func, Ty::I64, n_step);
            let k_1_g = push_iconst(func, Ty::I64, 1);
            let ok_m = push_icmp(func, IntCond::Uge, m64, k_4_g);
            let ok_n = push_icmp(func, IntCond::Uge, n64, k_nstep_g);
            let ok_k = push_icmp(func, IntCond::Uge, k64, k_1_g);
            let ok_mn = push_bin(func, Op::And, Flags::NONE, Ty::I1, ok_m, ok_n);
            let ok_neon = push_bin(func, Op::And, Flags::NONE, Ty::I1, ok_mn, ok_k);
            push_br(func, ok_neon, neon_pre_blk, &[], kernel.outer_header, &pre_args_new);
        }

        // 1. `neon_pre_blk`: precompute all byte strides, tile bounds, and zero constants once.
        let _ = begin_block(func, BlockKind::Normal);
        let sh_e = push_iconst(func, Ty::I64, u64::from(elem_shift));
        let sh_1 = push_iconst(func, Ty::I64, 1);
        let sh_2 = push_iconst(func, Ty::I64, 2);
        let lda_bytes = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, lda64, sh_e);
        let ldb_bytes = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, ldb64, sh_e);
        let ldc_bytes = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, ldc64, sh_e);
        let lda_bytes_2 = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, lda_bytes, sh_1);
        let lda_bytes_3 = push_bin(func, Op::Add, Flags::NONE, Ty::I64, lda_bytes, lda_bytes_2);
        let lda_bytes_4 = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, lda_bytes, sh_2);
        let ldc_bytes_2 = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, ldc_bytes, sh_1);
        let ldc_bytes_3 = push_bin(func, Op::Add, Flags::NONE, Ty::I64, ldc_bytes, ldc_bytes_2);
        let ldc_bytes_4 = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, ldc_bytes, sh_2);
        let k_bytes = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, k64, sh_e);

        let k_mask_m = push_iconst(func, Ty::I64, !3u64);
        let k_mask_n = push_iconst(func, Ty::I64, !(n_step - 1));
        let m_main = push_bin(func, Op::And, Flags::NONE, Ty::I64, m64, k_mask_m);
        let n_main = push_bin(func, Op::And, Flags::NONE, Ty::I64, n64, k_mask_n);
        let n_main_bytes = push_bin(func, Op::Shl, Flags::NONE, Ty::I64, n_main, sh_e);
        let b_j_end = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, b_base, n_main_bytes);

        let has_j_tail = push_icmp(func, IntCond::Ult, n_main, n64);
        let has_i_tail = push_icmp(func, IntCond::Ult, m_main, m64);

        let k_elem_bytes = push_iconst(func, Ty::I64, elem_bytes);
        let k_64 = push_iconst(func, Ty::I64, 64);
        let k_4 = push_iconst(func, Ty::I64, 4);
        let k_1 = push_iconst(func, Ty::I64, 1);
        let k_0 = push_iconst(func, Ty::I64, 0);

        let (s_zero, v_zero) = if !kernel.is_accumulate_into_c {
            let sz = push_zero_scalar(func, elem_ty);
            let vz = push_inst(func, Op::Splat, Flags::NONE, vec_ty, sz, 0);
            (sz, vz)
        } else {
            (NONE, NONE)
        };
        push_jump(func, i_main_head_blk, &[k_0, a_base, c_base]);

        // 2. `i_main_head_blk(i, a_i0, c_i0)`: 4 rows `[i, i + 4)`.
        let _ = begin_block(func, BlockKind::Normal);
        let i_iv = push_param(func, Ty::I64);
        let a_i0 = push_param(func, Ty::PTR);
        let c_i0 = push_param(func, Ty::PTR);
        let a_i1 = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, a_i0, lda_bytes);
        let a_i2 = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, a_i0, lda_bytes_2);
        let a_i3 = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, a_i0, lda_bytes_3);
        let pa0_end = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, a_i0, k_bytes);
        let c_i1 = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_i0, ldc_bytes);
        let c_i2 = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_i0, ldc_bytes_2);
        let c_i3 = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_i0, ldc_bytes_3);
        push_jump(func, j_main_head_blk, &[b_base, c_i0, c_i1, c_i2, c_i3]);

        // 3. `j_main_head_blk(b_j, c_j0, c_j1, c_j2, c_j3)`: load or zero-init 16 vector accumulators.
        let _ = begin_block(func, BlockKind::Normal);
        let b_j = push_param(func, Ty::PTR);
        let c_j0 = push_param(func, Ty::PTR);
        let c_j1 = push_param(func, Ty::PTR);
        let c_j2 = push_param(func, Ty::PTR);
        let c_j3 = push_param(func, Ty::PTR);
        let c_j_ptrs = [c_j0, c_j1, c_j2, c_j3];

        let mut k_init_args: Vec<u32, A> = Vec::with_capacity_in(5 + 16, alloc);
        k_init_args.push(a_i0);
        k_init_args.push(a_i1);
        k_init_args.push(a_i2);
        k_init_args.push(a_i3);
        k_init_args.push(b_j);
        for &c_ptr_r in &c_j_ptrs {
            for u in 0..4i32 {
                let acc0 = if kernel.is_accumulate_into_c {
                    push_load(func, vec_ty, c_ptr_r, u * 16)
                } else {
                    v_zero
                };
                k_init_args.push(acc0);
            }
        }
        push_jump(func, k_main_loop_blk, &k_init_args);

        // 4. `k_main_loop_blk`: THE 4x16 REGISTER-BLOCKED HOT INNER LOOP!
        //    21 params: `pa0..pa3, pb` + 16 vector accumulators `c[r][u]`.
        let _ = begin_block(func, BlockKind::Normal);
        let pa0 = push_param(func, Ty::PTR);
        let pa1 = push_param(func, Ty::PTR);
        let pa2 = push_param(func, Ty::PTR);
        let pa3 = push_param(func, Ty::PTR);
        let pb = push_param(func, Ty::PTR);
        let pa_ptrs = [pa0, pa1, pa2, pa3];

        let mut c_acc = [NONE; 16];
        for idx in 0..16 {
            set_at(&mut c_acc, idx, push_param(func, vec_ty));
        }

        // Load 4 contiguous 128-bit vectors of B (`+0, +16, +32, +48` -> TWO `ldp q, q`!).
        let b0 = push_load(func, vec_ty, pb, 0);
        let b1 = push_load(func, vec_ty, pb, 16);
        let b2 = push_load(func, vec_ty, pb, 32);
        let b3 = push_load(func, vec_ty, pb, 48);
        let b_vecs = [b0, b1, b2, b3];

        // For each of the 4 A rows, load 1 scalar, splat once, and execute 4 vector FMAs!
        let mut c_next = [NONE; 16];
        for (r, &pa_r) in pa_ptrs.iter().enumerate() {
            let a_scalar = push_load(func, elem_ty, pa_r, 0);
            let va_r = push_inst(func, Op::Splat, Flags::NONE, vec_ty, a_scalar, 0);
            for (u, &bu) in b_vecs.iter().enumerate() {
                let slot = r * 4 + u;
                let prev = at(&c_acc, slot);
                let updated = emit_mac(func, vec_ty, kernel.fma_flags, kernel.use_fused_fma, va_r, bu, prev);
                set_at(&mut c_next, slot, updated);
            }
        }

        // Advance running pointers with ZERO `Shl` / `Mul`!
        let pa0_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, pa0, k_elem_bytes);
        let pa1_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, pa1, k_elem_bytes);
        let pa2_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, pa2, k_elem_bytes);
        let pa3_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, pa3, k_elem_bytes);
        let pb_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, pb, ldb_bytes);

        let mut k_back_args: Vec<u32, A> = Vec::with_capacity_in(5 + 16, alloc);
        k_back_args.push(pa0_next);
        k_back_args.push(pa1_next);
        k_back_args.push(pa2_next);
        k_back_args.push(pa3_next);
        k_back_args.push(pb_next);
        for &cn in &c_next {
            k_back_args.push(cn);
        }
        let keep_k = push_icmp(func, IntCond::Ult, pa0_next, pa0_end);
        push_br(func, keep_k, k_main_loop_blk, &k_back_args, j_main_store_blk, &c_next);

        // 5. `j_main_store_blk`: store all 16 vector accumulators back to `C` and advance `j += 64B`.
        let _ = begin_block(func, BlockKind::Normal);
        let mut c_fin = [NONE; 16];
        for idx in 0..16 {
            set_at(&mut c_fin, idx, push_param(func, vec_ty));
        }
        for (r, &c_ptr_r) in c_j_ptrs.iter().enumerate() {
            for u in 0..4usize {
                let off_i32 = i32::try_from(u * 16).unwrap_or(0);
                let _ = push_store(func, at(&c_fin, r * 4 + u), c_ptr_r, off_i32);
            }
        }
        let b_j_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, b_j, k_64);
        let c_j0_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_j0, k_64);
        let c_j1_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_j1, k_64);
        let c_j2_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_j2, k_64);
        let c_j3_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_j3, k_64);
        let next_j_args = [b_j_next, c_j0_next, c_j1_next, c_j2_next, c_j3_next];
        let keep_j = push_icmp(func, IntCond::Ult, b_j_next, b_j_end);
        push_br(func, keep_j, j_main_head_blk, &next_j_args, i_main_jtail_chk_blk, &next_j_args);

        // 6. `i_main_jtail_chk_blk`: branch to 4-row scalar `j`-tail if `n_main < N`.
        let _ = begin_block(func, BlockKind::Normal);
        let b_jt_start = push_param(func, Ty::PTR);
        let c_jt0_start = push_param(func, Ty::PTR);
        let c_jt1_start = push_param(func, Ty::PTR);
        let c_jt2_start = push_param(func, Ty::PTR);
        let c_jt3_start = push_param(func, Ty::PTR);
        push_br(
            func,
            has_j_tail,
            j_tail_head_blk,
            &[n_main, b_jt_start, c_jt0_start, c_jt1_start, c_jt2_start, c_jt3_start],
            i_main_latch_blk,
            &[],
        );

        // 7. `j_tail_head_blk`: 4 rows x 1 column scalar `j`-tail.
        let _ = begin_block(func, BlockKind::Normal);
        let j_t = push_param(func, Ty::I64);
        let b_jt = push_param(func, Ty::PTR);
        let c_jt0 = push_param(func, Ty::PTR);
        let c_jt1 = push_param(func, Ty::PTR);
        let c_jt2 = push_param(func, Ty::PTR);
        let c_jt3 = push_param(func, Ty::PTR);
        let (s0_0, s1_0, s2_0, s3_0) = if kernel.is_accumulate_into_c {
            (
                push_load(func, elem_ty, c_jt0, 0),
                push_load(func, elem_ty, c_jt1, 0),
                push_load(func, elem_ty, c_jt2, 0),
                push_load(func, elem_ty, c_jt3, 0),
            )
        } else {
            (s_zero, s_zero, s_zero, s_zero)
        };
        push_jump(func, k_jtail_loop_blk, &[a_i0, a_i1, a_i2, a_i3, b_jt, s0_0, s1_0, s2_0, s3_0]);

        // 8. `k_jtail_loop_blk`: scalar k-loop for 4 rows x 1 tail column.
        let _ = begin_block(func, BlockKind::Normal);
        let t_pa0 = push_param(func, Ty::PTR);
        let t_pa1 = push_param(func, Ty::PTR);
        let t_pa2 = push_param(func, Ty::PTR);
        let t_pa3 = push_param(func, Ty::PTR);
        let t_pb = push_param(func, Ty::PTR);
        let t_s0 = push_param(func, elem_ty);
        let t_s1 = push_param(func, elem_ty);
        let t_s2 = push_param(func, elem_ty);
        let t_s3 = push_param(func, elem_ty);

        let tb_v = push_load(func, elem_ty, t_pb, 0);
        let ta0_v = push_load(func, elem_ty, t_pa0, 0);
        let t_s0_n = emit_mac(func, elem_ty, kernel.fma_flags, kernel.use_fused_fma, ta0_v, tb_v, t_s0);
        let ta1_v = push_load(func, elem_ty, t_pa1, 0);
        let t_s1_n = emit_mac(func, elem_ty, kernel.fma_flags, kernel.use_fused_fma, ta1_v, tb_v, t_s1);
        let ta2_v = push_load(func, elem_ty, t_pa2, 0);
        let t_s2_n = emit_mac(func, elem_ty, kernel.fma_flags, kernel.use_fused_fma, ta2_v, tb_v, t_s2);
        let ta3_v = push_load(func, elem_ty, t_pa3, 0);
        let t_s3_n = emit_mac(func, elem_ty, kernel.fma_flags, kernel.use_fused_fma, ta3_v, tb_v, t_s3);

        let t_pa0_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, t_pa0, k_elem_bytes);
        let t_pa1_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, t_pa1, k_elem_bytes);
        let t_pa2_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, t_pa2, k_elem_bytes);
        let t_pa3_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, t_pa3, k_elem_bytes);
        let t_pb_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, t_pb, ldb_bytes);
        let keep_kjt = push_icmp(func, IntCond::Ult, t_pa0_n, pa0_end);
        push_br(
            func,
            keep_kjt,
            k_jtail_loop_blk,
            &[t_pa0_n, t_pa1_n, t_pa2_n, t_pa3_n, t_pb_n, t_s0_n, t_s1_n, t_s2_n, t_s3_n],
            j_tail_store_blk,
            &[t_s0_n, t_s1_n, t_s2_n, t_s3_n],
        );

        // 9. `j_tail_store_blk`: store 4 tail scalars and advance `j += 1`.
        let _ = begin_block(func, BlockKind::Normal);
        let f_s0 = push_param(func, elem_ty);
        let f_s1 = push_param(func, elem_ty);
        let f_s2 = push_param(func, elem_ty);
        let f_s3 = push_param(func, elem_ty);
        let _ = push_store(func, f_s0, c_jt0, 0);
        let _ = push_store(func, f_s1, c_jt1, 0);
        let _ = push_store(func, f_s2, c_jt2, 0);
        let _ = push_store(func, f_s3, c_jt3, 0);
        let j_t_n = push_bin(func, Op::Add, Flags::NONE, Ty::I64, j_t, k_1);
        let b_jt_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, b_jt, k_elem_bytes);
        let c_jt0_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_jt0, k_elem_bytes);
        let c_jt1_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_jt1, k_elem_bytes);
        let c_jt2_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_jt2, k_elem_bytes);
        let c_jt3_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_jt3, k_elem_bytes);
        let keep_jt = push_icmp(func, IntCond::Ult, j_t_n, n64);
        push_br(
            func,
            keep_jt,
            j_tail_head_blk,
            &[j_t_n, b_jt_n, c_jt0_n, c_jt1_n, c_jt2_n, c_jt3_n],
            i_main_latch_blk,
            &[],
        );

        // 10. `i_main_latch_blk`: advance `i += 4`.
        let _ = begin_block(func, BlockKind::Normal);
        let i_next = push_bin(func, Op::Add, Flags::NONE, Ty::I64, i_iv, k_4);
        let a_i0_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, a_i0, lda_bytes_4);
        let c_i0_next = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_i0, ldc_bytes_4);
        let keep_i = push_icmp(func, IntCond::Ult, i_next, m_main);
        push_br(
            func,
            keep_i,
            i_main_head_blk,
            &[i_next, a_i0_next, c_i0_next],
            i_tail_chk_blk,
            &[i_next, a_i0_next, c_i0_next],
        );

        // 11. `i_tail_chk_blk`: branch to remaining `m % 4` scalar rows or exit.
        let _ = begin_block(func, BlockKind::Normal);
        let i_rem_start = push_param(func, Ty::I64);
        let a_rem_start = push_param(func, Ty::PTR);
        let c_rem_start = push_param(func, Ty::PTR);
        push_br(
            func,
            has_i_tail,
            i_tail_head_blk,
            &[i_rem_start, a_rem_start, c_rem_start],
            kernel.outer_exit,
            &exit_args_new,
        );

        // 12. `i_tail_head_blk(ir, a_ir, c_ir)`: 1 row of `i in m_main..M`.
        let _ = begin_block(func, BlockKind::Normal);
        let ir = push_param(func, Ty::I64);
        let a_ir = push_param(func, Ty::PTR);
        let c_ir = push_param(func, Ty::PTR);
        let pa_ir_end = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, a_ir, k_bytes);
        push_jump(func, j_itail_head_blk, &[k_0, b_base, c_ir]);

        // 13. `j_itail_head_blk(jr, b_jr, c_jr)`: 1 column of `j in 0..N` for row `ir`.
        let _ = begin_block(func, BlockKind::Normal);
        let jr = push_param(func, Ty::I64);
        let b_jr = push_param(func, Ty::PTR);
        let c_jr = push_param(func, Ty::PTR);
        let sr_0 = if kernel.is_accumulate_into_c {
            push_load(func, elem_ty, c_jr, 0)
        } else {
            s_zero
        };
        push_jump(func, k_itail_loop_blk, &[a_ir, b_jr, sr_0]);

        // 14. `k_itail_loop_blk(pa, pb, sr)`: scalar k-loop for row `ir`, col `jr`.
        let _ = begin_block(func, BlockKind::Normal);
        let r_pa = push_param(func, Ty::PTR);
        let r_pb = push_param(func, Ty::PTR);
        let r_s = push_param(func, elem_ty);
        let ra_v = push_load(func, elem_ty, r_pa, 0);
        let rb_v = push_load(func, elem_ty, r_pb, 0);
        let r_s_n = emit_mac(func, elem_ty, kernel.fma_flags, kernel.use_fused_fma, ra_v, rb_v, r_s);
        let r_pa_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, r_pa, k_elem_bytes);
        let r_pb_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, r_pb, ldb_bytes);
        let keep_rk = push_icmp(func, IntCond::Ult, r_pa_n, pa_ir_end);
        push_br(
            func,
            keep_rk,
            k_itail_loop_blk,
            &[r_pa_n, r_pb_n, r_s_n],
            j_itail_store_blk,
            &[r_s_n],
        );

        // 15. `j_itail_store_blk(sr_fin)`: store `C[ir, jr]` and advance `jr += 1`.
        let _ = begin_block(func, BlockKind::Normal);
        let sr_fin = push_param(func, elem_ty);
        let _ = push_store(func, sr_fin, c_jr, 0);
        let jr_n = push_bin(func, Op::Add, Flags::NONE, Ty::I64, jr, k_1);
        let b_jr_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, b_jr, k_elem_bytes);
        let c_jr_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_jr, k_elem_bytes);
        let keep_rj = push_icmp(func, IntCond::Ult, jr_n, n64);
        push_br(func, keep_rj, j_itail_head_blk, &[jr_n, b_jr_n, c_jr_n], i_tail_latch_blk, &[]);

        // 16. `i_tail_latch_blk`: advance `ir += 1` or exit.
        let _ = begin_block(func, BlockKind::Normal);
        let ir_n = push_bin(func, Op::Add, Flags::NONE, Ty::I64, ir, k_1);
        let a_ir_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, a_ir, lda_bytes);
        let c_ir_n = push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, c_ir, ldc_bytes);
        let keep_ri = push_icmp(func, IntCond::Ult, ir_n, m64);
        push_br(
            func,
            keep_ri,
            i_tail_head_blk,
            &[ir_n, a_ir_n, c_ir_n],
            kernel.outer_exit,
            &exit_args_new,
        );
    }

    if to_u32(func.block_starts.len()) != next_blk {
        return Err(SimplifyError::Refused {
            inst: to_idx(kernel.outer_header),
            what: "GEMM lowering block count mismatch",
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public Entrypoint
// ---------------------------------------------------------------------------

/// Detects triple-nested GEMM loops (`i-k-j` and `i-j-k`, `f32`/`f64`/`i32`) and lowers them to
/// a guarded hardware SME2 intrinsic (`"blaze.sme2.gemm_f32"` / `"blaze.sme2.gemm_f64"`) and/or
/// a universal 4x16 NEON register-blocked micro-kernel (`16` 128-bit vector accumulators).
pub fn optimize_gemm_loops<A: Allocator + Copy>(
    view: View<'_>,
    opts: GemmOpts,
    alloc: A,
) -> Result<GemmOutput<A>, SimplifyError> {
    if view.head.ptr_bits != 64 || view.blocks() < 4 || (!opts.enable_sme2 && !opts.enable_neon_4x16) {
        return Ok(GemmOutput {
            func: view.to_func_in(alloc),
            stats: GemmStats::default(),
        });
    }

    let cfg = CfgInfo::build(&view, alloc);
    let (counted, all_natural_loops) = discover_counted_loops(&view, &cfg, alloc);
    if counted.len() < 3 {
        return Ok(GemmOutput {
            func: view.to_func_in(alloc),
            stats: GemmStats::default(),
        });
    }

    // Collect up to `MAX_GEMMS_PER_FUNC` disjoint triple-nested GEMM kernels in deterministic order.
    let blocks = view.blocks();
    let mut claimed_blocks = filled(blocks, false, &alloc);
    let mut claimed_preheaders = filled(blocks, false, &alloc);
    let mut kernels: Vec<GemmKernel<A>, A> = Vec::new_in(alloc);

    for outer_idx in 0..counted.len() {
        if kernels.len() >= MAX_GEMMS_PER_FUNC {
            break;
        }
        let Some(k) = detect_gemm_nest(&view, &cfg, &counted, &all_natural_loops, outer_idx, alloc) else {
            continue;
        };
        if at(&claimed_preheaders, to_idx(k.outer_preheader)) {
            continue;
        }
        let mut overlaps = false;
        for b in 0..blocks {
            if at(&k.outer_blocks, b) && at(&claimed_blocks, b) {
                overlaps = true;
                break;
            }
        }
        if overlaps {
            continue;
        }
        set_at(&mut claimed_preheaders, to_idx(k.outer_preheader), true);
        for b in 0..blocks {
            if at(&k.outer_blocks, b) {
                set_at(&mut claimed_blocks, b, true);
            }
        }
        kernels.push(k);
    }

    if kernels.is_empty() {
        return Ok(GemmOutput {
            func: view.to_func_in(alloc),
            stats: GemmStats::default(),
        });
    }

    // Plan lowering paths and compute exact appended block indices for every selected GEMM.
    let mut plans: Vec<LowerPlan, A> = Vec::with_capacity_in(kernels.len(), alloc);
    let mut stats = GemmStats::default();
    let mut next_blk = to_u32(blocks);
    let mut preheader_redirect = filled(blocks, NONE, &alloc);
    let mut preheader_old_header = filled(blocks, NONE, &alloc);

    for k in &kernels {
        let compact_strides = k.lda.matches_bound(&view, k.k_dim)
            && k.ldb.matches_bound(&view, k.n_dim)
            && k.ldc.matches_bound(&view, k.n_dim);
        let sme2_rounding_ok = !k.elem.acc_ty().is_float() || k.use_fused_fma;
        let emit_sme2 =
            opts.enable_sme2 && k.elem.sme2_intrinsic().is_some() && compact_strides && sme2_rounding_ok;
        let emit_neon = opts.enable_neon_4x16 && k.elem.supports_neon_4x16();
        if !emit_sme2 && !emit_neon {
            plans.push(LowerPlan {
                emit_sme2: false,
                emit_neon: false,
                check_ca: false,
                check_cb: false,
                dispatch_blk: NONE,
            });
            continue;
        }
        let check_ca = !provably_noalias_bases(&view, k.c_ptr, k.a_ptr);
        let check_cb = !provably_noalias_bases(&view, k.c_ptr, k.b_ptr);
        let dispatch_blk = next_blk;
        next_blk += planned_blocks(emit_sme2, emit_neon, check_ca || check_cb);
        if emit_sme2 {
            stats.sme2_lowered += 1;
        }
        if emit_neon {
            stats.neon_blocked += 1;
        }
        stats.gemms_detected += 1;

        set_at(&mut preheader_redirect, to_idx(k.outer_preheader), dispatch_blk);
        set_at(&mut preheader_old_header, to_idx(k.outer_preheader), k.outer_header);
        plans.push(LowerPlan {
            emit_sme2,
            emit_neon,
            check_ca,
            check_cb,
            dispatch_blk,
        });
    }

    if stats.gemms_detected == 0 {
        return Ok(GemmOutput {
            func: view.to_func_in(alloc),
            stats,
        });
    }

    // Rebuild the function: copy original blocks `0..blocks` (redirecting each GEMM preheader
    // terminator to its `dispatch_blk`), then append all generated dispatch/SME2/NEON blocks.
    let mut func = view.to_func_in(alloc);
    clear_code_columns(&mut func);

    let mut val_map = filled(view.len(), NONE, &alloc);
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);

    for b in 0..blocks {
        let b32 = to_u32(b);
        let _ = begin_block(&mut func, view.block_kind(Block(b32)));
        let range = view.block(Block(b32));
        let term = range.end - 1;
        for inst in range.start..term {
            let nv = copy_remapped(&view, inst, &val_map, &mut func, &mut fixups);
            set_at(&mut val_map, inst, nv);
        }
        let redir = at(&preheader_redirect, b);
        if redir != NONE {
            let old_h = at(&preheader_old_header, b);
            copy_redirected_preheader_term(&view, term, old_h, redir, &val_map, &mut func, &mut fixups);
            set_at(&mut val_map, term, to_u32(func.ops.len().saturating_sub(1)));
        } else {
            let nv = copy_remapped(&view, term, &val_map, &mut func, &mut fixups);
            set_at(&mut val_map, term, nv);
        }
    }

    // Resolve any forward value references inside the copied original blocks.
    for fx in &fixups {
        match *fx {
            ValFixup::Data { inst, old_val } => {
                set_at(&mut func.data, inst, at(&val_map, to_idx(old_val)));
            }
            ValFixup::Extra { pos, old_val } => {
                set_at(&mut func.extra, pos, at(&val_map, to_idx(old_val)));
            }
        }
    }

    // Append lowered dispatch, SME2, and 4x16 NEON blocks for each selected GEMM kernel.
    for (k, &plan) in kernels.iter().zip(plans.iter()) {
        if plan.dispatch_blk != NONE {
            emit_gemm_lowered(&view, k, opts, plan, &val_map, &mut func, alloc)?;
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(GemmOutput { func, stats })
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::{Builder, Facts, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::SlotKind,
    };

    /// Builds `fn matmul_ikj(c: *mut T, a: *const T, b: *const T, n: u64)` computing
    /// `C[i*n + j] += A[i*n + k] * B[k*n + j]` in `i-k-j` order (matching `matmul_f32`).
    fn build_ikj_gemm(elem_ty: Ty, use_fma: bool, add_flags: Flags, c_noalias: bool) -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let mut c_param = Param::of(Ty::PTR);
        if c_noalias {
            c_param.facts = Facts {
                bits: ParamFactsRec::NOALIAS,
                ..Facts::default()
            };
        }
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[c_param, Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[],
        );
        b.set_signature(sig);

        let [entry, i_hdr, i_body, k_hdr, k_body, j_hdr, j_body, k_latch, i_latch, exit] =
            [(); 10].map(|_| b.create_block(BlockKind::Normal));
        let shift: u128 = if elem_ty == Ty::F64 { 3 } else { 2 };
        let align: u8 = if elem_ty == Ty::F64 { 3 } else { 2 };

        b.begin(entry);
        let pc = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let sh = b.iconst(Ty::I64, shift);
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        b.jump((k_hdr, &[zero]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let keep_k = b.icmp(IntCond::Ult, k, n);
        b.br(Flags::NONE, keep_k, (k_body, &[]), (i_latch, &[]));

        b.begin(k_body);
        let a_idx = b.binary(Op::Add, Flags::NONE, i_n, k);
        let a_off = b.binary(Op::Shl, Flags::NONE, a_idx, sh);
        let a_ptr = b.ptr_add(Flags::NONE, pa, a_off);
        let a_val = b.load(elem_ty, Flags::NONE, a_ptr, 0, MemAttrs::aligned(align));
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        b.jump((j_hdr, &[zero]));

        b.begin(j_hdr);
        let j = b.param(Ty::I64);
        let keep_j = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, keep_j, (j_body, &[]), (k_latch, &[]));

        b.begin(j_body);
        let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
        let b_off = b.binary(Op::Shl, Flags::NONE, b_idx, sh);
        let b_ptr = b.ptr_add(Flags::NONE, pb, b_off);
        let b_val = b.load(elem_ty, Flags::NONE, b_ptr, 0, MemAttrs::aligned(align));

        let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
        let c_off = b.binary(Op::Shl, Flags::NONE, c_idx, sh);
        let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
        let c_val = b.load(elem_ty, Flags::NONE, c_ptr, 0, MemAttrs::aligned(align));

        let c_new = if elem_ty.is_float() {
            if use_fma {
                b.ternary(Op::Fma, add_flags, a_val, b_val, c_val)
            } else {
                let prod = b.binary(Op::Fmul, add_flags, a_val, b_val);
                b.binary(Op::Fadd, add_flags, c_val, prod)
            }
        } else {
            let prod = b.binary(Op::Mul, Flags::NONE, a_val, b_val);
            b.binary(Op::Add, Flags::NONE, c_val, prod)
        };
        b.store(Flags::NONE, c_new, c_ptr, 0, MemAttrs::aligned(align));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((j_hdr, &[j2]));

        b.begin(k_latch);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((k_hdr, &[k2]));

        b.begin(i_latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((i_hdr, &[i2]));

        b.begin(exit);
        b.ret(&[]);
        b.finish_checked().unwrap()
    }

    /// Builds rectangular `i-j-k` dot-product GEMM `C[i*N + j] = sum_k A[i*K + k] * B[k*N + j]`
    /// (zero-initialized `acc` -> `is_accumulate_into_c == false`).
    fn build_ijk_dot_gemm_f32() -> Func<TestHeap> {
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
                Param::of(Ty::I64),
            ],
            &[],
        );
        b.set_signature(sig);

        let [entry, i_hdr, i_body, j_hdr, j_body, k_hdr, k_body, j_latch, i_latch, exit] =
            [(); 10].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let pc = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let m = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let k_dim = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        let fzero = b.fconst(Ty::F32, 0);
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Ult, i, m);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_k = b.binary(Op::Mul, Flags::NONE, i, k_dim);
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        b.jump((j_hdr, &[zero]));

        b.begin(j_hdr);
        let j = b.param(Ty::I64);
        let keep_j = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, keep_j, (j_body, &[]), (i_latch, &[]));

        b.begin(j_body);
        b.jump((k_hdr, &[zero, fzero]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let acc = b.param(Ty::F32);
        let keep_k = b.icmp(IntCond::Ult, k, k_dim);
        b.br(Flags::NONE, keep_k, (k_body, &[]), (j_latch, &[acc]));

        b.begin(k_body);
        let a_idx = b.binary(Op::Add, Flags::NONE, i_k, k);
        let a_off = b.binary(Op::Shl, Flags::NONE, a_idx, two);
        let a_ptr = b.ptr_add(Flags::NONE, pa, a_off);
        let a_val = b.load(Ty::F32, Flags::NONE, a_ptr, 0, MemAttrs::aligned(2));
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
        let b_off = b.binary(Op::Shl, Flags::NONE, b_idx, two);
        let b_ptr = b.ptr_add(Flags::NONE, pb, b_off);
        let b_val = b.load(Ty::F32, Flags::NONE, b_ptr, 0, MemAttrs::aligned(2));
        let prod = b.binary(Op::Fmul, Flags::REASSOC, a_val, b_val);
        let acc2 = b.binary(Op::Fadd, Flags::REASSOC, acc, prod);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((k_hdr, &[k2, acc2]));

        b.begin(j_latch);
        let final_acc = b.param(Ty::F32);
        let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
        let c_off = b.binary(Op::Shl, Flags::NONE, c_idx, two);
        let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
        b.store(Flags::NONE, final_acc, c_ptr, 0, MemAttrs::aligned(2));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((j_hdr, &[j2]));

        b.begin(i_latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((i_hdr, &[i2]));

        b.begin(exit);
        b.ret(&[]);
        b.finish_checked().unwrap()
    }

    /// Finds the maximum number of vector `Op::Fma` instructions in any single block.
    fn max_vec_fmas_in_block(view: View<'_>, vec_ty: Ty) -> usize {
        (0..view.blocks())
            .map(|b| {
                view.block(Block(b as u32))
                    .filter(|&i| view.op(i) == Op::Fma && Ty(view.tys[i]) == vec_ty)
                    .count()
            })
            .max()
            .unwrap_or(0)
    }

    /// Finds an `Op::TargetIntrinsic` call matching `name` and returns its argument values.
    fn find_intrinsic_call<'a>(view: View<'a>, name: &str) -> Option<&'a [u32]> {
        for inst in 0..view.len() {
            if view.op(inst) == Op::TargetIntrinsic {
                let pos = view.data[inst] as usize;
                let id = IntrinsicId(view.extra[pos]);
                if view.intrinsic_name(id) == name.as_bytes() {
                    let n = view.extra[pos + 1] as usize;
                    return Some(&view.extra[pos + 2..pos + 2 + n]);
                }
            }
        }
        None
    }

    #[test]
    fn ikj_f32_gemm_lowers_to_sme2_and_neon_4x16() {
        let func = build_ikj_gemm(Ty::F32, true, Flags::NONE, false);

        // Path A: SME2 dispatch (with NEON fallback).
        let sme2_out = optimize_gemm_loops(func.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(sme2_out.func.view()).unwrap();
        assert_eq!(sme2_out.stats.gemms_detected, 1);
        assert_eq!(sme2_out.stats.sme2_lowered, 1);
        let args = find_intrinsic_call(sme2_out.func.view(), SME2_GEMM_F32_INTRINSIC)
            .expect("SME2 f32 intrinsic call emitted");
        assert_eq!(args.len(), 7);
        // `flags` arg (index 6) must be `1` (accumulate into C).
        assert_eq!(const_u64(&sme2_out.func.view(), args[6] as usize), Some(1));

        // Path B: Universal 4x16 NEON register-blocked micro-kernel.
        let neon_out = optimize_gemm_loops(func.view(), GemmOpts::neon(), TestHeap).unwrap();
        verify(neon_out.func.view()).unwrap();
        assert_eq!(neon_out.stats.gemms_detected, 1);
        assert_eq!(neon_out.stats.sme2_lowered, 0);
        assert_eq!(neon_out.stats.neon_blocked, 1);
        assert!(find_intrinsic_call(neon_out.func.view(), SME2_GEMM_F32_INTRINSIC).is_none());

        let v4f32 = Ty::vector(crate::Scalar::F32, 4).unwrap();
        let v = neon_out.func.view();
        assert!(
            max_vec_fmas_in_block(v, v4f32) >= 16,
            "expected >= 16 `<4 x f32>` FMAs in the register-blocked k-loop"
        );
        // Verify the hot k-loop block carries 5 running pointers + 16 accumulators = 21 params
        // and uses ZERO `Op::Shl` / `Op::Mul` instructions.
        let hot_blk = (0..v.blocks())
            .find(|&b| {
                v.block(Block(b as u32))
                    .filter(|&i| v.op(i) == Op::Fma && Ty(v.tys[i]) == v4f32)
                    .count()
                    >= 16
            })
            .expect("hot 4x16 k-loop block");
        assert_eq!(v.params(Block(hot_blk as u32)).len(), 21);
        assert_eq!(v.block(Block(hot_blk as u32)).filter(|&i| v.op(i) == Op::Shl).count(), 0);
        assert_eq!(v.block(Block(hot_blk as u32)).filter(|&i| v.op(i) == Op::Mul).count(), 0);
        assert_eq!(v.block(Block(hot_blk as u32)).filter(|&i| v.op(i) == Op::Splat).count(), 4);
    }

    #[test]
    fn ijk_f32_dot_gemm_overwrite_lowers_to_sme2_and_neon_4x16() {
        let func = build_ijk_dot_gemm_f32();

        let sme2_out = optimize_gemm_loops(func.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(sme2_out.func.view()).unwrap();
        assert_eq!(sme2_out.stats.gemms_detected, 1);
        assert_eq!(sme2_out.stats.sme2_lowered, 1);
        let args = find_intrinsic_call(sme2_out.func.view(), SME2_GEMM_F32_INTRINSIC)
            .expect("SME2 f32 intrinsic call emitted for i-j-k");
        assert_eq!(args.len(), 7);
        // `flags` arg (index 6) must be `0` (overwrite C = A*B).
        assert_eq!(const_u64(&sme2_out.func.view(), args[6] as usize), Some(0));

        let neon_out = optimize_gemm_loops(func.view(), GemmOpts::neon(), TestHeap).unwrap();
        verify(neon_out.func.view()).unwrap();
        assert_eq!(neon_out.stats.gemms_detected, 1);
        assert_eq!(neon_out.stats.sme2_lowered, 0);
        assert_eq!(neon_out.stats.neon_blocked, 1);
        let v4f32 = Ty::vector(crate::Scalar::F32, 4).unwrap();
        assert!(max_vec_fmas_in_block(neon_out.func.view(), v4f32) >= 16);
    }

    #[test]
    fn f64_and_i32_gemm_lowering_paths() {
        // f64 GEMM -> SME2 f64 intrinsic + 4x8 (`<2 x f64>`) NEON micro-kernel.
        let f64_fn = build_ikj_gemm(Ty::F64, true, Flags::NONE, false);
        let sme2_f64 = optimize_gemm_loops(f64_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(sme2_f64.func.view()).unwrap();
        assert_eq!(sme2_f64.stats.sme2_lowered, 1);
        assert!(find_intrinsic_call(sme2_f64.func.view(), SME2_GEMM_F64_INTRINSIC).is_some());

        let neon_f64 = optimize_gemm_loops(f64_fn.view(), GemmOpts::neon(), TestHeap).unwrap();
        verify(neon_f64.func.view()).unwrap();
        let v2f64 = Ty::vector(crate::Scalar::F64, 2).unwrap();
        assert!(max_vec_fmas_in_block(neon_f64.func.view(), v2f64) >= 16);

        // i32 GEMM -> even under `GemmOpts::sme2()`, skips SME2 and emits 4x16 `<4 x i32>` NEON!
        let i32_fn = build_ikj_gemm(Ty::I32, false, Flags::NONE, false);
        let i32_out = optimize_gemm_loops(i32_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(i32_out.func.view()).unwrap();
        assert_eq!(i32_out.stats.gemms_detected, 1);
        assert_eq!(i32_out.stats.sme2_lowered, 0);
        assert_eq!(i32_out.stats.neon_blocked, 1);
        let v4i32 = Ty::vector(crate::Scalar::I32, 4).unwrap();
        let v = i32_out.func.view();
        let vec_muls = (0..v.blocks())
            .map(|b| {
                v.block(Block(b as u32))
                    .filter(|&i| v.op(i) == Op::Mul && Ty(v.tys[i]) == v4i32)
                    .count()
            })
            .max()
            .unwrap_or(0);
        assert!(vec_muls >= 16, "expected >= 16 `<4 x i32>` multiplies in the k-loop");
    }

    #[test]
    fn rotated_single_block_and_hoisted_row_ptrs_detected() {
        // Post-`sota_optimize_fixpoint` / `rotate_loops` shape: hoisted `a_row`/`c_row`/`b_row`
        // and rotated `SingleBlock` inner `j` loop with `Slt` bounds.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[],
        );
        b.set_signature(sig);
        let [entry, i_hdr, i_body, k_hdr, k_body, j_loop, k_latch, i_latch, exit] =
            [(); 9].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let pc = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Slt, i, n);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        let i_bytes = b.binary(Op::Shl, Flags::NONE, i_n, two);
        let a_row = b.ptr_add(Flags::NONE, pa, i_bytes);
        let c_row = b.ptr_add(Flags::NONE, pc, i_bytes);
        b.jump((k_hdr, &[zero]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let keep_k = b.icmp(IntCond::Slt, k, n);
        b.br(Flags::NONE, keep_k, (k_body, &[]), (i_latch, &[]));

        b.begin(k_body);
        let k_sh = b.binary(Op::Shl, Flags::NONE, k, two);
        let a_p = b.ptr_add(Flags::NONE, a_row, k_sh);
        let a_val = b.load(Ty::F32, Flags::NONE, a_p, 0, MemAttrs::aligned(2));
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        let kn_sh = b.binary(Op::Shl, Flags::NONE, k_n, two);
        let b_row = b.ptr_add(Flags::NONE, pb, kn_sh);
        let enter_j = b.icmp(IntCond::Slt, zero, n);
        b.br(Flags::NONE, enter_j, (j_loop, &[zero]), (k_latch, &[]));

        b.begin(j_loop);
        let j = b.param(Ty::I64);
        let j_bytes = b.binary(Op::Shl, Flags::NONE, j, two);
        let b_p = b.ptr_add(Flags::NONE, b_row, j_bytes);
        let b_val = b.load(Ty::F32, Flags::NONE, b_p, 0, MemAttrs::aligned(2));
        let c_p = b.ptr_add(Flags::NONE, c_row, j_bytes);
        let c_val = b.load(Ty::F32, Flags::NONE, c_p, 0, MemAttrs::aligned(2));
        let c_new = b.ternary(Op::Fmuladd, Flags::NONE, a_val, b_val, c_val);
        b.store(Flags::NONE, c_new, c_p, 0, MemAttrs::aligned(2));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        let keep_j = b.icmp(IntCond::Slt, j2, n);
        b.br(Flags::NONE, keep_j, (j_loop, &[j2]), (k_latch, &[]));

        b.begin(k_latch);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((k_hdr, &[k2]));

        b.begin(i_latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((i_hdr, &[i2]));

        b.begin(exit);
        b.ret(&[]);
        let func = b.finish_checked().unwrap();

        let sme2_out = optimize_gemm_loops(func.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(sme2_out.func.view()).unwrap();
        assert_eq!(sme2_out.stats.gemms_detected, 1);
        assert_eq!(sme2_out.stats.sme2_lowered, 1);
        assert!(find_intrinsic_call(sme2_out.func.view(), SME2_GEMM_F32_INTRINSIC).is_some());
    }

    #[test]
    fn non_gemm_strict_ieee_and_noalias_behavior() {
        // 1. Strict IEEE `fmul + fadd` WITHOUT `Flags::REASSOC`/`CONTRACT` must NEVER contract into
        //    SME2 `FMOPA` or `Op::Fma`, but lowers to bit-exact 4x16 NEON using separate `Fmul`+`Fadd`.
        let strict_fn = build_ikj_gemm(Ty::F32, false, Flags::NONE, false);
        let strict_out = optimize_gemm_loops(strict_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(strict_out.func.view()).unwrap();
        assert_eq!(strict_out.stats.gemms_detected, 1);
        assert_eq!(strict_out.stats.sme2_lowered, 0);
        assert_eq!(strict_out.stats.neon_blocked, 1);
        let sv = strict_out.func.view();
        assert_eq!((0..sv.len()).filter(|&i| matches!(sv.op(i), Op::Fma | Op::Fmuladd)).count(), 0);
        assert!(find_intrinsic_call(sv, SME2_GEMM_F32_INTRINSIC).is_none());

        // 2. `C` with `ParamFactsRec::NOALIAS` skips runtime `Op::Umin` alias checks.
        let noalias_fn = build_ikj_gemm(Ty::F32, true, Flags::NONE, true);
        let noalias_out = optimize_gemm_loops(noalias_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(noalias_out.func.view()).unwrap();
        assert_eq!(noalias_out.stats.gemms_detected, 1);
        let v = noalias_out.func.view();
        assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Umin).count(), 0);

        // 3. Aliased & tiny runtime execution on an `i32` GEMM falls back cleanly and stays bit-exact.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I32)]);
        b.set_signature(sig);
        let slot_a = b.slot(16 * 4, 4, SlotKind::Temp);
        let slot_b = b.slot(16 * 4, 4, SlotKind::Temp);
        let slot_c = b.slot(16 * 4, 4, SlotKind::Temp);
        let [entry, i_hdr, i_body, k_hdr, k_body, j_hdr, j_body, k_latch, i_latch, exit] =
            [(); 10].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let alias_mode = b.param(Ty::I64);
        let pa = b.slot_addr(slot_a);
        let pb = b.slot_addr(slot_b);
        let pc_raw = b.slot_addr(slot_c);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        for idx in 0..16u32 {
            let va = b.iconst(Ty::I32, u128::from(idx + 1));
            let vb = b.iconst(Ty::I32, u128::from(idx * 2 + 3));
            let vc = b.iconst(Ty::I32, 0);
            b.store(Flags::NONE, va, pa, (idx * 4) as i32, MemAttrs::aligned(2));
            b.store(Flags::NONE, vb, pb, (idx * 4) as i32, MemAttrs::aligned(2));
            b.store(Flags::NONE, vc, pc_raw, (idx * 4) as i32, MemAttrs::aligned(2));
        }
        let is_alias = b.icmp(IntCond::Ne, alias_mode, zero);
        let pc = b.select(Flags::NONE, is_alias, pa, pc_raw);
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        b.jump((k_hdr, &[zero]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let keep_k = b.icmp(IntCond::Ult, k, n);
        b.br(Flags::NONE, keep_k, (k_body, &[]), (i_latch, &[]));

        b.begin(k_body);
        let a_idx = b.binary(Op::Add, Flags::NONE, i_n, k);
        let a_sh = b.binary(Op::Shl, Flags::NONE, a_idx, two);
        let a_p = b.ptr_add(Flags::NONE, pa, a_sh);
        let a_val = b.load(Ty::I32, Flags::NONE, a_p, 0, MemAttrs::aligned(2));
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        b.jump((j_hdr, &[zero]));

        b.begin(j_hdr);
        let j = b.param(Ty::I64);
        let keep_j = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, keep_j, (j_body, &[]), (k_latch, &[]));

        b.begin(j_body);
        let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
        let b_sh = b.binary(Op::Shl, Flags::NONE, b_idx, two);
        let b_p = b.ptr_add(Flags::NONE, pb, b_sh);
        let b_val = b.load(Ty::I32, Flags::NONE, b_p, 0, MemAttrs::aligned(2));
        let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
        let c_sh = b.binary(Op::Shl, Flags::NONE, c_idx, two);
        let c_p = b.ptr_add(Flags::NONE, pc, c_sh);
        let c_val = b.load(Ty::I32, Flags::NONE, c_p, 0, MemAttrs::aligned(2));
        let prod = b.binary(Op::Mul, Flags::NONE, a_val, b_val);
        let c_new = b.binary(Op::Add, Flags::NONE, c_val, prod);
        b.store(Flags::NONE, c_new, c_p, 0, MemAttrs::aligned(2));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((j_hdr, &[j2]));

        b.begin(k_latch);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((k_hdr, &[k2]));

        b.begin(i_latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((i_hdr, &[i2]));

        b.begin(exit);
        let out_v = b.load(Ty::I32, Flags::NONE, pc, 0, MemAttrs::aligned(2));
        b.ret(&[out_v]);
        let aliased_fn = b.finish_checked().unwrap();

        let opt_out = optimize_gemm_loops(aliased_fn.view(), GemmOpts::neon(), TestHeap).unwrap();
        verify(opt_out.func.view()).unwrap();
        assert_eq!(opt_out.stats.gemms_detected, 1);
        let opt_v = opt_out.func.view();
        assert!(
            (0..opt_v.len()).filter(|&i| opt_v.op(i) == Op::Umin).count() >= 2,
            "runtime alias guard must check both C vs A and C vs B"
        );

        // Evaluate both disjoint (`alias_mode = 0`) and aliased (`alias_mode = 1`, `C == A`) on n=0..=4!
        for &(nv, al) in &[(0u128, 0u128), (1, 0), (3, 0), (4, 0), (3, 1), (4, 1)] {
            let args = [Val::Int(nv), Val::Int(al)];
            let Outcome::Returned(orig) = eval::run_in(aliased_fn.view(), &args, 100_000, TestHeap).unwrap() else {
                panic!("orig eval failed");
            };
            let Outcome::Returned(got) = eval::run_in(opt_out.func.view(), &args, 100_000, TestHeap).unwrap() else {
                panic!("opt eval failed");
            };
            assert_eq!(orig, got, "mismatch at n={nv}, alias={al}");
        }

        // 4. Non-GEMM triple loop (`C[i*n+j] += A[i*n+k] + B[k*n+j]`, add instead of mul) stays untouched.
        let mut nb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let nsig = nb.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[],
        );
        nb.set_signature(nsig);
        let [e0, ih, ib, kh, kb, jh, jb, kl, il, ex] = [(); 10].map(|_| nb.create_block(BlockKind::Normal));
        nb.begin(e0);
        let (pc, pa, pb, n) = (nb.param(Ty::PTR), nb.param(Ty::PTR), nb.param(Ty::PTR), nb.param(Ty::I64));
        let (z, o, tw) = (nb.iconst(Ty::I64, 0), nb.iconst(Ty::I64, 1), nb.iconst(Ty::I64, 2));
        nb.jump((ih, &[z]));
        nb.begin(ih);
        let i = nb.param(Ty::I64);
        let ki = nb.icmp(IntCond::Ult, i, n);
        nb.br(Flags::NONE, ki, (ib, &[]), (ex, &[]));
        nb.begin(ib);
        let i_n = nb.binary(Op::Mul, Flags::NONE, i, n);
        nb.jump((kh, &[z]));
        nb.begin(kh);
        let k = nb.param(Ty::I64);
        let kk = nb.icmp(IntCond::Ult, k, n);
        nb.br(Flags::NONE, kk, (kb, &[]), (il, &[]));
        nb.begin(kb);
        let a_idx = nb.binary(Op::Add, Flags::NONE, i_n, k);
        let a_sh = nb.binary(Op::Shl, Flags::NONE, a_idx, tw);
        let a_p = nb.ptr_add(Flags::NONE, pa, a_sh);
        let av = nb.load(Ty::F32, Flags::NONE, a_p, 0, MemAttrs::aligned(2));
        let k_n = nb.binary(Op::Mul, Flags::NONE, k, n);
        nb.jump((jh, &[z]));
        nb.begin(jh);
        let j = nb.param(Ty::I64);
        let kj = nb.icmp(IntCond::Ult, j, n);
        nb.br(Flags::NONE, kj, (jb, &[]), (kl, &[]));
        nb.begin(jb);
        let b_idx = nb.binary(Op::Add, Flags::NONE, k_n, j);
        let b_sh = nb.binary(Op::Shl, Flags::NONE, b_idx, tw);
        let b_p = nb.ptr_add(Flags::NONE, pb, b_sh);
        let bv = nb.load(Ty::F32, Flags::NONE, b_p, 0, MemAttrs::aligned(2));
        let c_idx = nb.binary(Op::Add, Flags::NONE, i_n, j);
        let c_sh = nb.binary(Op::Shl, Flags::NONE, c_idx, tw);
        let cp = nb.ptr_add(Flags::NONE, pc, c_sh);
        let cv = nb.load(Ty::F32, Flags::NONE, cp, 0, MemAttrs::aligned(2));
        let sum = nb.binary(Op::Fadd, Flags::REASSOC, av, bv);
        let cn = nb.binary(Op::Fadd, Flags::REASSOC, cv, sum);
        nb.store(Flags::NONE, cn, cp, 0, MemAttrs::aligned(2));
        let j2 = nb.binary(Op::Add, Flags::NONE, j, o);
        nb.jump((jh, &[j2]));
        nb.begin(kl);
        let k2 = nb.binary(Op::Add, Flags::NONE, k, o);
        nb.jump((kh, &[k2]));
        nb.begin(il);
        let i2 = nb.binary(Op::Add, Flags::NONE, i, o);
        nb.jump((ih, &[i2]));
        nb.begin(ex);
        nb.ret(&[]);
        let non_gemm_fn = nb.finish_checked().unwrap();

        let ng_out = optimize_gemm_loops(non_gemm_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(ng_out.func.view()).unwrap();
        assert_eq!(ng_out.stats.gemms_detected, 0);
    }

    /// Builds `i-k-j` widening `i8/u8 -> i32` GEMM `C[i*n + j] += ext(A[i*n + k]) * ext(B[k*n + j])`
    /// with 1-byte strides on `A` and `B` and 4-byte stride on `C`.
    fn build_ikj_byte_gemm(ext_a: Op, ext_b: Op, c_noalias: bool) -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let mut c_param = Param::of(Ty::PTR);
        if c_noalias {
            c_param.facts = Facts {
                bits: ParamFactsRec::NOALIAS,
                ..Facts::default()
            };
        }
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[c_param, Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[],
        );
        b.set_signature(sig);

        let [entry, i_hdr, i_body, k_hdr, k_body, j_hdr, j_body, k_latch, i_latch, exit] =
            [(); 10].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let pc = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        b.jump((k_hdr, &[zero]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let keep_k = b.icmp(IntCond::Ult, k, n);
        b.br(Flags::NONE, keep_k, (k_body, &[]), (i_latch, &[]));

        b.begin(k_body);
        let a_idx = b.binary(Op::Add, Flags::NONE, i_n, k);
        let a_ptr = b.ptr_add(Flags::NONE, pa, a_idx);
        let a_raw = b.load(Ty::I8, Flags::NONE, a_ptr, 0, MemAttrs::aligned(0));
        let a_val = b.convert(ext_a, Flags::NONE, Ty::I32, a_raw);
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        b.jump((j_hdr, &[zero]));

        b.begin(j_hdr);
        let j = b.param(Ty::I64);
        let keep_j = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, keep_j, (j_body, &[]), (k_latch, &[]));

        b.begin(j_body);
        let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
        let b_ptr = b.ptr_add(Flags::NONE, pb, b_idx);
        let b_raw = b.load(Ty::I8, Flags::NONE, b_ptr, 0, MemAttrs::aligned(0));
        let b_val = b.convert(ext_b, Flags::NONE, Ty::I32, b_raw);

        let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
        let c_off = b.binary(Op::Shl, Flags::NONE, c_idx, two);
        let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
        let c_val = b.load(Ty::I32, Flags::NONE, c_ptr, 0, MemAttrs::aligned(2));
        let prod = b.binary(Op::Mul, Flags::NONE, a_val, b_val);
        let c_new = b.binary(Op::Add, Flags::NONE, c_val, prod);
        b.store(Flags::NONE, c_new, c_ptr, 0, MemAttrs::aligned(2));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((j_hdr, &[j2]));

        b.begin(k_latch);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((k_hdr, &[k2]));

        b.begin(i_latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((i_hdr, &[i2]));

        b.begin(exit);
        b.ret(&[]);
        b.finish_checked().unwrap()
    }

    /// Builds rectangular `i-j-k` dot-product widening `i8/u8 -> i32` GEMM
    /// `C[i*N + j] = sum_k ext(A[i*K + k]) * ext(B[k*N + j])` (overwrite mode, `flags = 0`).
    fn build_ijk_dot_byte_gemm(ext_op: Op) -> Func<TestHeap> {
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
                Param::of(Ty::I64),
            ],
            &[],
        );
        b.set_signature(sig);

        let [entry, i_hdr, i_body, j_hdr, j_body, k_hdr, k_body, j_latch, i_latch, exit] =
            [(); 10].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let pc = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let m = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let k_dim = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        let izero = b.iconst(Ty::I32, 0);
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Ult, i, m);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_k = b.binary(Op::Mul, Flags::NONE, i, k_dim);
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        b.jump((j_hdr, &[zero]));

        b.begin(j_hdr);
        let j = b.param(Ty::I64);
        let keep_j = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, keep_j, (j_body, &[]), (i_latch, &[]));

        b.begin(j_body);
        b.jump((k_hdr, &[zero, izero]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let acc = b.param(Ty::I32);
        let keep_k = b.icmp(IntCond::Ult, k, k_dim);
        b.br(Flags::NONE, keep_k, (k_body, &[]), (j_latch, &[acc]));

        b.begin(k_body);
        let a_idx = b.binary(Op::Add, Flags::NONE, i_k, k);
        let a_ptr = b.ptr_add(Flags::NONE, pa, a_idx);
        let a_raw = b.load(Ty::I8, Flags::NONE, a_ptr, 0, MemAttrs::aligned(0));
        let a_val = b.convert(ext_op, Flags::NONE, Ty::I32, a_raw);
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
        let b_ptr = b.ptr_add(Flags::NONE, pb, b_idx);
        let b_raw = b.load(Ty::I8, Flags::NONE, b_ptr, 0, MemAttrs::aligned(0));
        let b_val = b.convert(ext_op, Flags::NONE, Ty::I32, b_raw);
        let prod = b.binary(Op::Mul, Flags::NONE, a_val, b_val);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, prod);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((k_hdr, &[k2, acc2]));

        b.begin(j_latch);
        let final_acc = b.param(Ty::I32);
        let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
        let c_off = b.binary(Op::Shl, Flags::NONE, c_idx, two);
        let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
        b.store(Flags::NONE, final_acc, c_ptr, 0, MemAttrs::aligned(2));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((j_hdr, &[j2]));

        b.begin(i_latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((i_hdr, &[i2]));

        b.begin(exit);
        b.ret(&[]);
        b.finish_checked().unwrap()
    }

    /// Builds idiomatic safe-Rust overwrite matmul with a sibling row-zero-init loop inside `i`:
    /// `for i in 0..n { for j in 0..n { c[i*n+j] = 0; } for k in 0..n { ... for j in 0..n { c[i*n+j] += a*b; } } }`.
    /// When `zero_after_k` is true, places the zero-init loop AFTER the `k` loop (negative test).
    fn build_sibling_zero_init_ikj_gemm(is_i8: bool, zero_after_k: bool) -> Func<TestHeap> {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[],
        );
        b.set_signature(sig);

        let [
            entry,
            i_hdr,
            i_body,
            z_hdr,
            z_body,
            k_pre,
            k_hdr,
            k_body,
            j_hdr,
            j_body,
            k_latch,
            i_latch,
            exit,
        ] = [(); 13].map(|_| b.create_block(BlockKind::Normal));
        let acc_ty = if is_i8 { Ty::I32 } else { Ty::F32 };

        b.begin(entry);
        let pc = b.param(Ty::PTR);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        let zero_elem = if is_i8 { b.iconst(Ty::I32, 0) } else { b.fconst(Ty::F32, 0) };
        b.jump((i_hdr, &[zero]));

        b.begin(i_hdr);
        let i = b.param(Ty::I64);
        let keep_i = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

        b.begin(i_body);
        let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
        if zero_after_k {
            b.jump((k_pre, &[]));
        } else {
            b.jump((z_hdr, &[zero]));
        }

        b.begin(z_hdr);
        let jz = b.param(Ty::I64);
        let keep_jz = b.icmp(IntCond::Ult, jz, n);
        let z_exit = if zero_after_k { i_latch } else { k_pre };
        b.br(Flags::NONE, keep_jz, (z_body, &[]), (z_exit, &[]));

        b.begin(z_body);
        let z_idx = b.binary(Op::Add, Flags::NONE, i_n, jz);
        let z_off = b.binary(Op::Shl, Flags::NONE, z_idx, two);
        let z_ptr = b.ptr_add(Flags::NONE, pc, z_off);
        b.store(Flags::NONE, zero_elem, z_ptr, 0, MemAttrs::aligned(2));
        let jz2 = b.binary(Op::Add, Flags::NONE, jz, one);
        b.jump((z_hdr, &[jz2]));

        b.begin(k_pre);
        b.jump((k_hdr, &[zero]));

        b.begin(k_hdr);
        let k = b.param(Ty::I64);
        let keep_k = b.icmp(IntCond::Ult, k, n);
        let k_exit = if zero_after_k { z_hdr } else { i_latch };
        let k_exit_args: &[crate::Value] = if zero_after_k { &[zero] } else { &[] };
        b.br(Flags::NONE, keep_k, (k_body, &[]), (k_exit, k_exit_args));

        b.begin(k_body);
        let a_idx = b.binary(Op::Add, Flags::NONE, i_n, k);
        let a_val = if is_i8 {
            let a_ptr = b.ptr_add(Flags::NONE, pa, a_idx);
            let a_raw = b.load(Ty::I8, Flags::NONE, a_ptr, 0, MemAttrs::aligned(0));
            b.convert(Op::Sext, Flags::NONE, Ty::I32, a_raw)
        } else {
            let a_off = b.binary(Op::Shl, Flags::NONE, a_idx, two);
            let a_ptr = b.ptr_add(Flags::NONE, pa, a_off);
            b.load(Ty::F32, Flags::NONE, a_ptr, 0, MemAttrs::aligned(2))
        };
        let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
        b.jump((j_hdr, &[zero]));

        b.begin(j_hdr);
        let j = b.param(Ty::I64);
        let keep_j = b.icmp(IntCond::Ult, j, n);
        b.br(Flags::NONE, keep_j, (j_body, &[]), (k_latch, &[]));

        b.begin(j_body);
        let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
        let b_val = if is_i8 {
            let b_ptr = b.ptr_add(Flags::NONE, pb, b_idx);
            let b_raw = b.load(Ty::I8, Flags::NONE, b_ptr, 0, MemAttrs::aligned(0));
            b.convert(Op::Sext, Flags::NONE, Ty::I32, b_raw)
        } else {
            let b_off = b.binary(Op::Shl, Flags::NONE, b_idx, two);
            let b_ptr = b.ptr_add(Flags::NONE, pb, b_off);
            b.load(Ty::F32, Flags::NONE, b_ptr, 0, MemAttrs::aligned(2))
        };
        let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
        let c_off = b.binary(Op::Shl, Flags::NONE, c_idx, two);
        let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
        let c_val = b.load(acc_ty, Flags::NONE, c_ptr, 0, MemAttrs::aligned(2));
        let c_new = if is_i8 {
            let prod = b.binary(Op::Mul, Flags::NONE, a_val, b_val);
            b.binary(Op::Add, Flags::NONE, c_val, prod)
        } else {
            b.ternary(Op::Fma, Flags::NONE, a_val, b_val, c_val)
        };
        b.store(Flags::NONE, c_new, c_ptr, 0, MemAttrs::aligned(2));
        let j2 = b.binary(Op::Add, Flags::NONE, j, one);
        b.jump((j_hdr, &[j2]));

        b.begin(k_latch);
        let k2 = b.binary(Op::Add, Flags::NONE, k, one);
        b.jump((k_hdr, &[k2]));

        b.begin(i_latch);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((i_hdr, &[i2]));

        b.begin(exit);
        b.ret(&[]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn signed_i8_gemm_lowers_to_sme2_smopa() {
        // 1. i-k-j accumulate (`flags = 1`) with runtime alias check.
        let ikj_fn = build_ikj_byte_gemm(Op::Sext, Op::Sext, false);
        let out = optimize_gemm_loops(ikj_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.gemms_detected, 1);
        assert_eq!(out.stats.sme2_lowered, 1);
        assert_eq!(out.stats.neon_blocked, 0);
        let args = find_intrinsic_call(out.func.view(), SME2_GEMM_I8_I32_INTRINSIC)
            .expect("SME2 i8->i32 intrinsic emitted");
        assert_eq!(args.len(), 7);
        assert_eq!(const_u64(&out.func.view(), args[6] as usize), Some(1));
        assert!(
            (0..out.func.view().len()).filter(|&i| out.func.view().op(i) == Op::Umin).count() >= 2,
            "runtime alias guard must check C vs A and C vs B"
        );

        // 2. NOALIAS C skips runtime alias checks.
        let noalias_fn = build_ikj_byte_gemm(Op::Sext, Op::Sext, true);
        let noalias_out = optimize_gemm_loops(noalias_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(noalias_out.func.view()).unwrap();
        assert_eq!(noalias_out.stats.sme2_lowered, 1);
        assert_eq!((0..noalias_out.func.view().len()).filter(|&i| noalias_out.func.view().op(i) == Op::Umin).count(), 0);

        // 3. Rectangular i-j-k dot-product overwrite (`flags = 0`).
        let ijk_fn = build_ijk_dot_byte_gemm(Op::Sext);
        let ijk_out = optimize_gemm_loops(ijk_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(ijk_out.func.view()).unwrap();
        assert_eq!(ijk_out.stats.sme2_lowered, 1);
        let ijk_args = find_intrinsic_call(ijk_out.func.view(), SME2_GEMM_I8_I32_INTRINSIC)
            .expect("SME2 i8->i32 intrinsic emitted for i-j-k");
        assert_eq!(const_u64(&ijk_out.func.view(), ijk_args[6] as usize), Some(0));

        // 4. Without SME2 (`GemmOpts::neon()`), widening i8->i32 stays untouched; mixed Sext/Zext is rejected.
        let neon_only = optimize_gemm_loops(ikj_fn.view(), GemmOpts::neon(), TestHeap).unwrap();
        assert_eq!(neon_only.stats.gemms_detected, 0);
        let mixed_fn = build_ikj_byte_gemm(Op::Sext, Op::Zext, true);
        let mixed_out = optimize_gemm_loops(mixed_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        assert_eq!(mixed_out.stats.gemms_detected, 0);
    }

    #[test]
    fn unsigned_u8_gemm_lowers_to_sme2_umopa() {
        let ikj_fn = build_ikj_byte_gemm(Op::Zext, Op::Zext, true);
        let out = optimize_gemm_loops(ikj_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.gemms_detected, 1);
        assert_eq!(out.stats.sme2_lowered, 1);
        let args = find_intrinsic_call(out.func.view(), SME2_GEMM_U8_U32_INTRINSIC)
            .expect("SME2 u8->u32 intrinsic emitted");
        assert_eq!(args.len(), 7);
        assert_eq!(const_u64(&out.func.view(), args[6] as usize), Some(1));

        let ijk_fn = build_ijk_dot_byte_gemm(Op::Zext);
        let ijk_out = optimize_gemm_loops(ijk_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(ijk_out.func.view()).unwrap();
        assert_eq!(ijk_out.stats.sme2_lowered, 1);
        let ijk_args = find_intrinsic_call(ijk_out.func.view(), SME2_GEMM_U8_U32_INTRINSIC)
            .expect("SME2 u8->u32 intrinsic emitted for i-j-k");
        assert_eq!(const_u64(&ijk_out.func.view(), ijk_args[6] as usize), Some(0));
    }

    #[test]
    fn sibling_zero_init_loop_lowers_to_overwrite_gemm_f32_and_i8() {
        // 1. Idiomatic f32: `for i { for j { c[i,j] = 0.0 }; for k { for j { c[i,j] += a*b } } }`
        let f32_fn = build_sibling_zero_init_ikj_gemm(false, false);
        let f32_sme2 = optimize_gemm_loops(f32_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(f32_sme2.func.view()).unwrap();
        assert_eq!(f32_sme2.stats.gemms_detected, 1);
        assert_eq!(f32_sme2.stats.sme2_lowered, 1);
        assert_eq!(f32_sme2.stats.neon_blocked, 1);
        let f32_args = find_intrinsic_call(f32_sme2.func.view(), SME2_GEMM_F32_INTRINSIC)
            .expect("SME2 f32 intrinsic emitted for sibling zero-init loop");
        assert_eq!(
            const_u64(&f32_sme2.func.view(), f32_args[6] as usize),
            Some(0),
            "sibling zero-init loop must emit overwrite mode (flags = 0)"
        );

        // Verify NEON 4x16 path also uses overwrite mode (zero-splat instead of loading C).
        let f32_neon = optimize_gemm_loops(f32_fn.view(), GemmOpts::neon(), TestHeap).unwrap();
        verify(f32_neon.func.view()).unwrap();
        let v4f32 = Ty::vector(crate::Scalar::F32, 4).unwrap();
        assert!(max_vec_fmas_in_block(f32_neon.func.view(), v4f32) >= 16);

        // 2. Idiomatic i8->i32: `for i { for j { c[i,j] = 0 }; for k { for j { c[i,j] += (a as i32)*(b as i32) } } }`
        let i8_fn = build_sibling_zero_init_ikj_gemm(true, false);
        let i8_sme2 = optimize_gemm_loops(i8_fn.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(i8_sme2.func.view()).unwrap();
        assert_eq!(i8_sme2.stats.gemms_detected, 1);
        assert_eq!(i8_sme2.stats.sme2_lowered, 1);
        let i8_args = find_intrinsic_call(i8_sme2.func.view(), SME2_GEMM_I8_I32_INTRINSIC)
            .expect("SME2 i8->i32 intrinsic emitted for sibling zero-init loop");
        assert_eq!(
            const_u64(&i8_sme2.func.view(), i8_args[6] as usize),
            Some(0),
            "sibling zero-init i8->i32 loop must emit overwrite mode (flags = 0)"
        );

        // 3. Negative control: zero-init loop AFTER the k-j loop must NOT be recognized as overwrite GEMM!
        let bad_order = build_sibling_zero_init_ikj_gemm(false, true);
        let bad_out = optimize_gemm_loops(bad_order.view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(bad_out.func.view()).unwrap();
        assert_eq!(bad_out.stats.gemms_detected, 0);
    }

    #[test]
    fn collapsed_br_latches_sibling_zero_init_lowers_to_sme2_and_strict_neon() {
        // Exact CFG shape emitted after `Range::next` + `sroa` + `thread_jumps`:
        // - `i_hdr` -> `z_hdr(0)` directly (non-empty `h_br.t_args`),
        // - `z_hdr` exits directly to `k_hdr(0)`,
        // - `k_hdr` exits directly back to `i_hdr(i+1)` (`l0.latch == k_hdr` with `Op::Br`!),
        // - `j_hdr` exits directly back to `k_hdr(k+1)` (`l1.latch == j_hdr` with `Op::Br`!).
        let build_collapsed = |mode: u8| -> Func<TestHeap> {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[],
            );
            b.set_signature(sig);
            let [entry, i_hdr, z_hdr, z_body, k_hdr, k_body, j_hdr, j_body, exit] =
                [(); 9].map(|_| b.create_block(BlockKind::Normal));
            let is_i8 = mode == 0;
            let use_fma = mode == 2;
            let acc_ty = if is_i8 { Ty::I32 } else { Ty::F32 };

            b.begin(entry);
            let pc = b.param(Ty::PTR);
            let pa = b.param(Ty::PTR);
            let pb = b.param(Ty::PTR);
            let n = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            let two = b.iconst(Ty::I64, 2);
            let zero_elem = if is_i8 { b.iconst(Ty::I32, 0) } else { b.fconst(Ty::F32, 0) };
            b.jump((i_hdr, &[zero]));

            b.begin(i_hdr);
            let i = b.param(Ty::I64);
            let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
            let i_next = b.binary(Op::Add, Flags::NONE, i, one);
            let keep_i = b.icmp(IntCond::Ult, i, n);
            b.br(Flags::NONE, keep_i, (z_hdr, &[zero]), (exit, &[]));

            b.begin(z_hdr);
            let jz = b.param(Ty::I64);
            let keep_jz = b.icmp(IntCond::Ult, jz, n);
            b.br(Flags::NONE, keep_jz, (z_body, &[]), (k_hdr, &[zero]));

            b.begin(z_body);
            let z_idx = b.binary(Op::Add, Flags::NONE, i_n, jz);
            let z_off = b.binary(Op::Shl, Flags::NONE, z_idx, two);
            let z_ptr = b.ptr_add(Flags::NONE, pc, z_off);
            b.store(Flags::NONE, zero_elem, z_ptr, 0, MemAttrs::aligned(2));
            let jz2 = b.binary(Op::Add, Flags::NONE, jz, one);
            b.jump((z_hdr, &[jz2]));

            b.begin(k_hdr);
            let k = b.param(Ty::I64);
            let k_next = b.binary(Op::Add, Flags::NONE, k, one);
            let keep_k = b.icmp(IntCond::Ult, k, n);
            b.br(Flags::NONE, keep_k, (k_body, &[]), (i_hdr, &[i_next]));

            b.begin(k_body);
            let a_idx = b.binary(Op::Add, Flags::NONE, i_n, k);
            let a_val = if is_i8 {
                let a_ptr = b.ptr_add(Flags::NONE, pa, a_idx);
                let a_raw = b.load(Ty::I8, Flags::NONE, a_ptr, 0, MemAttrs::aligned(0));
                b.convert(Op::Sext, Flags::NONE, Ty::I32, a_raw)
            } else {
                let a_off = b.binary(Op::Shl, Flags::NONE, a_idx, two);
                let a_ptr = b.ptr_add(Flags::NONE, pa, a_off);
                b.load(Ty::F32, Flags::NONE, a_ptr, 0, MemAttrs::aligned(2))
            };
            let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
            b.jump((j_hdr, &[zero]));

            b.begin(j_hdr);
            let j = b.param(Ty::I64);
            let keep_j = b.icmp(IntCond::Ult, j, n);
            b.br(Flags::NONE, keep_j, (j_body, &[]), (k_hdr, &[k_next]));

            b.begin(j_body);
            let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
            let b_val = if is_i8 {
                let b_ptr = b.ptr_add(Flags::NONE, pb, b_idx);
                let b_raw = b.load(Ty::I8, Flags::NONE, b_ptr, 0, MemAttrs::aligned(0));
                b.convert(Op::Sext, Flags::NONE, Ty::I32, b_raw)
            } else {
                let b_off = b.binary(Op::Shl, Flags::NONE, b_idx, two);
                let b_ptr = b.ptr_add(Flags::NONE, pb, b_off);
                b.load(Ty::F32, Flags::NONE, b_ptr, 0, MemAttrs::aligned(2))
            };
            let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
            let c_off = b.binary(Op::Shl, Flags::NONE, c_idx, two);
            let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
            let c_val = b.load(acc_ty, Flags::NONE, c_ptr, 0, MemAttrs::aligned(2));
            let c_new = if is_i8 {
                let prod = b.binary(Op::Mul, Flags::NONE, a_val, b_val);
                b.binary(Op::Add, Flags::NONE, c_val, prod)
            } else if use_fma {
                b.ternary(Op::Fma, Flags::NONE, a_val, b_val, c_val)
            } else {
                let prod = b.binary(Op::Fmul, Flags::NONE, a_val, b_val);
                b.binary(Op::Fadd, Flags::NONE, c_val, prod)
            };
            b.store(Flags::NONE, c_new, c_ptr, 0, MemAttrs::aligned(2));
            let j2 = b.binary(Op::Add, Flags::NONE, j, one);
            b.jump((j_hdr, &[j2]));

            b.begin(exit);
            b.ret(&[]);
            b.finish_checked().unwrap()
        };

        // 1. i8 -> i32 lowers directly to SME2 SMOPA overwrite (`flags = 0`).
        let i8_out = optimize_gemm_loops(build_collapsed(0).view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(i8_out.func.view()).unwrap();
        assert_eq!(i8_out.stats.sme2_lowered, 1);
        let i8_args = find_intrinsic_call(i8_out.func.view(), SME2_GEMM_I8_I32_INTRINSIC).unwrap();
        assert_eq!(const_u64(&i8_out.func.view(), i8_args[6] as usize), Some(0));

        // 2. Strict IEEE f32 (`+= a * b`) lowers to 4x16 NEON without FMA contraction or SME2.
        let strict_out = optimize_gemm_loops(build_collapsed(1).view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(strict_out.func.view()).unwrap();
        assert_eq!(strict_out.stats.sme2_lowered, 0);
        assert_eq!(strict_out.stats.neon_blocked, 1);
        assert!(find_intrinsic_call(strict_out.func.view(), SME2_GEMM_F32_INTRINSIC).is_none());

        // 3. FMA f32 (`mul_add`) lowers directly to SME2 FMOPA overwrite (`flags = 0`).
        let fma_out = optimize_gemm_loops(build_collapsed(2).view(), GemmOpts::sme2(), TestHeap).unwrap();
        verify(fma_out.func.view()).unwrap();
        assert_eq!(fma_out.stats.sme2_lowered, 1);
        let fma_args = find_intrinsic_call(fma_out.func.view(), SME2_GEMM_F32_INTRINSIC).unwrap();
        assert_eq!(const_u64(&fma_out.func.view(), fma_args[6] as usize), Some(0));
    }
}
